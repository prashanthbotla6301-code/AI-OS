//! AgentOS agent-runtime.
//!
//! Owns the *agent abstraction* for the whole OS:
//!
//! * [`AgentState`] — the lifecycle state machine every agent obeys,
//! * [`AgentRecord`] — metadata for a registered agent (from an installed
//!   `.agent` package),
//! * [`Executor`] — the execution interface. Phase 1 ships exactly one
//!   implementation, [`MockExecutor`], which performs state transitions and
//!   emits lifecycle events on the agent bus. **It never executes agent code.**
//!   Real sandboxed execution (process/wasm engines under seccomp/landlock +
//!   resource caps) is Phase 2; the interfaces below are its contract.
//! * [`SandboxProfile`] / [`ResourceBudget`] — the isolation + quota surface
//!   the Phase 2 executor will enforce; Phase 1 records and reports them.
//!
//! Before any agent reaches Running, every permission its manifest requests
//! is checked with permission-manager (deny-by-default). Denied => Blocked
//! with a human-readable reason. This is real wiring, not a mock.

use agentos_common::now_ms;
use agentos_ipc::{serve, Handler, RpcResponse};
use agentos_schema::SandboxSpec;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Lifecycle state machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum AgentState {
    /// Installed on disk, not yet validated for this boot.
    Installed,
    /// Loaded, manifest valid, permission precheck passed at least once.
    Ready,
    Starting,
    Running,
    Suspended,
    Stopping,
    Stopped,
    /// Permission precheck failed; includes a reason in the record.
    Blocked,
    Failed,
}

impl AgentState {
    pub fn legal_transitions(self) -> &'static [AgentState] {
        use AgentState::*;
        match self {
            Installed => &[Ready, Blocked, Failed],
            Ready => &[Starting, Blocked, Stopped],
            Starting => &[Running, Blocked, Failed],
            Running => &[Suspended, Stopping, Failed],
            Suspended => &[Running, Stopping, Failed],
            Stopping => &[Stopped, Failed],
            Stopped => &[Ready, Failed],
            Blocked => &[Ready, Stopped],
            Failed => &[Ready, Stopped],
        }
    }
    pub fn can_transition(self, to: AgentState) -> bool {
        self.legal_transitions().contains(&to)
    }
}

// ---------------------------------------------------------------------------
// Agent record + sandbox/resource interfaces
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResourceBudget {
    pub mem_limit_mb: u32,
    pub cpu_percent: u32,
    /// Placeholder for Phase 2 token budgets routed through model-router.
    pub ai_tokens_per_day: u64,
}

impl Default for ResourceBudget {
    fn default() -> Self {
        ResourceBudget {
            mem_limit_mb: 256,
            cpu_percent: 25,
            ai_tokens_per_day: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SandboxProfile {
    /// Filesystem roots visible to the agent. Empty = no filesystem access.
    pub fs_roots: Vec<String>,
    /// Phase 1: always false — network egress does not exist yet.
    pub network: bool,
    /// Phase 1: always false — arbitrary process execution does not exist yet.
    pub exec: bool,
    pub budget: ResourceBudget,
}

impl SandboxProfile {
    /// Build the Phase 1 profile from a package's sandbox spec:
    /// network/exec are forced off regardless of what the package asks for.
    pub fn phase1(spec: Option<&SandboxSpec>) -> SandboxProfile {
        let (fs, net, exec) = match spec {
            Some(s) => (s.fs.clone(), false, false), // deliberately ignore s.network / s.exec
            None => (vec![], false, false),
        };
        SandboxProfile {
            fs_roots: fs,
            network: net,
            exec: exec,
            budget: ResourceBudget::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: String,
    pub name: String,
    pub version: String,
    pub capabilities: Vec<String>,
    pub requested_permissions: Vec<agentos_schema::PermissionRequest>,
    pub engine: String,
    #[serde(default)]
    pub description: Option<String>,
    pub install_path: String,
    pub state: AgentState,
    pub state_since_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub sandbox: SandboxProfile,
}

// ---------------------------------------------------------------------------
// Executor interface (Phase 2 plugs the real sandboxed executor in here)
// ---------------------------------------------------------------------------

pub trait Executor: Send + Sync {
    fn name(&self) -> &'static str;
    /// Begin execution of a validated, permission-checked agent.
    fn start(&self, rec: &mut AgentRecord) -> Result<(), String>;
    /// Stop a running agent.
    fn stop(&self, rec: &mut AgentRecord) -> Result<(), String>;
}

/// Phase 1 executor: performs NO code execution. It transitions state, emits a
/// lifecycle event on the agent bus, and records that execution was simulated.
pub struct MockExecutor {
    pub bus_socket: PathBuf,
}

impl MockExecutor {
    fn emit(&self, rec: &AgentRecord, state: &str) {
        // Fire-and-forget; the bus being down must not break the runtime.
        let _ = agentos_ipc::call(
            &self.bus_socket,
            "bus.send",
            serde_json::json!({
                "from": format!("system:runtime"),
                "to": "topic:agent-lifecycle",
                "kind": "agent.lifecycle",
                "payload": { "agent": rec.id, "state": state, "ts": now_ms() }
            }),
        );
    }
}

impl Executor for MockExecutor {
    fn name(&self) -> &'static str {
        "mock"
    }
    fn start(&self, rec: &mut AgentRecord) -> Result<(), String> {
        if rec.engine != agentos_schema::engine::MOCK {
            return Err(format!(
                "engine {:?} cannot execute in Phase 1 (only \"mock\" runs, and it simulates work without executing agent code)",
                rec.engine
            ));
        }
        rec.detail =
            Some("mock executor: simulated run, no agent code is executed in Phase 1".into());
        self.emit(rec, "running");
        Ok(())
    }
    fn stop(&self, rec: &mut AgentRecord) -> Result<(), String> {
        self.emit(rec, "stopped");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct Registry {
    /// keyed by agent id
    agents: BTreeMap<String, AgentRecord>,
}

impl Registry {
    /// Scan the install root (<state>/agents/<id>/<version>/manifest.json).
    /// Highest installed version of each id wins (multi-version on disk).
    pub fn scan(agents_dir: &Path) -> Registry {
        let mut agents = BTreeMap::new();
        let ids = match fs::read_dir(agents_dir) {
            Ok(rd) => rd,
            Err(_) => return Registry { agents },
        };
        for id_entry in ids.flatten() {
            let id_dir = id_entry.path();
            if !id_dir.is_dir() {
                continue;
            }
            let mut best: Option<(String, AgentRecord)> = None;
            let versions = match fs::read_dir(&id_dir) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for v_entry in versions {
                let Ok(e) = v_entry else { continue };
                let mpath = e.path().join("manifest.json");
                if !mpath.is_file() {
                    continue;
                }
                let report = agentos_schema::validate_package_dir(&e.path());
                if !report.valid() {
                    agentos_common::warn(&format!(
                        "ignoring invalid installed agent {}: {:?}",
                        e.path().display(),
                        report.errors
                    ));
                    continue;
                }
                let Ok(m) = agentos_schema::load_manifest(&e.path()) else {
                    continue;
                };
                let spec: Option<AgentSpec> = fs::read_to_string(e.path().join("agent.json"))
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok());
                let rec = AgentRecord {
                    id: m.id.clone(),
                    name: m.name.clone(),
                    version: m.version.clone(),
                    capabilities: m.capabilities.clone(),
                    requested_permissions: m.permissions.clone(),
                    engine: m
                        .runtime
                        .engine
                        .clone()
                        .unwrap_or_else(|| agentos_schema::engine::MOCK.into()),
                    description: m.description.clone(),
                    install_path: e.path().display().to_string(),
                    state: AgentState::Installed,
                    state_since_ms: now_ms(),
                    detail: None,
                    sandbox: SandboxProfile::phase1(spec.as_ref().and_then(|s| s.sandbox.as_ref())),
                };
                let rec_version = semver::Version::parse(&rec.version).ok();
                let better = match &best {
                    None => true,
                    Some((v, _)) => {
                        let best_version = semver::Version::parse(v).ok();
                        match (rec_version, best_version) {
                            (Some(a), Some(b)) => a > b,
                            _ => rec.version > *v,
                        }
                    }
                };
                if better {
                    best = Some((rec.version.clone(), rec));
                }
            }
            if let Some((_, rec)) = best {
                agents.insert(id_entry.file_name().to_string_lossy().to_string(), rec);
            }
        }
        Registry { agents }
    }

    pub fn list(&self) -> Vec<AgentRecord> {
        self.agents.values().cloned().collect()
    }
    pub fn get(&self, id: &str) -> Option<&AgentRecord> {
        self.agents.get(id)
    }
    pub fn get_mut(&mut self, id: &str) -> Option<&mut AgentRecord> {
        self.agents.get_mut(id)
    }
    pub fn set_state(
        &mut self,
        id: &str,
        to: AgentState,
        detail: Option<String>,
    ) -> Result<(), String> {
        let rec = self
            .get_mut(id)
            .ok_or_else(|| format!("unknown agent {id:?}"))?;
        let from = rec.state;
        if !from.can_transition(to) {
            return Err(format!(
                "illegal transition {from:?} -> {to:?} for agent {id}"
            ));
        }
        rec.state = to;
        rec.state_since_ms = now_ms();
        rec.detail = detail;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentSpec {
    #[serde(default)]
    sandbox: Option<SandboxSpec>,
}

// ---------------------------------------------------------------------------
// Permission gate: no agent runs without satisfying the permission authority
// ---------------------------------------------------------------------------

pub struct PermissionGate {
    pub permissions_socket: PathBuf,
}

impl PermissionGate {
    /// Check every requested permission. Returns Ok(()) if all Allow.
    /// Deny/Ask => Err(reason list) (Ask is treated as blocked in Phase 1).
    pub fn precheck(&self, rec: &AgentRecord) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        for req in &rec.requested_permissions {
            let access = req.access.as_deref().unwrap_or("write");
            let res = agentos_ipc::call(
                &self.permissions_socket,
                "permissions.check",
                serde_json::json!({
                    "subject": rec.id,
                    "permission": req.permission,
                    "access": access,
                    "resource": req.resource,
                }),
            );
            match res {
                Ok(v) => {
                    let decision = v["decision"].as_str().unwrap_or("deny");
                    match decision {
                        "allow" => {}
                        "ask" => problems.push(format!(
                            "{}: requires user consent (consent flow arrives Phase 2; blocked now)",
                            req.permission
                        )),
                        _ => problems.push(format!(
                            "{}: denied — {}",
                            req.permission,
                            v["reason"].as_str().unwrap_or("no grant")
                        )),
                    }
                }
                Err(e) => problems.push(format!(
                    "{}: permission-manager unreachable ({})",
                    req.permission, e.message
                )),
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

struct Service {
    registry: Mutex<Registry>,
    executor: MockExecutor,
    gate: PermissionGate,
    agents_dir: PathBuf,
}

impl Service {
    fn rescan(&self) -> usize {
        let mut reg = self.registry.lock().unwrap();
        let fresh = Registry::scan(&self.agents_dir);
        let n = fresh.list().len();
        // preserve live state for agents that still exist
        for (id, rec) in fresh.agents {
            if let Some(existing) = reg.agents.get(&id) {
                if existing.version == rec.version && existing.state != AgentState::Installed {
                    let mut keep = rec;
                    keep.state = existing.state;
                    keep.detail = existing.detail.clone();
                    keep.state_since_ms = existing.state_since_ms;
                    reg.agents.insert(id, keep);
                    continue;
                }
            }
            reg.agents.insert(id, rec);
        }
        n
    }

    fn start_agent(&self, id: &str) -> Result<AgentRecord, String> {
        {
            let reg = self.registry.lock().unwrap();
            let rec = reg.get(id).ok_or_else(|| format!("unknown agent {id:?}"))?;
            // 1. permission gate (deny-by-default via permission-manager)
            if let Err(problems) = self.gate.precheck(rec) {
                let reason = problems.join("; ");
                drop(reg);
                self.registry
                    .lock()
                    .unwrap()
                    .set_state(id, AgentState::Blocked, Some(reason.clone()))
                    .ok();
                return Err(reason);
            }
        }
        // state: Ready path (first start transitions Installed->Ready->Starting->Running)
        {
            let mut reg = self.registry.lock().unwrap();
            match reg.get(id).map(|r| r.state) {
                Some(AgentState::Installed) => reg.set_state(id, AgentState::Ready, None)?,
                Some(AgentState::Stopped)
                | Some(AgentState::Failed)
                | Some(AgentState::Blocked) => reg.set_state(id, AgentState::Ready, None)?,
                Some(s) if s != AgentState::Ready => {
                    return Err(format!("agent is {s:?}, cannot start"))
                }
                _ => {}
            }
            reg.set_state(id, AgentState::Starting, None)?;
        }
        {
            let mut reg = self.registry.lock().unwrap();
            let rec = reg.get_mut(id).unwrap();
            if let Err(e) = self.executor.start(rec) {
                let msg = format!("executor: {e}");
                rec.state = AgentState::Failed;
                rec.detail = Some(msg.clone());
                return Err(msg);
            }
            rec.state = AgentState::Running;
            rec.state_since_ms = now_ms();
            Ok(rec.clone())
        }
    }

    fn stop_agent(&self, id: &str) -> Result<AgentRecord, String> {
        let mut reg = self.registry.lock().unwrap();
        match reg.get(id).map(|r| r.state) {
            Some(AgentState::Running) | Some(AgentState::Suspended) => {}
            Some(s) => return Err(format!("agent is {s:?}, not running")),
            None => return Err(format!("unknown agent {id:?}")),
        }
        reg.set_state(id, AgentState::Stopping, None)?;
        let rec = reg.get_mut(id).unwrap();
        let _ = self.executor.stop(rec);
        rec.state = AgentState::Stopped;
        rec.state_since_ms = now_ms();
        Ok(rec.clone())
    }
}

fn main() {
    let paths = agentos_common::Paths::detect();
    let mut socket = paths.socket(agentos_common::sockets::RUNTIME);
    let mut state_dir = paths.state_dir;
    let mut run_dir = paths.run_dir;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from).unwrap_or_default(),
            "--state-dir" => state_dir = args.next().map(PathBuf::from).unwrap_or_default(),
            "--run-dir" => run_dir = args.next().map(PathBuf::from).unwrap_or_default(),
            _ => {}
        }
    }
    agentos_common::logger_init("agent-runtime");
    agentos_common::info("agent-runtime starting");

    let svc = Arc::new(Service {
        registry: Mutex::new(Registry::scan(&state_dir.join("agents"))),
        executor: MockExecutor {
            bus_socket: run_dir.join(agentos_common::sockets::BUS),
        },
        gate: PermissionGate {
            permissions_socket: run_dir.join(agentos_common::sockets::PERMISSIONS),
        },
        agents_dir: state_dir.join("agents"),
    });
    let n = svc.registry.lock().unwrap().list().len();
    agentos_common::info(&format!("registry: {n} agent(s) installed"));

    let handler: Handler = {
        let svc = svc.clone();
        Arc::new(move |req| {
            let p = &req.params;
            match req.method.as_str() {
                "ping" => RpcResponse::ok(
                    req.id,
                    serde_json::json!({"service":"agent-runtime","version":agentos_common::VERSION,"executor":svc.executor.name()}),
                ),
                "agents.list" => {
                    let list = svc.registry.lock().unwrap().list();
                    RpcResponse::ok(req.id, serde_json::json!({ "agents": list }))
                }
                "agents.get" => {
                    let id = p["id"].as_str().unwrap_or_default();
                    match svc.registry.lock().unwrap().get(id) {
                        Some(r) => {
                            RpcResponse::ok(req.id, serde_json::to_value(r).unwrap_or_default())
                        }
                        None => RpcResponse::err(
                            req.id,
                            agentos_ipc::ERR_NOT_FOUND,
                            format!("unknown agent {id:?}"),
                        ),
                    }
                }
                "agents.start" => {
                    let id = p["id"].as_str().unwrap_or_default().to_string();
                    match svc.start_agent(&id) {
                        Ok(r) => {
                            RpcResponse::ok(req.id, serde_json::to_value(r).unwrap_or_default())
                        }
                        Err(e) => {
                            // If blocked, return the record state alongside the error detail.
                            let state = svc
                                .registry
                                .lock()
                                .unwrap()
                                .get(&id)
                                .map(|r| serde_json::to_value(r).unwrap_or_default());
                            let mut result = serde_json::json!({ "error": e });
                            if let Some(s) = state {
                                result["agent"] = s;
                            }
                            RpcResponse::ok(req.id, result)
                        }
                    }
                }
                "agents.stop" => {
                    let id = p["id"].as_str().unwrap_or_default().to_string();
                    match svc.stop_agent(&id) {
                        Ok(r) => {
                            RpcResponse::ok(req.id, serde_json::to_value(r).unwrap_or_default())
                        }
                        Err(e) => RpcResponse::err(req.id, agentos_ipc::ERR_INVALID_STATE, e),
                    }
                }
                "agents.rescan" => {
                    RpcResponse::ok(req.id, serde_json::json!({ "agents": svc.rescan() }))
                }
                _ => RpcResponse::err(
                    req.id,
                    agentos_ipc::ERR_UNKNOWN_METHOD,
                    format!("unknown method {}", req.method),
                ),
            }
        })
    };

    if let Err(e) = serve(&socket, handler) {
        agentos_common::error(&format!("agent-runtime fatal: {e}"));
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_schema::permission as perm;

    #[test]
    fn lifecycle_transitions() {
        use AgentState::*;
        assert!(Installed.can_transition(Ready));
        assert!(Ready.can_transition(Starting));
        assert!(Starting.can_transition(Running));
        assert!(Running.can_transition(Suspended));
        assert!(Suspended.can_transition(Running));
        assert!(Running.can_transition(Stopping));
        assert!(Stopping.can_transition(Stopped));
        assert!(Stopped.can_transition(Ready)); // restart
        assert!(Blocked.can_transition(Ready)); // unblocked after a grant
        assert!(Failed.can_transition(Ready)); // retry
        assert!(
            !Installed.can_transition(Running),
            "cannot leap straight to Running"
        );
        assert!(
            !Stopped.can_transition(Running),
            "restart must go through Ready"
        );
        assert!(!Blocked.can_transition(Running));
    }

    fn write_agent(dir: &Path, id: &str, version: &str, perms: &str) {
        let p = dir.join("agents").join(id).join(version);
        fs::create_dir_all(&p).unwrap();
        fs::write(
            p.join("manifest.json"),
            format!(r#"{{"format":"agentos","format_version":"1.0.0","id":"{id}","name":"T","version":"{version}","runtime":{{"engine":"mock"}},"permissions":{perms},"capabilities":["t"]}}"#),
        )
        .unwrap();
        fs::write(
            p.join("agent.json"),
            r#"{"entry":{"kind":"builtin:echo"},"sandbox":{"fs":[],"network":true,"exec":true}}"#,
        )
        .unwrap();
        fs::write(
            p.join("permissions.json"),
            format!(r#"{{"permissions":{perms}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn registry_scan_picks_best_version_and_forces_phase1_sandbox() {
        let dir = std::env::temp_dir().join(format!("agentos-rt-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        write_agent(&dir, "com.t.agent", "1.0.0", "[]");
        write_agent(&dir, "com.t.agent", "1.2.0", "[]");
        let reg = Registry::scan(&dir.join("agents"));
        assert_eq!(reg.list().len(), 1);
        assert_eq!(reg.get("com.t.agent").unwrap().version, "1.2.0");
        // sandbox spec requested network+exec but Phase 1 profile forces off
        let sandbox = &reg.get("com.t.agent").unwrap().sandbox;
        assert!(!sandbox.network);
        assert!(!sandbox.exec);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_state_enforces_legality() {
        let dir = std::env::temp_dir().join(format!("agentos-rt-state-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        write_agent(&dir, "com.t.x", "0.1.0", "[]");
        let mut reg = Registry::scan(&dir.join("agents"));
        assert!(reg.set_state("com.t.x", AgentState::Ready, None).is_ok());
        assert!(
            reg.set_state("com.t.x", AgentState::Running, None).is_err(),
            "Ready->Running is illegal; must pass Starting"
        );
        assert!(reg.set_state("com.t.x", AgentState::Starting, None).is_ok());
        assert!(reg
            .set_state("com.t.x", AgentState::Running, Some("mock run".into()))
            .is_ok());
        assert!(reg.set_state("nope", AgentState::Ready, None).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mock_executor_refuses_non_mock_engines() {
        let exec = MockExecutor {
            bus_socket: PathBuf::from("/nonexistent/bus.sock"),
        };
        let mut rec = AgentRecord {
            id: "com.t.process".into(),
            name: "P".into(),
            version: "1.0.0".into(),
            capabilities: vec![],
            requested_permissions: vec![],
            engine: "process".into(),
            description: None,
            install_path: "/tmp".into(),
            state: AgentState::Starting,
            state_since_ms: 0,
            detail: None,
            sandbox: SandboxProfile::phase1(None),
        };
        let err = exec.start(&mut rec).unwrap_err();
        assert!(err.contains("cannot execute in Phase 1"));
    }

    #[test]
    fn permission_gate_parses_check_results() {
        // No live permission-manager in unit tests; the gate surfaces the
        // unreachability as a blocking problem (fail closed).
        let gate = PermissionGate {
            permissions_socket: PathBuf::from("/nonexistent/permissions.sock"),
        };
        let rec = AgentRecord {
            id: "com.t.net".into(),
            name: "N".into(),
            version: "1.0.0".into(),
            capabilities: vec![],
            requested_permissions: vec![agentos_schema::PermissionRequest {
                permission: perm::NETWORK.into(),
                access: None,
                resource: None,
                reason: None,
            }],
            engine: "mock".into(),
            description: None,
            install_path: "/tmp".into(),
            state: AgentState::Ready,
            state_since_ms: 0,
            detail: None,
            sandbox: SandboxProfile::phase1(None),
        };
        assert!(
            gate.precheck(&rec).is_err(),
            "must fail closed when permission-manager is unreachable"
        );
    }
}
