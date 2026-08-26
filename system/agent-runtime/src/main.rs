//! AgentOS agent-runtime.
//!
//! Owns the *agent abstraction* for the whole OS:
//!
//! * [`AgentState`] — the lifecycle state machine every agent obeys,
//! * [`AgentRecord`] — metadata for a registered agent (from an installed
//!   `.agent` package),
//! * [`Executor`] — the execution interface. Phase 2A ships two
//!   implementations: [`MockExecutor`] (simulates a run without executing a
//!   payload, for `mock` packages) and [`NativeExecutor`], which executes a
//!   package's declarative `native.task` **inside this process, inside the
//!   sandbox model**: pure operations only, bounded ops/output/time, and a
//!   permission-manager re-check (fail closed) immediately before execution.
//!   No package code is ever loaded or run; there are no fs/network/exec
//!   primitives in the native op set. Process/Wasm engines fail closed until
//!   their sandboxed executors arrive.
//! * [`SandboxProfile`] / [`ResourceBudget`] — the isolation + quota surface
//!   the executor enforces; network/exec are forced off regardless of what a
//!   package requests.
//!
//! Before any agent reaches Running, every permission its manifest requests
//! is checked with permission-manager (deny-by-default). Denied => Blocked
//! with a human-readable reason, and the executor re-checks at execution
//! time. Every lifecycle transition is announced on the agent bus
//! (`topic:agent-lifecycle`), and a native run's result is announced on
//! `topic:agent-results`. This is real wiring, not a mock.

use agentos_common::now_ms;
use agentos_ipc::{serve, Handler, RpcResponse};
use agentos_schema::native;
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
    /// Result of the last successful execution (native executor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<TaskOutput>,
}

/// Deterministic output of one native task execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TaskOutput {
    /// The task's text output (ops joined with newline).
    pub text: String,
    /// How many ops actually ran.
    pub ops_run: usize,
    /// True if the output budget cut the run short.
    pub truncated: bool,
    /// Wall-clock execution time inside the runtime.
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
// Executor interface + implementations
// ---------------------------------------------------------------------------

pub trait Executor: Send + Sync {
    fn name(&self) -> &'static str;
    /// Begin execution of a validated, permission-checked agent.
    fn start(&self, rec: &mut AgentRecord) -> Result<(), String>;
    /// Stop a running agent.
    fn stop(&self, rec: &mut AgentRecord) -> Result<(), String>;
}

/// Mock executor (Phase 1 contract, kept unchanged): performs NO code
/// execution. It records that execution was simulated. Lifecycle events are
/// emitted uniformly by the Service, not by executors.
pub struct MockExecutor;

impl Executor for MockExecutor {
    fn name(&self) -> &'static str {
        "mock"
    }
    fn start(&self, rec: &mut AgentRecord) -> Result<(), String> {
        if rec.engine != agentos_schema::engine::MOCK {
            return Err(format!(
                "engine {:?} cannot execute here (only \"mock\" and \"native\" run, and mock only simulates work without executing agent code)",
                rec.engine
            ));
        }
        rec.detail = Some("mock executor: simulated run, no agent code is executed".into());
        Ok(())
    }
    fn stop(&self, rec: &mut AgentRecord) -> Result<(), String> {
        let _ = rec;
        Ok(())
    }
}

/// Native executor (Phase 2A): executes a package's declarative
/// `native.task` **inside the runtime**, under the sandbox model.
///
/// Security properties:
/// * the task is pure data — the op set has no filesystem, network, exec,
///   or model primitives, so nothing can escape the runtime,
/// * every declared permission is re-checked with permission-manager
///   immediately before execution; any denial — or an unreachable
///   authority — aborts the run (fail closed),
/// * ops/output budgets from `agentos_schema::native` are enforced here too
///   (a run can never loop unbounded or emit unbounded data),
/// * the result is announced on the agent bus from `system:runtime`; the
///   agent-owned `agent.invoke` permission was required to get this far.
pub struct NativeExecutor {
    pub bus_socket: PathBuf,
    pub permissions_socket: PathBuf,
}

#[derive(Deserialize)]
struct EntrySpec {
    #[serde(default)]
    entry: serde_json::Value,
}

impl NativeExecutor {
    /// Load + re-validate the native task from the installed package.
    /// Packages are re-checked at execution time, never trusted at install
    /// time alone.
    fn load_task(&self, rec: &AgentRecord) -> Result<native::Task, String> {
        let path = Path::new(&rec.install_path).join("agent.json");
        let s = fs::read_to_string(&path).map_err(|e| format!("cannot read agent.json: {e}"))?;
        let spec: EntrySpec =
            serde_json::from_str(&s).map_err(|e| format!("agent.json invalid: {e}"))?;
        let entry = native::parse_entry(&spec.entry)?;
        Ok(entry.task)
    }

    /// Defense in depth: re-check every declared permission with the
    /// permission authority immediately before executing. Deny, ask, or an
    /// unreachable authority all abort the run (fail closed).
    fn recheck_permissions(&self, rec: &AgentRecord) -> Result<(), String> {
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
                Ok(v) if v["decision"] == "allow" => {}
                Ok(v) => {
                    return Err(format!(
                        "permission {} not granted at execution time ({})",
                        req.permission,
                        v["reason"].as_str().unwrap_or("denied")
                    ))
                }
                Err(e) => {
                    return Err(format!(
                        "permission-manager unreachable ({}); refusing to execute, fail closed",
                        e.message
                    ))
                }
            }
        }
        Ok(())
    }

    /// Run the ops. Pure, bounded, deterministic.
    fn run_ops(&self, task: &native::Task) -> Result<TaskOutput, String> {
        let started = std::time::Instant::now();
        let mut out = String::new();
        let mut truncated = false;
        let mut ops_run = 0usize;
        for (i, op) in task.ops.iter().enumerate() {
            ops_run = i + 1;
            match op {
                native::Op::Say { message } => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    if out.len() + message.len() > native::MAX_OUTPUT_BYTES {
                        truncated = true;
                        break;
                    }
                    out.push_str(message);
                }
                native::Op::Fail { message } => {
                    return Err(format!("task failed at op {}: {message}", i + 1))
                }
            }
        }
        Ok(TaskOutput {
            text: out,
            ops_run,
            truncated,
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }

    /// Announce the result on the bus. The permission to do so was granted
    /// and re-checked; the bus itself being down must not fail a completed
    /// run (delivery is transport, authority is permission-manager).
    fn announce(&self, rec: &AgentRecord, out: &TaskOutput) {
        let _ = agentos_ipc::call(
            &self.bus_socket,
            "bus.send",
            serde_json::json!({
                "from": "system:runtime",
                "to": "topic:agent-results",
                "kind": "agent.result",
                "payload": {
                    "agent": rec.id,
                    "version": rec.version,
                    "text": out.text,
                    "ops_run": out.ops_run,
                    "duration_ms": out.duration_ms,
                    "ts": now_ms(),
                }
            }),
        );
    }
}

impl Executor for NativeExecutor {
    fn name(&self) -> &'static str {
        "native"
    }
    fn start(&self, rec: &mut AgentRecord) -> Result<(), String> {
        if rec.engine != agentos_schema::engine::NATIVE {
            return Err(format!(
                "native executor refuses engine {:?} (fail closed)",
                rec.engine
            ));
        }
        let task = self.load_task(rec)?;
        self.recheck_permissions(rec)?;
        let out = self.run_ops(&task)?;
        self.announce(rec, &out);
        rec.detail = Some(format!(
            "native executor: {} op(s) in {} ms -> {:?}{}",
            out.ops_run,
            out.duration_ms,
            out.text,
            if out.truncated {
                " (output truncated)"
            } else {
                ""
            }
        ));
        rec.output = Some(out);
        Ok(())
    }
    fn stop(&self, rec: &mut AgentRecord) -> Result<(), String> {
        let _ = rec;
        Ok(())
    }
}

/// Executor set: one per engine the runtime can run. Anything else fails
/// closed in `for_engine`.
pub struct Executors {
    pub mock: MockExecutor,
    pub native: NativeExecutor,
}

impl Executors {
    pub fn for_engine(&self, engine: &str) -> Option<&dyn Executor> {
        match engine {
            agentos_schema::engine::MOCK => Some(&self.mock),
            agentos_schema::engine::NATIVE => Some(&self.native),
            _ => None,
        }
    }
    pub fn names(&self) -> Vec<&'static str> {
        vec!["mock", "native"]
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
                    output: None,
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
    executors: Executors,
    gate: PermissionGate,
    agents_dir: PathBuf,
    bus_socket: PathBuf,
}

impl Service {
    /// Announce a lifecycle transition on the agent bus. Fire-and-forget: a
    /// down bus must never break the runtime's own state machine.
    fn emit_lifecycle(&self, id: &str, state: &str, detail: Option<String>) {
        let mut payload = serde_json::json!({ "agent": id, "state": state, "ts": now_ms() });
        if let Some(d) = detail {
            payload["detail"] = serde_json::json!(d);
        }
        let _ = agentos_ipc::call(
            &self.bus_socket,
            "bus.send",
            serde_json::json!({
                "from": "system:runtime",
                "to": "topic:agent-lifecycle",
                "kind": "agent.lifecycle",
                "payload": payload
            }),
        );
    }

    fn rescan(&self) -> usize {
        let mut reg = self.registry.lock().unwrap();
        let fresh = Registry::scan(&self.agents_dir);
        let n = fresh.list().len();
        // The store is the source of truth: the merged registry contains
        // exactly the agents on disk. Live state (Running/Stopped/... and the
        // last execution output) is preserved for agents that still exist at
        // the same version; agents removed by apm.uninstall disappear here.
        let mut merged = Registry {
            agents: BTreeMap::new(),
        };
        for (id, rec) in fresh.agents {
            if let Some(existing) = reg.agents.get(&id) {
                if existing.version == rec.version && existing.state != AgentState::Installed {
                    let mut keep = rec;
                    keep.state = existing.state;
                    keep.detail = existing.detail.clone();
                    keep.state_since_ms = existing.state_since_ms;
                    keep.output = existing.output.clone();
                    merged.agents.insert(id, keep);
                    continue;
                }
            }
            merged.agents.insert(id, rec);
        }
        reg.agents = merged.agents;
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
                self.emit_lifecycle(id, "blocked", Some(reason.clone()));
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
        self.emit_lifecycle(id, "starting", None);
        // 2. execute under the engine's executor, inside the runtime
        let result = {
            let mut reg = self.registry.lock().unwrap();
            let rec = reg.get_mut(id).unwrap();
            let engine = rec.engine.clone();
            match self.executors.for_engine(&engine) {
                Some(exec) => match exec.start(rec) {
                    Ok(()) => {
                        rec.state = AgentState::Running;
                        rec.state_since_ms = now_ms();
                        Ok(rec.clone())
                    }
                    Err(e) => {
                        let msg = format!("executor: {e}");
                        rec.state = AgentState::Failed;
                        rec.state_since_ms = now_ms();
                        rec.detail = Some(msg.clone());
                        Err(msg)
                    }
                },
                None => {
                    let msg = format!(
                        "executor: no executor for engine {engine:?}; refusing to run (fail closed)"
                    );
                    rec.state = AgentState::Failed;
                    rec.state_since_ms = now_ms();
                    rec.detail = Some(msg.clone());
                    Err(msg)
                }
            }
        };
        match &result {
            Ok(rec) => self.emit_lifecycle(id, "running", rec.detail.clone()),
            Err(e) => self.emit_lifecycle(id, "failed", Some(e.clone())),
        }
        result
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
        let engine = rec.engine.clone();
        match self.executors.for_engine(&engine) {
            Some(exec) => {
                let _ = exec.stop(rec);
            }
            None => {}
        }
        rec.state = AgentState::Stopped;
        rec.state_since_ms = now_ms();
        let rec = rec.clone();
        drop(reg);
        self.emit_lifecycle(id, "stopping", None);
        self.emit_lifecycle(id, "stopped", rec.detail.clone());
        Ok(rec)
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
        executors: Executors {
            mock: MockExecutor,
            native: NativeExecutor {
                bus_socket: run_dir.join(agentos_common::sockets::BUS),
                permissions_socket: run_dir.join(agentos_common::sockets::PERMISSIONS),
            },
        },
        gate: PermissionGate {
            permissions_socket: run_dir.join(agentos_common::sockets::PERMISSIONS),
        },
        agents_dir: state_dir.join("agents"),
        bus_socket: run_dir.join(agentos_common::sockets::BUS),
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
                    serde_json::json!({"service":"agent-runtime","version":agentos_common::VERSION,"executor":"native","executors":svc.executors.names()}),
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
                "agents.result" => {
                    let id = p["id"].as_str().unwrap_or_default();
                    let reg = svc.registry.lock().unwrap();
                    match reg.get(id) {
                        Some(r) => match &r.output {
                            Some(out) => RpcResponse::ok(
                                req.id,
                                serde_json::json!({
                                    "agent": r.id,
                                    "state": r.state,
                                    "engine": r.engine,
                                    "text": out.text,
                                    "ops_run": out.ops_run,
                                    "truncated": out.truncated,
                                    "duration_ms": out.duration_ms,
                                }),
                            ),
                            None => RpcResponse::err(
                                req.id,
                                agentos_ipc::ERR_INVALID_STATE,
                                format!("agent {id} has no execution result (state {:?})", r.state),
                            ),
                        },
                        None => RpcResponse::err(
                            req.id,
                            agentos_ipc::ERR_NOT_FOUND,
                            format!("unknown agent {id:?}"),
                        ),
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
        let exec = MockExecutor;
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
            output: None,
        };
        let err = exec.start(&mut rec).unwrap_err();
        assert!(err.contains("cannot execute"));
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
            output: None,
        };
        assert!(
            gate.precheck(&rec).is_err(),
            "must fail closed when permission-manager is unreachable"
        );
    }

    // -----------------------------------------------------------------------
    // Native executor (Phase 2A)
    // -----------------------------------------------------------------------

    /// A stub permission authority speaking the real wire contract, so the
    /// executor's re-check path is exercised against the actual IPC surface.
    fn stub_permissions(sock: &Path, decision: &'static str) {
        let _ = std::fs::remove_file(sock);
        let handler: Handler = Arc::new(move |req| {
            if req.method == "permissions.check" {
                RpcResponse::ok(
                    req.id,
                    serde_json::json!({"decision": decision, "reason": "stub"}),
                )
            } else {
                RpcResponse::err(req.id, agentos_ipc::ERR_UNKNOWN_METHOD, "stub")
            }
        });
        let s = sock.to_path_buf();
        std::thread::spawn(move || {
            let _ = agentos_ipc::serve(&s, handler);
        });
        for _ in 0..100 {
            if sock.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("stub permission server did not come up");
    }

    fn native_agent(dir: &Path, entry_json: &str) -> (AgentRecord, PathBuf) {
        let pkg = dir.join("pkg.agent");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(
            pkg.join("agent.json"),
            format!(r#"{{"entry":{entry_json}}}"#),
        )
        .unwrap();
        let rec = AgentRecord {
            id: "com.t.native".into(),
            name: "Native".into(),
            version: "1.0.0".into(),
            capabilities: vec![],
            requested_permissions: vec![agentos_schema::PermissionRequest {
                permission: perm::AGENT_INVOKE.into(),
                access: Some("write".into()),
                resource: None,
                reason: None,
            }],
            engine: "native".into(),
            description: None,
            install_path: pkg.display().to_string(),
            state: AgentState::Starting,
            state_since_ms: 0,
            detail: None,
            sandbox: SandboxProfile::phase1(None),
            output: None,
        };
        (rec, pkg)
    }

    const HELLO_ENTRY: &str =
        r#"{"kind":"native.task","task":{"ops":[{"op":"say","message":"Hello from AgentOS"}]}}"#;

    #[test]
    fn native_executor_runs_say_task_and_captures_result() {
        let dir = std::env::temp_dir().join(format!("agentos-rt-native-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (mut rec, _pkg) = native_agent(&dir, HELLO_ENTRY);
        let psock = dir.join("perm.sock");
        let bsock = dir.join("bus.sock");
        stub_permissions(&psock, "allow");
        // stub bus that records the announcement
        let announced = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        {
            let announced = announced.clone();
            let _ = std::fs::remove_file(&bsock);
            let handler: Handler = Arc::new(move |req| {
                if req.method == "bus.send" {
                    announced.lock().unwrap().push(req.params.clone());
                }
                RpcResponse::ok(req.id, serde_json::json!({"id":"stub"}))
            });
            let s = bsock.clone();
            std::thread::spawn(move || {
                let _ = agentos_ipc::serve(&s, handler);
            });
            for _ in 0..100 {
                if bsock.exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let exec = NativeExecutor {
            bus_socket: bsock.clone(),
            permissions_socket: psock.clone(),
        };
        exec.start(&mut rec).expect("native run must succeed");
        assert_eq!(rec.state, AgentState::Starting); // state set by Service, not executor
        let out = rec.output.as_ref().expect("output must be captured");
        assert_eq!(out.text, "Hello from AgentOS");
        assert_eq!(out.ops_run, 1);
        assert!(!out.truncated);
        assert!(rec
            .detail
            .as_deref()
            .unwrap()
            .contains("Hello from AgentOS"));
        // result announced on the bus from the runtime's identity
        let bus = announced.lock().unwrap();
        assert_eq!(bus.len(), 1);
        assert_eq!(bus[0]["kind"], "agent.result");
        assert_eq!(bus[0]["from"], "system:runtime");
        assert_eq!(bus[0]["payload"]["agent"], "com.t.native");
        assert_eq!(bus[0]["payload"]["text"], "Hello from AgentOS");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_executor_fails_closed_without_permission_authority() {
        let dir = std::env::temp_dir().join(format!("agentos-rt-native2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (mut rec, _pkg) = native_agent(&dir, HELLO_ENTRY);
        let exec = NativeExecutor {
            bus_socket: dir.join("bus.sock"),
            permissions_socket: dir.join("missing.sock"),
        };
        let err = exec.start(&mut rec).unwrap_err();
        assert!(
            err.contains("fail closed"),
            "unreachable authority must abort the run: {err}"
        );
        assert!(rec.output.is_none(), "no output may be captured on denial");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_executor_fails_closed_on_denied_permission() {
        let dir = std::env::temp_dir().join(format!("agentos-rt-native3-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (mut rec, _pkg) = native_agent(&dir, HELLO_ENTRY);
        let psock = dir.join("perm.sock");
        stub_permissions(&psock, "deny");
        let exec = NativeExecutor {
            bus_socket: dir.join("bus.sock"),
            permissions_socket: psock,
        };
        let err = exec.start(&mut rec).unwrap_err();
        assert!(err.contains("not granted"), "got: {err}");
        assert!(rec.output.is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_executor_fail_op_and_malformed_task() {
        let dir = std::env::temp_dir().join(format!("agentos-rt-native4-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let psock = dir.join("perm.sock");
        stub_permissions(&psock, "allow");

        // declared `fail` op -> deterministic execution failure
        let (mut rec, _) = native_agent(
            &dir,
            r#"{"kind":"native.task","task":{"ops":[{"op":"say","message":"a"},{"op":"fail","message":"boom"}]}}"#,
        );
        let exec = NativeExecutor {
            bus_socket: dir.join("bus.sock"),
            permissions_socket: psock.clone(),
        };
        let err = exec.start(&mut rec).unwrap_err();
        assert!(err.contains("boom"), "got: {err}");
        assert!(rec.output.is_none());

        // malformed task (unknown op) -> rejected before anything runs
        let (mut rec, _) = native_agent(
            &dir,
            r#"{"kind":"native.task","task":{"ops":[{"op":"explode"}]}}"#,
        );
        let exec = NativeExecutor {
            bus_socket: dir.join("bus.sock"),
            permissions_socket: psock,
        };
        let err = exec.start(&mut rec).unwrap_err();
        assert!(err.contains("native.task"), "got: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_executor_refuses_other_engines() {
        let dir = std::env::temp_dir().join(format!("agentos-rt-native5-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (mut rec, _) = native_agent(&dir, HELLO_ENTRY);
        rec.engine = "process".into();
        let exec = NativeExecutor {
            bus_socket: dir.join("bus.sock"),
            permissions_socket: dir.join("perm.sock"),
        };
        let err = exec.start(&mut rec).unwrap_err();
        assert!(err.contains("fail closed"), "got: {err}");
        // and the dispatcher has no executor for it either
        let ex = Executors {
            mock: MockExecutor,
            native: exec,
        };
        assert!(ex.for_engine("process").is_none());
        assert!(ex.for_engine("wasm").is_none());
        assert!(ex.for_engine("native").is_some());
        assert!(ex.for_engine("mock").is_some());
        let _ = fs::remove_dir_all(&dir);
    }
}
