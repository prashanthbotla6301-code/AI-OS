//! Phase 2A integration suite: proves AgentOS can install and execute a
//! native agent as an OS-level package, end to end, over the real services.
//!
//! Boots `agentos-core --dev` (which supervises permission-manager, agent-bus,
//! agent-runtime, ai-orchestrator, model-router and agent-package-manager as
//! real processes on real Unix sockets) and then drives the full lifecycle:
//!
//!   create → validate → install → discover (desktop path) → permission
//!   denial → grant → start → execute → "Hello from AgentOS" → bus delivery
//!   → stop → uninstall → verify removal → reinstall.
//!
//! Security checks are first-class: malformed packages must be rejected,
//! permissions denied by default, and a denied permission must prevent the
//! corresponding operation.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const AGENT_ID: &str = "com.agentos.helloagent";
const EXPECTED: &str = "Hello from AgentOS";

// ---------------------------------------------------------------------------
// Dev-core harness (same shape as integration.rs; self-contained here)
// ---------------------------------------------------------------------------

fn bin_dir() -> PathBuf {
    if let Ok(v) = std::env::var("AGENTOS_BIN_DIR") {
        return PathBuf::from(v);
    }
    let mut p = std::env::current_exe().expect("current test executable");
    p.pop();
    p.pop();
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
        let _ = std::fs::remove_dir_all(&self.run_dir.parent().unwrap());
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tests is below repository root")
        .to_path_buf()
}

fn spawn_dev_core() -> DevCore {
    let tag = format!("p2a-{}-{}", std::process::id(), agentos_common::new_id());
    let base = std::env::temp_dir().join(&tag);
    let run_dir = base.join("run");
    let state_dir = base.join("state");
    let etc_dir = base.join("etc");
    let log_dir = base.join("logs");
    for d in [&run_dir, &state_dir, &etc_dir, &log_dir] {
        std::fs::create_dir_all(d).unwrap();
    }

    let bins = bin_dir();
    let cfg = serde_json::json!({
        "version": 1,
        "hostname": "agentos-dev-p2a",
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
    let root = repo_root();
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

fn wait_socket(core: &DevCore, name: &str, timeout: Duration) -> bool {
    let path = core.run_dir.join(name);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && agentos_ipc::call(&path, "ping", serde_json::json!({})).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn call(core: &DevCore, sock: &str, method: &str, params: Value) -> Result<Value, String> {
    agentos_ipc::call(&core.run_dir.join(sock), method, params).map_err(|e| e.message)
}

fn stage(name: &str, ok: bool, detail: &str) {
    println!(
        "[P2A] {} {} — {}",
        if ok { "PASS" } else { "FAIL" },
        name,
        detail
    );
    assert!(ok, "stage {name} failed: {detail}");
}

fn write_package(dir: &Path, manifest: &str, agent: &str, perms: &str) -> PathBuf {
    let p = dir.join("broken.agent");
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(p.join("manifest.json"), manifest).unwrap();
    std::fs::write(p.join("agent.json"), agent).unwrap();
    std::fs::write(p.join("permissions.json"), perms).unwrap();
    p
}

// ---------------------------------------------------------------------------
// The Phase 2A end-to-end run
// ---------------------------------------------------------------------------

#[test]
fn phase2a_native_agent_end_to_end() {
    if !bin("agentos-core").exists() {
        eprintln!("SKIP: agent binaries not built; run `cargo build --workspace --bins` first");
        return;
    }
    let core = spawn_dev_core();

    for sock in [
        "core.sock",
        "permissions.sock",
        "bus.sock",
        "runtime.sock",
        "models.sock",
        "apm.sock",
    ] {
        assert!(
            wait_socket(&core, sock, Duration::from_secs(20)),
            "service {sock} did not come up"
        );
    }
    let pkg_path = repo_root().join("packages/examples/hello-agent.agent");

    // -- 1. VALIDATE ---------------------------------------------------------
    let rep = call(
        &core,
        "apm.sock",
        "apm.validate",
        serde_json::json!({"path": pkg_path.display().to_string()}),
    )
    .unwrap();
    stage(
        "VALIDATE",
        rep["errors"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        &format!("{rep}"),
    );

    // -- 2. INSTALL ----------------------------------------------------------
    let ins = call(
        &core,
        "apm.sock",
        "apm.install",
        serde_json::json!({"path": pkg_path.display().to_string()}),
    )
    .unwrap();
    stage(
        "INSTALL",
        ins["id"] == AGENT_ID && ins["version"] == "1.0.0",
        &format!("{ins}"),
    );
    let ver = call(
        &core,
        "apm.sock",
        "apm.verify",
        serde_json::json!({"id": AGENT_ID}),
    )
    .unwrap();
    stage(
        "INSTALL-INTEGRITY",
        ver["integrity"] == "ok",
        &format!("{ver}"),
    );

    // -- 3. DESKTOP DISCOVERY (the exact RPC the desktop Agents panel uses) --
    call(
        &core,
        "runtime.sock",
        "agents.rescan",
        serde_json::json!({}),
    )
    .unwrap();
    let list = call(&core, "runtime.sock", "agents.list", serde_json::json!({})).unwrap();
    let row = list["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == AGENT_ID)
        .unwrap_or(&serde_json::Value::Null)
        .clone();
    stage(
        "DESKTOP-DISCOVERY",
        row["name"] == "HelloAgent"
            && row["engine"] == "native"
            && row["state"] == "Installed"
            && row["output"].is_null(),
        &format!("{row}"),
    );

    // -- 4. PERMISSION CHECK: denied by default => start must be blocked -----
    let denied = call(
        &core,
        "runtime.sock",
        "agents.start",
        serde_json::json!({"id": AGENT_ID}),
    )
    .unwrap();
    stage(
        "PERMISSION-CHECK-DENIED",
        denied["agent"]["state"] == "Blocked"
            && denied["error"].as_str().unwrap_or("").contains("denied"),
        &format!("{denied}"),
    );
    let no_result = call(
        &core,
        "runtime.sock",
        "agents.result",
        serde_json::json!({"id": AGENT_ID}),
    );
    stage(
        "PERMISSION-CHECK-NO-EXECUTION",
        no_result.is_err(),
        "agents.result must refuse: a blocked agent never executed",
    );

    // -- 5. GRANT the declared permission ------------------------------------
    call(
        &core,
        "permissions.sock",
        "grants.grant",
        serde_json::json!({
            "subject": AGENT_ID,
            "permission": "agent.invoke",
            "access": "write",
            "note": "phase2a e2e"
        }),
    )
    .unwrap();

    // -- 6. START ------------------------------------------------------------
    let started = call(
        &core,
        "runtime.sock",
        "agents.start",
        serde_json::json!({"id": AGENT_ID}),
    )
    .unwrap();
    stage(
        "START",
        started["state"] == "Running"
            && started["sandbox"]["network"] == false
            && started["sandbox"]["exec"] == false,
        &format!("{started}"),
    );

    // -- 7. EXECUTE + RESULT -------------------------------------------------
    let out = call(
        &core,
        "runtime.sock",
        "agents.result",
        serde_json::json!({"id": AGENT_ID}),
    )
    .unwrap();
    stage(
        "RUNTIME-EXECUTION",
        out["text"] == EXPECTED
            && out["ops_run"] == 1
            && out["engine"] == "native"
            && out["truncated"] == false,
        &format!("{out}"),
    );

    // -- 8. AGENT BUS: result + lifecycle envelopes are on the bus -----------
    let recent = call(
        &core,
        "bus.sock",
        "bus.recent",
        serde_json::json!({"limit": 64}),
    )
    .unwrap();
    let envelopes = recent["envelopes"].as_array().unwrap();
    let has_result = envelopes.iter().any(|e| {
        e["kind"] == "agent.result"
            && e["payload"]["agent"] == AGENT_ID
            && e["payload"]["text"] == EXPECTED
    });
    let has_lifecycle = envelopes.iter().any(|e| {
        e["kind"] == "agent.lifecycle"
            && e["payload"]["agent"] == AGENT_ID
            && e["payload"]["state"] == "running"
    });
    stage(
        "AGENT-BUS",
        has_result && has_lifecycle,
        &format!("result={has_result} lifecycle={has_lifecycle}"),
    );

    // -- 9. AGENT BUS: live subscription receives the next event -------------
    // Subscribe on a raw connection (push mode), then stop the agent and
    // expect the delivery frame.
    let mut sub = UnixStream::connect(core.run_dir.join("bus.sock")).unwrap();
    sub.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = serde_json::json!({"id":1,"method":"bus.subscribe","params":{"filters":["agent.lifecycle"]}});
    sub.write_all(format!("{req}\n").as_bytes()).unwrap();
    let mut reader = BufReader::new(sub.try_clone().unwrap());
    let mut ack = String::new();
    reader.read_line(&mut ack).unwrap();
    let ack_ok = ack.contains("\"subscribed\":true");
    call(
        &core,
        "runtime.sock",
        "agents.stop",
        serde_json::json!({"id": AGENT_ID}),
    )
    .unwrap();
    // The stop transition emits "stopping" then "stopped"; consume frames
    // until the "stopped" delivery arrives (bounded).
    let mut delivered: Value = Value::Null;
    let mut last_frame = String::new();
    for _ in 0..8 {
        let mut frame = String::new();
        if reader.read_line(&mut frame).unwrap_or(0) == 0 {
            break;
        }
        last_frame = frame.trim().to_string();
        delivered = serde_json::from_str(&last_frame).unwrap_or(Value::Null);
        if delivered["params"]["payload"]["state"] == "stopped" {
            break;
        }
    }
    stage(
        "AGENT-BUS-SUBSCRIPTION",
        ack_ok
            && delivered["event"] == "bus.deliver"
            && delivered["params"]["kind"] == "agent.lifecycle"
            && delivered["params"]["payload"]["state"] == "stopped",
        &format!("ack_ok={ack_ok} last_frame={last_frame}"),
    );

    // -- 10. STOP ------------------------------------------------------------
    let stopped = call(
        &core,
        "runtime.sock",
        "agents.stop",
        serde_json::json!({"id": AGENT_ID}),
    );
    stage(
        "STOP",
        matches!(&stopped, Err(e) if e.contains("not running"))
            || stopped.map(|v| v["state"] == "Stopped").unwrap_or(false),
        "second stop must be refused (agent already stopped by subscription step)",
    );

    // -- 11. FAILED lifecycle: declared fail op => executor error ------------
    let fail_dir = core.run_dir.parent().unwrap().join("failpkg");
    let _ = std::fs::remove_dir_all(&fail_dir);
    let fail_pkg = write_package(
        &fail_dir,
        r#"{"format":"agentos","format_version":"1.0.0","id":"com.test.faildemo","name":"FailDemo","version":"1.0.0","runtime":{"engine":"native","entry":{"kind":"native.task","task":{"ops":[{"op":"fail","message":"deterministic boom"}]}}},"permissions":[],"capabilities":["demo"]}"#,
        r#"{"entry":{"kind":"native.task","task":{"ops":[{"op":"fail","message":"deterministic boom"}]}}}"#,
        r#"{"permissions":[]}"#,
    );
    let ins2 = call(
        &core,
        "apm.sock",
        "apm.install",
        serde_json::json!({"path": fail_pkg.display().to_string()}),
    )
    .unwrap();
    call(
        &core,
        "runtime.sock",
        "agents.rescan",
        serde_json::json!({}),
    )
    .unwrap();
    let failed = call(
        &core,
        "runtime.sock",
        "agents.start",
        serde_json::json!({"id":"com.test.faildemo"}),
    )
    .unwrap();
    stage(
        "FAILED-LIFECYCLE",
        failed["agent"]["state"] == "Failed"
            && failed["error"]
                .as_str()
                .unwrap_or("")
                .contains("deterministic boom"),
        &format!("installed={} {failed}", ins2["id"] == "com.test.faildemo"),
    );

    // -- 12. uninstall is refused while an agent is running ------------------
    // (restart helloagent first: Blocked->grant already present)
    let _ = call(
        &core,
        "runtime.sock",
        "agents.start",
        serde_json::json!({"id": AGENT_ID}),
    );
    let refuse = call(
        &core,
        "apm.sock",
        "apm.uninstall",
        serde_json::json!({"id": AGENT_ID}),
    );
    stage(
        "UNINSTALL-RUNNING-REFUSED",
        matches!(&refuse, Err(e) if e.to_lowercase().contains("running")),
        &format!("{refuse:?}"),
    );
    call(
        &core,
        "runtime.sock",
        "agents.stop",
        serde_json::json!({"id": AGENT_ID}),
    )
    .unwrap();

    // -- 13. UNINSTALL + VERIFY REMOVAL --------------------------------------
    let rm = call(
        &core,
        "apm.sock",
        "apm.uninstall",
        serde_json::json!({"id": AGENT_ID}),
    )
    .unwrap();
    call(
        &core,
        "runtime.sock",
        "agents.rescan",
        serde_json::json!({}),
    )
    .unwrap();
    let list2 = call(&core, "runtime.sock", "agents.list", serde_json::json!({})).unwrap();
    let still_there = list2["agents"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["id"] == AGENT_ID);
    let get_gone = call(
        &core,
        "runtime.sock",
        "agents.get",
        serde_json::json!({"id": AGENT_ID}),
    );
    let apm_list = call(&core, "apm.sock", "apm.list", serde_json::json!({})).unwrap();
    let installed_now: Vec<_> = apm_list["installed"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["id"] == AGENT_ID)
        .collect();
    stage(
        "UNINSTALL",
        rm["removed"] == 1
            && !still_there
            && get_gone.is_err()
            && installed_now.is_empty()
            && !core.state_dir.join("agents").join(AGENT_ID).exists(),
        &format!("{rm} still_listed={still_there}"),
    );

    // -- 14. REINSTALL -------------------------------------------------------
    let re = call(
        &core,
        "apm.sock",
        "apm.install",
        serde_json::json!({"path": pkg_path.display().to_string()}),
    )
    .unwrap();
    call(
        &core,
        "runtime.sock",
        "agents.rescan",
        serde_json::json!({}),
    )
    .unwrap();
    let re_start = call(
        &core,
        "runtime.sock",
        "agents.start",
        serde_json::json!({"id": AGENT_ID}),
    )
    .unwrap();
    let re_out = call(
        &core,
        "runtime.sock",
        "agents.result",
        serde_json::json!({"id": AGENT_ID}),
    )
    .unwrap();
    stage(
        "REINSTALL",
        re["id"] == AGENT_ID && re_start["state"] == "Running" && re_out["text"] == EXPECTED,
        &format!("{re} state={} text={}", re_start["state"], re_out["text"]),
    );
    // leave the system clean
    call(
        &core,
        "runtime.sock",
        "agents.stop",
        serde_json::json!({"id": AGENT_ID}),
    )
    .unwrap();
    call(
        &core,
        "apm.sock",
        "apm.uninstall",
        serde_json::json!({"id": AGENT_ID}),
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// Invalid package rejection (security): malformed input must never install
// ---------------------------------------------------------------------------

#[test]
fn phase2a_invalid_packages_are_rejected() {
    if !bin("agentos-core").exists() {
        eprintln!("SKIP: agent binaries not built");
        return;
    }
    let core = spawn_dev_core();
    assert!(wait_socket(&core, "apm.sock", Duration::from_secs(20)));

    let base = core.run_dir.parent().unwrap().join("badpkgs");
    let _ = std::fs::remove_dir_all(&base);

    let cases: Vec<(&str, PathBuf, &str)> = vec![
        (
            "bad-manifest",
            write_package(
                &base,
                r#"{"format":"agentos","format_version":"9.0","id":"BAD ID","name":"","version":"x"}"#,
                r#"{"entry":{"kind":"builtin:echo"}}"#,
                r#"{"permissions":[]}"#,
            ),
            "invalid package must be rejected",
        ),
        (
            "undeclared-permission",
            write_package(
                &base,
                r#"{"format":"agentos","format_version":"1.0.0","id":"com.evil.agent","name":"Evil","version":"1.0.0","runtime":{"engine":"native","entry":{"kind":"native.task","task":{"ops":[{"op":"say","message":"x"}]}}},"permissions":[{"permission":"network"}]}"#,
                r#"{"entry":{"kind":"native.task","task":{"ops":[{"op":"say","message":"x"}]}}}"#,
                r#"{"permissions":[]}"#,
            ),
            "manifest permission missing from permissions.json",
        ),
        (
            "malformed-native-task",
            write_package(
                &base,
                r#"{"format":"agentos","format_version":"1.0.0","id":"com.bad.native","name":"BadNative","version":"1.0.0","runtime":{"engine":"native","entry":{"kind":"native.task","task":{"ops":[{"op":"say","message":"x"}]}}},"permissions":[]}"#,
                r#"{"entry":{"kind":"native.task","task":{"ops":[{"op":"exec","cmd":"curl evil.example"}]}}}"#,
                r#"{"permissions":[]}"#,
            ),
            "native entry with an undeclared op must reject the package",
        ),
    ];

    for (name, path, why) in cases {
        let rep = call(
            &core,
            "apm.sock",
            "apm.validate",
            serde_json::json!({"path": path.display().to_string()}),
        )
        .unwrap();
        let valid = rep["errors"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false);
        stage(&format!("REJECT-{name}"), !valid, why);
        let ins = call(
            &core,
            "apm.sock",
            "apm.install",
            serde_json::json!({"path": path.display().to_string()}),
        );
        stage(
            &format!("REJECT-INSTALL-{name}"),
            ins.is_err(),
            "apm.install must fail for invalid packages",
        );
    }

    let installed = call(&core, "apm.sock", "apm.list", serde_json::json!({})).unwrap();
    stage(
        "REJECT-NOTHING-INSTALLED",
        installed["installed"].as_array().unwrap().is_empty(),
        "the agent store must remain empty after rejected installs",
    );

    // symlink escape is rejected by the package tree walk
    #[cfg(unix)]
    {
        let link_pkg = base.join("link.agent");
        std::fs::create_dir_all(link_pkg.join("assets")).unwrap();
        std::fs::write(
            link_pkg.join("manifest.json"),
            r#"{"format":"agentos","format_version":"1.0.0","id":"com.evil.link","name":"Link","version":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            link_pkg.join("agent.json"),
            r#"{"entry":{"kind":"builtin:echo"}}"#,
        )
        .unwrap();
        std::fs::write(link_pkg.join("permissions.json"), r#"{"permissions":[]}"#).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", link_pkg.join("assets/escape")).unwrap();
        let ins = call(
            &core,
            "apm.sock",
            "apm.install",
            serde_json::json!({"path": link_pkg.display().to_string()}),
        );
        stage(
            "REJECT-SYMLINK",
            ins.is_err(),
            "packages containing symbolic links must be rejected",
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}
