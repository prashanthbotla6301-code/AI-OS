//! agentos-core entrypoint. Modes:
//!
//! * (default, in the OS image) **PID 1**: full system initialization,
//!   supervision, IPC, autotest, shutdown.
//! * `--dev --run-dir ... --state-dir ...`: host development supervisor used
//!   by integration tests; identical logic, no root-only side effects.

use agentos_common::Paths;
use agentos_core::*;
use agentos_ipc::{serve, Handler, RpcResponse};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let is_pid1 = std::process::id() == 1;
    let mut dev = argv.iter().any(|a| a == "--dev");
    let mut run_dir = PathBuf::new();
    let mut state_dir = PathBuf::new();
    let mut etc_dir = PathBuf::new();
    let mut log_dir = PathBuf::new();
    let mut autotest_flag = false;

    let mut args = argv.iter().skip(1).peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--dev" => dev = true,
            "--run-dir" => run_dir = args.next().map(PathBuf::from).unwrap_or_default(),
            "--state-dir" => state_dir = args.next().map(PathBuf::from).unwrap_or_default(),
            "--etc-dir" => etc_dir = args.next().map(PathBuf::from).unwrap_or_default(),
            "--log-dir" => log_dir = args.next().map(PathBuf::from).unwrap_or_default(),
            "--autotest" => autotest_flag = true,
            _ => {}
        }
    }

    if is_pid1 || !dev {
        // ----- real PID 1 boot -------------------------------------------
        agentos_common::logger_init("agentos-core");
        console_println(&format!(
            "[agentos-core] AgentOS v{} booting (PID 1)",
            agentos_common::VERSION
        ));
        agentos_common::info(&format!(
            "AgentOS {} — PID 1 starting",
            agentos_common::VERSION
        ));

        if let Err(e) = mount_all() {
            // If we are on a host without root, refuse to masquerade.
            console_println(&format!(
                "[agentos-core] FATAL: cannot mount core filesystems: {e}"
            ));
            agentos_common::error(&format!("mount_all failed: {e}"));
            std::process::exit(1);
        }
        // `/proc/cmdline` is only available after the core mounts procfs.
        let autotest = autotest_flag || read_cmdline_flag("agentos.autotest");

        let paths = Paths::detect();
        if let Err(e) = paths.ensure_runtime_dirs().and_then(|_| {
            prepare_runtime_dir(&paths, true)
                .and_then(|_| prepare_state_dirs(&paths, true))
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))
        }) {
            console_println(&format!(
                "[agentos-core] FATAL: cannot create runtime dirs: {e}"
            ));
            std::process::exit(1);
        }
        relax_device_permissions();

        let cfg = SystemConfig::load(&paths.etc_dir);
        let _ = set_hostname(&cfg.hostname);

        // boot info for the desktop / diagnostics
        let _ = agentos_common::atomic_write(
            &paths.run_dir.join("boot.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "boot_id": "pending",
                "version": agentos_common::VERSION,
                "kernel": kernel_version(),
                "autotest": autotest,
                "started_at_ms": agentos_common::now_ms(),
            }))
            .unwrap_or_default()
            .as_bytes(),
        );

        let sup = Arc::new(Supervisor::new(cfg, paths.clone(), false, autotest));

        // debug consoles: tty1 (framebuffer console) + serial
        sup.spawn_tty("/dev/tty1", "root");
        sup.spawn_tty("/dev/ttyS0", "root");

        sup.spawn_initial();
        sup.spawn_shell();

        install_signal_handlers();
        start_ipc(sup.clone());
        if autotest {
            let s2 = sup.clone();
            std::thread::spawn(move || autotest::run(s2));
        }
        supervise_forever(sup);
    } else {
        // ----- dev mode ---------------------------------------------------
        agentos_common::logger_init("agentos-core-dev");
        if run_dir.as_os_str().is_empty() || state_dir.as_os_str().is_empty() {
            eprintln!("usage: agentos-core --dev --run-dir DIR --state-dir DIR [--etc-dir DIR] [--log-dir DIR] [--autotest]");
            std::process::exit(2);
        }
        let etc = if etc_dir.as_os_str().is_empty() {
            state_dir.join("etc")
        } else {
            etc_dir
        };
        let log = if log_dir.as_os_str().is_empty() {
            run_dir.join("logs")
        } else {
            log_dir
        };
        let paths = Paths::new(&run_dir, &state_dir, &etc, &log);
        paths.ensure_runtime_dirs().expect("dev dirs");
        prepare_runtime_dir(&paths, false).expect("dev runtime dir");
        prepare_state_dirs(&paths, false).expect("dev state dirs");
        let autotest = autotest_flag;
        let cfg = SystemConfig::load(&paths.etc_dir);
        let sup = Arc::new(Supervisor::new(cfg, paths.clone(), true, autotest));
        sup.spawn_initial();
        if sup.cfg.shell.enabled && std::env::var("AGENTOS_DEV_SHELL").is_ok() {
            sup.spawn_shell();
        }
        install_signal_handlers();
        start_ipc(sup.clone());
        if autotest {
            let s2 = sup.clone();
            std::thread::spawn(move || autotest::run(s2));
        }
        supervise_forever(sup);
    }
}

fn start_ipc(sup: Arc<Supervisor>) {
    let socket = sup.paths.socket(agentos_common::sockets::CORE);
    let sock_wait = socket.clone();
    std::thread::spawn(move || {
        let sup2 = sup;
        let handler: Handler = Arc::new(move |req| {
            let p = &req.params;
            match req.method.as_str() {
                "ping" => RpcResponse::ok(
                    req.id,
                    serde_json::json!({"service":"agentos-core","version":agentos_common::VERSION}),
                ),
                "status" => RpcResponse::ok(
                    req.id,
                    serde_json::json!({
                        "name": agentos_common::NAME,
                        "version": agentos_common::VERSION,
                        "kernel": kernel_version(),
                        "hostname": hostname_now(),
                        "uptime_s": sup2.started.elapsed().as_secs(),
                        "boot_id": sup2.boot_id,
                        "autotest": sup2.autotest,
                        "dev_mode": sup2.dev,
                        "shell_pid": sup2.shell_pid(),
                    }),
                ),
                "services" => {
                    RpcResponse::ok(req.id, serde_json::json!({ "services": sup2.statuses() }))
                }
                "service.restart" => {
                    let name = p["name"].as_str().unwrap_or_default().to_string();
                    match sup2.restart_service(&name) {
                        Ok(()) => RpcResponse::ok(req.id, serde_json::json!({"restarted": name})),
                        Err(e) => RpcResponse::err(req.id, agentos_ipc::ERR_NOT_FOUND, e),
                    }
                }
                "service.stop" => {
                    let name = p["name"].as_str().unwrap_or_default().to_string();
                    match sup2.stop_service(&name) {
                        Ok(()) => RpcResponse::ok(req.id, serde_json::json!({"stopped": name})),
                        Err(e) => RpcResponse::err(req.id, agentos_ipc::ERR_NOT_FOUND, e),
                    }
                }
                "config.get" => {
                    let cfg = serde_json::to_value(&sup2.cfg).unwrap_or_default();
                    RpcResponse::ok(req.id, cfg)
                }
                "config.set" => {
                    // Whitelisted, typed settings only.
                    let key = p["key"].as_str().unwrap_or_default().to_string();
                    match (key.as_str(), p["value"].as_str()) {
                        ("hostname", Some(v)) if valid_hostname(v) => {
                            let _ = set_hostname(v);
                            RpcResponse::ok(req.id, serde_json::json!({"set": key}))
                        }
                        ("shell.enabled", Some("true" | "false")) => {
                            sup2.shell_enabled.store(
                                p["value"].as_str() == Some("true"),
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            RpcResponse::ok(req.id, serde_json::json!({"set": key}))
                        }
                        _ => RpcResponse::err(
                            req.id,
                            agentos_ipc::ERR_INVALID_PARAMS,
                            "unsupported key/value (supported: hostname, shell.enabled)",
                        ),
                    }
                }
                "sys.info" => RpcResponse::ok(req.id, sys_info()),
                "shutdown" | "poweroff" => {
                    let s3 = sup2.clone();
                    std::thread::spawn(move || s3.shutdown(false));
                    RpcResponse::ok(req.id, serde_json::json!({"shutting_down": true}))
                }
                "reboot" => {
                    let s3 = sup2.clone();
                    std::thread::spawn(move || s3.shutdown(true));
                    RpcResponse::ok(req.id, serde_json::json!({"rebooting": true}))
                }
                _ => RpcResponse::err(
                    req.id,
                    agentos_ipc::ERR_UNKNOWN_METHOD,
                    format!("unknown method {}", req.method),
                ),
            }
        });
        if let Err(e) = serve(&socket, handler) {
            agentos_common::error(&format!("core IPC fatal: {e}"));
            std::process::exit(1);
        }
    });
    // wait for the socket to exist before returning (callers race otherwise)
    let sock = sock_wait;
    for _ in 0..200 {
        if sock.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn valid_hostname(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 63
        && h.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !h.starts_with('-')
        && !h.ends_with('-')
}

fn hostname_now() -> String {
    let mut buf = [0u8; 256];
    let r = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if r == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).to_string()
    } else {
        String::new()
    }
}

fn sys_info() -> serde_json::Value {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let mut mem_total = 0u64;
    let mut mem_avail = 0u64;
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        for line in s.lines() {
            if let Some(v) = line.strip_prefix("MemTotal:") {
                mem_total = v
                    .trim()
                    .split(' ')
                    .next()
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(0);
            }
            if let Some(v) = line.strip_prefix("MemAvailable:") {
                mem_avail = v
                    .trim()
                    .split(' ')
                    .next()
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(0);
            }
        }
    }
    serde_json::json!({
        "cpu_count": cpus,
        "mem_total_kb": mem_total,
        "mem_available_kb": mem_avail,
    })
}

/// Supervision loop: wake on any child signal; reap; kick scheduled respawns.
fn supervise_forever(sup: Arc<Supervisor>) {
    let pipe_fd = SIGNAL_PIPE.0.load(std::sync::atomic::Ordering::SeqCst);
    loop {
        let mut pfd = libc::pollfd {
            fd: pipe_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd, 1, 250) };
        if r > 0 {
            // drain pipe
            let mut buf = [0u8; 64];
            unsafe {
                let _ = libc::read(pipe_fd, buf.as_mut_ptr() as *mut libc::c_void, 64);
            }
        }
        let sig = PENDING_SIGNAL.swap(0, std::sync::atomic::Ordering::SeqCst);
        match sig {
            libc::SIGCHLD => reap_zombies(&sup),
            libc::SIGTERM | libc::SIGUSR2 => sup.shutdown(false),
            libc::SIGINT | libc::SIGUSR1 => sup.shutdown(true),
            _ => {}
        }
        sup.tick();
    }
}

// ---------------------------------------------------------------------------
// Autotest: in-guest end-to-end verification, results on the serial console
// ---------------------------------------------------------------------------

mod autotest {
    use super::*;
    use agentos_common::Paths;
    use std::sync::Arc;

    struct Results {
        passed: usize,
        failed: usize,
    }

    fn check(res: &mut Results, name: &str, ok: bool, detail: String) {
        if ok {
            res.passed += 1;
            console_println(&format!("[AGENTOS-TEST] PASS {name} — {detail}"));
        } else {
            res.failed += 1;
            console_println(&format!("[AGENTOS-TEST] FAIL {name} — {detail}"));
        }
    }

    fn ping(sock: &str) -> Result<String, String> {
        let p = Paths::detect();
        match agentos_ipc::call(&p.socket(sock), "ping", serde_json::json!({})) {
            Ok(v) => Ok(serde_json::to_string(&v).unwrap_or_default()),
            Err(e) => Err(e.message),
        }
    }

    pub fn run(sup: Arc<Supervisor>) {
        // wait for all enabled services to answer ping (max 30s)
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        'wait: loop {
            let mut all_ok = true;
            for svc in &sup.cfg.services {
                if !svc.enabled {
                    continue;
                }
                if ping(&svc.socket).is_err() {
                    all_ok = false;
                    break;
                }
            }
            if all_ok || std::time::Instant::now() > deadline {
                break 'wait;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        // give the shell time to draw
        std::thread::sleep(Duration::from_secs(if sup.dev { 2 } else { 8 }));

        let p = Paths::detect();
        let mut res = Results {
            passed: 0,
            failed: 0,
        };
        console_println("[AGENTOS-TEST] begin");

        // 1..6: services answer ping
        for sock in [
            agentos_common::sockets::PERMISSIONS,
            agentos_common::sockets::BUS,
            agentos_common::sockets::RUNTIME,
            agentos_common::sockets::ORCHESTRATOR,
            agentos_common::sockets::MODELS,
            agentos_common::sockets::APM,
        ] {
            match ping(sock) {
                Ok(v) => check(&mut res, &format!("service.{sock}"), true, v),
                Err(e) => check(&mut res, &format!("service.{sock}"), false, e),
            }
        }

        // 7: core supervision state
        let statuses = sup.statuses();
        let running = statuses
            .iter()
            .filter(|s| matches!(s.state, SvcState::Running))
            .count();
        check(
            &mut res,
            "core.services-running",
            running == sup.cfg.services.iter().filter(|s| s.enabled).count(),
            format!("{running} running"),
        );

        // 8: shell process alive (+ heartbeat file when the shell wrote one)
        let shell_ok = sup.shell_pid().is_some() || sup.dev;
        let hb = p.run_dir.join("shell.json");
        let hb_fresh = std::fs::metadata(&hb)
            .map(|m| {
                m.modified()
                    .ok()
                    .map(|t| t.elapsed().ok().map(|e| e.as_secs() < 10).unwrap_or(false))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        check(
            &mut res,
            "desktop.shell",
            shell_ok || hb_fresh,
            format!("pid={:?} heartbeat={}", sup.shell_pid(), hb_fresh),
        );

        // 9: permissions deny-by-default
        let d = agentos_ipc::call(
            &p.socket(agentos_common::sockets::PERMISSIONS),
            "permissions.check",
            serde_json::json!({"subject":"com.agentos.netdemo","permission":"network","access":"write"}),
        );
        check(
            &mut res,
            "permissions.deny-by-default",
            matches!(&d, Ok(v) if v["decision"]=="deny"),
            format!("{}", d.map(|v| v.to_string()).unwrap_or_else(|e| e.message)),
        );

        // 10: install hello.agent
        let examples = [
            "/opt/agentos/packages/hello.agent",
            "packages/examples/hello.agent",
        ];
        let pkg = examples.iter().find(|e| std::path::Path::new(e).exists());
        match pkg {
            Some(path) => {
                let ins = agentos_ipc::call(
                    &p.socket(agentos_common::sockets::APM),
                    "apm.install",
                    serde_json::json!({"path": path}),
                );
                check(
                    &mut res,
                    "apm.install-hello",
                    ins.is_ok(),
                    ins.map(|v| v.to_string()).unwrap_or_else(|e| e.message),
                );
            }
            None => check(
                &mut res,
                "apm.install-hello",
                false,
                "example package not found".into(),
            ),
        }
        // Installation and runtime discovery are separate service boundaries;
        // refresh the runtime before asserting it sees the new package.
        let _ = agentos_ipc::call(
            &p.socket(agentos_common::sockets::RUNTIME),
            "agents.rescan",
            serde_json::json!({}),
        );

        // 11: runtime sees it and starts it (mock executor)
        let lst = agentos_ipc::call(
            &p.socket(agentos_common::sockets::RUNTIME),
            "agents.list",
            serde_json::json!({}),
        );
        let listed = matches!(&lst, Ok(v) if v["agents"].as_array().map(|a| a.iter().any(|x| x["id"]=="com.agentos.hello")).unwrap_or(false));
        check(
            &mut res,
            "runtime.registry",
            listed,
            lst.map(|v| v["agents"].to_string())
                .unwrap_or_else(|e| e.message),
        );
        let start = agentos_ipc::call(
            &p.socket(agentos_common::sockets::RUNTIME),
            "agents.start",
            serde_json::json!({"id":"com.agentos.hello"}),
        );
        check(
            &mut res,
            "runtime.start-hello(mock-executor)",
            matches!(&start, Ok(v) if v["state"]=="Running"),
            start.map(|v| v.to_string()).unwrap_or_else(|e| e.message),
        );

        // 12: goal -> plan pipeline
        let goal = agentos_ipc::call(
            &p.socket(agentos_common::sockets::ORCHESTRATOR),
            "goals.submit",
            serde_json::json!({"text":"organize my downloads folder"}),
        );
        let plan_ok = matches!(&goal, Ok(v) if v["plan"]["tasks"].as_array().map(|a| a.len() >= 3).unwrap_or(false));
        check(
            &mut res,
            "orchestrator.goal-plan",
            plan_ok,
            goal.map(|v| {
                v["plan"]["tasks"]
                    .as_array()
                    .map(|a| a.len().to_string())
                    .unwrap_or_default()
            })
            .unwrap_or_else(|e| e.message),
        );

        // 13: model router local mock
        let mr = agentos_ipc::call(
            &p.socket(agentos_common::sockets::MODELS),
            "models.route",
            serde_json::json!({"text":"hello agentos","capability":"text.echo"}),
        );
        check(
            &mut res,
            "model-router.local-mock",
            matches!(&mr, Ok(v) if v["completion"].as_str().unwrap_or("").starts_with("[echo-1]")),
            mr.map(|v| v["model"].to_string())
                .unwrap_or_else(|e| e.message),
        );

        // 14: bus system message + recent audit
        let send = agentos_ipc::call(
            &p.socket(agentos_common::sockets::BUS),
            "bus.send",
            serde_json::json!({"from":"system:core","to":"topic:events","kind":"sys.test","payload":{"autotest":true}}),
        );
        let rec = agentos_ipc::call(
            &p.socket(agentos_common::sockets::BUS),
            "bus.recent",
            serde_json::json!({"limit":5}),
        );
        let rec_ok = send.is_ok()
            && matches!(&rec, Ok(v) if v["envelopes"].as_array().map(|a| a.iter().any(|e| e["kind"]=="sys.test")).unwrap_or(false));
        check(
            &mut res,
            "agent-bus.envelope-routing",
            rec_ok,
            format!("send={send:?}"),
        );

        // 15: net-demo agent blocked by permissions (negative test)
        if let Some(path) = examples.iter().find(|e| std::path::Path::new(e).exists()) {
            let _ = agentos_ipc::call(
                &p.socket(agentos_common::sockets::APM),
                "apm.uninstall",
                serde_json::json!({"id":"com.agentos.netdemo"}),
            );
            let src = if path.ends_with("hello.agent") {
                path.replace("hello.agent", "net-demo.agent")
            } else {
                path.to_string()
            };
            if std::path::Path::new(&src).exists() {
                let _ = agentos_ipc::call(
                    &p.socket(agentos_common::sockets::APM),
                    "apm.install",
                    serde_json::json!({"path": src}),
                );
                let _ = agentos_ipc::call(
                    &p.socket(agentos_common::sockets::RUNTIME),
                    "agents.rescan",
                    serde_json::json!({}),
                );
                let blk = agentos_ipc::call(
                    &p.socket(agentos_common::sockets::RUNTIME),
                    "agents.start",
                    serde_json::json!({"id":"com.agentos.netdemo"}),
                );
                let blocked = matches!(&blk, Ok(v) if v["agent"]["state"]=="Blocked");
                check(
                    &mut res,
                    "security.network-agent-blocked",
                    blocked,
                    blk.map(|v| v["agent"]["detail"].to_string())
                        .unwrap_or_else(|e| e.message),
                );
            } else {
                check(
                    &mut res,
                    "security.network-agent-blocked",
                    false,
                    "net-demo package missing".into(),
                );
            }
        }

        console_println(&format!(
            "[AGENTOS-TEST] RESULT: {}/{} PASSED",
            res.passed,
            res.passed + res.failed
        ));
        let all = res.failed == 0;
        std::thread::sleep(Duration::from_millis(800));
        sup.shutdown(!all && !sup.dev); // full pass => poweroff; failures => reboot (distinct exit paths)
    }
}
