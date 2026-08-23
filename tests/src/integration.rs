//! Integration test: boots `agentos-core --dev` which spawns ALL real system
//! services (permission-manager, agent-bus, agent-runtime, ai-orchestrator,
//! model-router, agent-package-manager) as real processes on real Unix
//! sockets, then exercises the full pipeline:
//!
//! install → permission gate → lifecycle → goal/plan → model route → bus.
//!
//! Requires binaries to be built first (`make dev-test` does this; plain
//! `cargo test` skips gracefully if they are missing).

use serde_json::Value;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin_dir() -> PathBuf {
    // Cargo test binaries live in `<target>/<profile>/deps`; walk from the
    // executable instead of assuming the repository has a fixed target path.
    if let Ok(v) = std::env::var("AGENTOS_BIN_DIR") {
        return PathBuf::from(v);
    }
    let mut p = std::env::current_exe().expect("current test executable");
    p.pop(); // test executable
    p.pop(); // deps
    p
}

fn bin(name: &str) -> PathBuf {
    bin_dir().join(name)
}

struct DevCore {
    child: Child,
    run_dir: PathBuf,
    state_dir: PathBuf,
}

impl Drop for DevCore {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.run_dir);
    }
}

fn spawn_dev_core() -> DevCore {
    let tag = format!("it-{}-{}", std::process::id(), agentos_common::new_id());
    let base = std::env::temp_dir().join(&tag);
    let run_dir = base.join("run");
    let state_dir = base.join("state");
    let etc_dir = base.join("etc");
    let log_dir = base.join("logs");
    for d in [&run_dir, &state_dir, &etc_dir, &log_dir] {
        std::fs::create_dir_all(d).unwrap();
    }

    let root = repo_root();
    // generate a dev config pointing at the built binaries
    let bins = bin_dir();
    let cfg = serde_json::json!({
        "version": 1,
        "hostname": "agentos-dev",
        "planner": "mock",
        "services": [
            {"name": "permission-manager", "bin": bins.join("agentos-permissions").display().to_string(), "args": [], "user": "root", "socket": "permissions.sock", "enabled": true},
            {"name": "agent-bus", "bin": bins.join("agentos-bus").display().to_string(), "args": [], "user": "root", "socket": "bus.sock", "enabled": true},
            {"name": "agent-runtime", "bin": bins.join("agentos-runtime").display().to_string(), "args": [], "user": "root", "socket": "runtime.sock", "enabled": true},
            {"name": "ai-orchestrator", "bin": bins.join("agentos-orchestrator").display().to_string(), "args": [], "user": "root", "socket": "orchestrator.sock", "enabled": true},
            {"name": "model-router", "bin": bins.join("agentos-models").display().to_string(), "args": [], "user": "root", "socket": "models.sock", "enabled": true},
            {"name": "agent-package-manager", "bin": bins.join("agentos-apm").display().to_string(), "args": [], "user": "root", "socket": "apm.sock", "enabled": true}
        ],
        "shell": {"bin": "/nonexistent/agentos-shell", "user": "root", "enabled": false},
        "ttys": []
    });
    std::fs::write(
        etc_dir.join("agentos.json"),
        serde_json::to_string_pretty(&cfg).unwrap(),
    )
    .unwrap();
    // ship the real permission policy + models config
    std::fs::copy(
        root.join("rootfs/etc/agentos/permissions-default.json"),
        etc_dir.join("permissions-default.json"),
    )
    .unwrap();
    std::fs::copy(
        root.join("rootfs/etc/agentos/models.json"),
        etc_dir.join("models.json"),
    )
    .unwrap();

    let child = Command::new(bin("agentos-core"))
        .arg("--dev")
        .arg("--run-dir")
        .arg(&run_dir)
        .arg("--state-dir")
        .arg(&state_dir)
        .arg("--etc-dir")
        .arg(&etc_dir)
        .arg("--log-dir")
        .arg(&log_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agentos-core --dev");

    DevCore {
        child,
        run_dir,
        state_dir,
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests is below repository root")
        .to_path_buf()
}

fn wait_socket(core: &DevCore, name: &str, timeout: Duration) -> bool {
    let path = core.run_dir.join(name);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            if agentos_ipc::call(&path, "ping", serde_json::json!({})).is_ok() {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn call(core: &DevCore, sock: &str, method: &str, params: Value) -> Result<Value, String> {
    agentos_ipc::call(&core.run_dir.join(sock), method, params).map_err(|e| e.message)
}

#[test]
fn core_boots_services_and_full_pipeline_works() {
    if !bin("agentos-core").exists() {
        eprintln!("SKIP: agent binaries not built; run `cargo build --workspace --bins` (or make dev-test) first");
        return;
    }
    let core = spawn_dev_core();

    // 1. every service comes up and answers ping
    for sock in [
        "core.sock",
        "permissions.sock",
        "bus.sock",
        "runtime.sock",
        "orchestrator.sock",
        "models.sock",
        "apm.sock",
    ] {
        assert!(
            wait_socket(&core, sock, Duration::from_secs(20)),
            "service {sock} did not come up"
        );
    }

    // 2. core status reports all services running
    let services = call(&core, "core.sock", "services", serde_json::json!({})).unwrap();
    let running = services["services"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["state"] == "running")
        .count();
    assert_eq!(running, 6, "expected 6 running services: {services}");

    // 3. deny-by-default permission model
    let d = call(&core, "permissions.sock", "permissions.check", serde_json::json!({"subject":"com.agentos.netdemo","permission":"network","access":"write"})).unwrap();
    assert_eq!(d["decision"], "deny");

    // 4. install the hello agent package (real copy + registry)
    let pkg = repo_root().join("packages/examples/hello.agent");
    let ins = call(
        &core,
        "apm.sock",
        "apm.install",
        serde_json::json!({"path": pkg.display().to_string()}),
    )
    .unwrap();
    assert_eq!(ins["id"], "com.agentos.hello");

    // 5. runtime sees the installed agent
    let _ = call(
        &core,
        "runtime.sock",
        "agents.rescan",
        serde_json::json!({}),
    );
    let agents = call(&core, "runtime.sock", "agents.list", serde_json::json!({})).unwrap();
    assert!(agents["agents"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["id"] == "com.agentos.hello"));

    // 6. lifecycle: Installed -> Running through the mock executor
    let started = call(
        &core,
        "runtime.sock",
        "agents.start",
        serde_json::json!({"id":"com.agentos.hello"}),
    )
    .unwrap();
    assert_eq!(started["state"], "Running", "{started}");
    let stopped = call(
        &core,
        "runtime.sock",
        "agents.stop",
        serde_json::json!({"id":"com.agentos.hello"}),
    )
    .unwrap();
    assert_eq!(stopped["state"], "Stopped");

    // 7. security: net-demo agent is BLOCKED (network denied)
    let pkg2 = repo_root().join("packages/examples/net-demo.agent");
    let _ = call(
        &core,
        "apm.sock",
        "apm.install",
        serde_json::json!({"path": pkg2.display().to_string()}),
    );
    let _ = call(
        &core,
        "runtime.sock",
        "agents.rescan",
        serde_json::json!({}),
    );
    let blocked = call(
        &core,
        "runtime.sock",
        "agents.start",
        serde_json::json!({"id":"com.agentos.netdemo"}),
    )
    .unwrap();
    assert_eq!(blocked["agent"]["state"], "Blocked", "{blocked}");
    assert!(blocked["error"].as_str().unwrap_or("").contains("denied"));

    // 8. grant network, unblock, then revoke again (permission-manager real wiring)
    call(&core, "permissions.sock", "grants.grant", serde_json::json!({"subject":"com.agentos.netdemo","permission":"network","access":"write"})).unwrap();
    let started2 = call(
        &core,
        "runtime.sock",
        "agents.start",
        serde_json::json!({"id":"com.agentos.netdemo"}),
    )
    .unwrap();
    assert_eq!(
        started2["state"], "Running",
        "after grant the agent must run: {started2}"
    );
    call(
        &core,
        "permissions.sock",
        "grants.revoke",
        serde_json::json!({"subject":"com.agentos.netdemo","permission":"network"}),
    )
    .unwrap();

    // 9. goals: deterministic plan with a valid dependency graph
    let goal = call(
        &core,
        "orchestrator.sock",
        "goals.submit",
        serde_json::json!({"text":"organize my downloads"}),
    )
    .unwrap();
    assert!(goal["plan"]["tasks"].as_array().unwrap().len() >= 3);

    // 10. model routing: local echo-mock works, usage accounting
    let route = call(
        &core,
        "models.sock",
        "models.route",
        serde_json::json!({"text":"hello","capability":"text.echo"}),
    )
    .unwrap();
    assert!(route["completion"]
        .as_str()
        .unwrap()
        .starts_with("[echo-1]"));

    // 11. bus: system traffic + audit trail; agent senders denied
    let send = call(
        &core,
        "bus.sock",
        "bus.send",
        serde_json::json!({"from":"system:test","to":"topic:t","kind":"k","payload":{}}),
    )
    .unwrap();
    assert!(send["id"].is_string());
    let denied = call(
        &core,
        "bus.sock",
        "bus.send",
        serde_json::json!({"from":"com.evil.agent","to":"agent:com.other","kind":"x","payload":{}}),
    );
    assert!(denied.unwrap_err().contains("agent.invoke"));

    // 12. persistence: goals journal survives a service restart via core restart-service
    call(
        &core,
        "core.sock",
        "service.restart",
        serde_json::json!({"name":"ai-orchestrator"}),
    )
    .unwrap();
    assert!(wait_socket(
        &core,
        "orchestrator.sock",
        Duration::from_secs(20)
    ));
    let goals = call(
        &core,
        "orchestrator.sock",
        "goals.list",
        serde_json::json!({}),
    )
    .unwrap();
    assert!(
        !goals["goals"].as_array().unwrap().is_empty(),
        "goals must survive service restart"
    );

    // 13. supervision: killing a service brings it back
    let before = call(&core, "core.sock", "services", serde_json::json!({})).unwrap();
    let apm_pid = before["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "agent-package-manager")
        .unwrap()["pid"]
        .as_u64()
        .unwrap() as i32;
    unsafe {
        libc_kill(apm_pid);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut recovered = false;
    while Instant::now() < deadline {
        let now = call(&core, "core.sock", "services", serde_json::json!({})).unwrap();
        let pid = now["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "agent-package-manager")
            .unwrap()["pid"]
            .as_u64();
        if pid.map(|p| p as i32 != apm_pid).unwrap_or(false) {
            recovered = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(recovered, "core must restart a killed service");
}

// tiny libc dependency stand-in (avoid adding libc to test crate deps)
fn libc_kill(pid: i32) {
    let out = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .output();
    assert!(out.is_ok());
}
