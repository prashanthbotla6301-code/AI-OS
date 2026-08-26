//! AgentOS ai-orchestrator.
//!
//! Owns the Goal-oriented execution vocabulary of the OS:
//!
//!   Goal ──(Planner)──▶ Plan ──▶ Tasks (dependency graph)
//!
//! * [`Planner`] is a **trait**. Phase 1 ships [`MockPlanner`] — a fully
//!   deterministic, offline keyword planner (see `planners` doc). A Phase 2
//!   `LlmPlanner` (backed by model-router) implements the same trait and is
//!   selected by configuration; nothing else in the OS changes.
//! * Plans/tasks/journal persist to `<state>/goals/goals.json` so a service restart
//!   does not lose goals (real persistence; the OS itself is RAM-resident in
//!   Phase 1 — see docs/architecture.md).
//! * Task status transitions are validated; agents are never dispatched in
//!   Phase 1 (there is no executor yet) — tasks reach Ready/Blocked only.

use agentos_common::{new_id, now_ms};
use agentos_ipc::{serve, Handler, RpcResponse};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Core vocabulary: Goal, Plan, Task
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum GoalStatus {
    Accepted,
    Planned,
    Executing,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Goal {
    pub id: String,
    pub text: String,
    pub created_at_ms: u64,
    pub status: GoalStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum TaskStatus {
    Pending, // waiting on dependencies
    Ready,   // dispatchable to an agent with the right capability
    Running,
    Blocked, // e.g. requires a permission nobody granted
    Done,
    Failed,
}

impl TaskStatus {
    pub fn can_transition(self, to: TaskStatus) -> bool {
        use TaskStatus::*;
        match self {
            Pending => matches!(to, Ready | Blocked | Failed),
            // `Done` is accepted as an operator/mock completion in Phase 1;
            // a production dispatcher will require Ready -> Running -> Done.
            Ready => matches!(to, Running | Done | Blocked | Failed),
            Running => matches!(to, Done | Failed | Blocked),
            Blocked => matches!(to, Pending | Ready | Failed),
            Done => false,
            Failed => matches!(to, Pending),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub id: String,
    pub plan_id: String,
    pub title: String,
    /// Capability an agent must offer to pick this up (capability selection
    /// is Phase 2; the field is the contract).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    pub status: TaskStatus,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    pub id: String,
    pub goal_id: String,
    pub created_at_ms: u64,
    pub planner: String,
    pub tasks: Vec<Task>,
}

impl Plan {
    /// A plan is dispatchable iff every depends_on edge points at a task in
    /// the same plan and there are no cycles.
    pub fn validate_graph(&self) -> Result<(), String> {
        let ids: std::collections::BTreeSet<String> =
            self.tasks.iter().map(|t| t.id.clone()).collect();
        if ids.len() != self.tasks.len() {
            return Err("plan contains duplicate task ids".into());
        }
        for task in &self.tasks {
            if task.plan_id != self.id {
                return Err(format!(
                    "task {} belongs to plan {}, expected {}",
                    task.id, task.plan_id, self.id
                ));
            }
        }
        for task in &self.tasks {
            for dependency in &task.depends_on {
                if !ids.contains(dependency) {
                    return Err(format!(
                        "task {} depends on unknown {}",
                        task.id, dependency
                    ));
                }
            }
        }
        // Cycle check (DFS). Owned String keys keep the recursive helper's
        // lifetimes independent from the borrowed Plan.
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            White,
            Grey,
            Black,
        }
        let mut marks: std::collections::BTreeMap<String, Mark> =
            ids.iter().cloned().map(|id| (id, Mark::White)).collect();
        fn visit(
            id: &str,
            plan: &Plan,
            marks: &mut std::collections::BTreeMap<String, Mark>,
        ) -> Result<(), String> {
            match marks.get(id) {
                Some(Mark::Grey) => return Err(format!("cycle detected at {id}")),
                Some(Mark::Black) => return Ok(()),
                _ => {}
            }
            marks.insert(id.to_string(), Mark::Grey);
            let task = plan
                .tasks
                .iter()
                .find(|task| task.id == id)
                .expect("validated task id");
            for dependency in &task.depends_on {
                visit(dependency, plan, marks)?;
            }
            marks.insert(id.to_string(), Mark::Black);
            Ok(())
        }
        for task in &self.tasks {
            visit(&task.id, self, &mut marks)?;
        }
        Ok(())
    }

    /// Tasks whose deps are all Done become Ready.
    pub fn refresh_ready(&mut self) {
        // Read the status map first; borrowing `self.tasks` immutably while
        // iterating it mutably is rejected by Rust and would also make the
        // transition semantics harder to reason about.
        let done: std::collections::BTreeSet<String> = self
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Done)
            .map(|task| task.id.clone())
            .collect();
        for task in &mut self.tasks {
            if task.status == TaskStatus::Pending
                && (task.depends_on.is_empty()
                    || task.depends_on.iter().all(|id| done.contains(id)))
            {
                task.status = TaskStatus::Ready;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Planner trait — the LLM plug-in point
// ---------------------------------------------------------------------------

/// A planner turns a Goal into a Plan. Implementations MUST be deterministic
/// for the same goal text OR clearly labelled otherwise; MockPlanner is.
pub trait Planner: Send + Sync {
    fn name(&self) -> &'static str;
    fn plan(&self, goal: &Goal) -> Plan;
}

/// Deterministic offline planner. Reads as honest scaffolding: it recognises
/// a few goal families and produces fixed plans. This file is the ONLY place
/// fake intelligence exists in AgentOS, and it is clearly marked.
pub mod planners {
    use super::*;

    fn hash_str(s: &str) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in s.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    fn mk(goal: &Goal, planner: &str, titles: &[(&str, Option<&str>, bool, &[usize])]) -> Plan {
        // (title, capability, blocked, depends_on indices)
        let plan_id = format!(
            "plan-{:012x}",
            hash_str(&format!("{}|{}", goal.id, planner)) & 0xfff_ffff_ffff
        );
        let tasks = titles
            .iter()
            .enumerate()
            .map(|(i, (title, cap, blocked, deps))| Task {
                id: format!(
                    "task-{:012x}-{}",
                    hash_str(&format!("{}/{}/{}", goal.id, plan_id, i)) & 0xfff_ffff_ffff,
                    i + 1
                ),
                plan_id: plan_id.clone(),
                title: title.to_string(),
                capability: cap.map(str::to_string),
                status: if *blocked {
                    TaskStatus::Blocked
                } else {
                    TaskStatus::Ready
                },
                depends_on: deps
                    .iter()
                    .map(|d| {
                        format!(
                            "task-{:012x}-{}",
                            hash_str(&format!("{}/{}/{}", goal.id, plan_id, *d)) & 0xfff_ffff_ffff,
                            *d + 1
                        )
                    })
                    .collect(),
                note: if *blocked {
                    Some(
                        "requires a permission that is deny-by-default in Phase 1 (network)".into(),
                    )
                } else {
                    None
                },
            })
            .collect();
        Plan {
            id: plan_id,
            goal_id: goal.id.clone(),
            created_at_ms: now_ms(),
            planner: planner.to_string(),
            tasks,
        }
    }

    pub struct MockPlanner;

    impl Planner for MockPlanner {
        fn name(&self) -> &'static str {
            "mock"
        }

        fn plan(&self, goal: &Goal) -> Plan {
            let text = goal.text.to_lowercase();
            let has = |words: &[&str]| words.iter().any(|w| text.contains(w));

            if has(&[
                "organize",
                "organise",
                "clean",
                "sort",
                "tidy",
                "folder",
                "downloads",
                "file",
            ]) {
                return mk(
                    goal,
                    "mock",
                    &[
                        (
                            "Scan target directories and build a file inventory",
                            Some("fs.scan"),
                            false,
                            &[],
                        ),
                        (
                            "Classify files by type, age and usage",
                            Some("fs.classify"),
                            false,
                            &[0],
                        ),
                        (
                            "Propose a folder structure and move plan",
                            Some("fs.plan"),
                            false,
                            &[1],
                        ),
                        ("Apply the move plan", Some("fs.write"), true, &[2]), // needs filesystem.write
                        ("Report what changed", Some("report"), false, &[3]),
                    ],
                );
            }
            if has(&[
                "summar",
                "read",
                "document",
                "report",
                "digest",
                "write about",
            ]) {
                return mk(
                    goal,
                    "mock",
                    &[
                        (
                            "Locate and read the source material",
                            Some("fs.read"),
                            false,
                            &[],
                        ),
                        (
                            "Extract key points and open questions",
                            Some("text.analyze"),
                            false,
                            &[0],
                        ),
                        ("Draft the summary", Some("text.generate"), false, &[1]),
                        (
                            "Present the summary to the user",
                            Some("ui.present"),
                            false,
                            &[2],
                        ),
                    ],
                );
            }
            if has(&[
                "net",
                "fetch",
                "http",
                "browse",
                "download from",
                "web",
                "online",
                "email",
            ]) {
                return mk(
                    goal,
                    "mock",
                    &[
                        (
                            "Check network permission for the requesting agent",
                            Some("net.check"),
                            false,
                            &[],
                        ),
                        (
                            "Fetch the requested resources",
                            Some("net.fetch"),
                            true,
                            &[0],
                        ), // needs network
                        ("Process and store results", Some("fs.write"), true, &[1]),
                        ("Report outcomes", Some("report"), false, &[2]),
                    ],
                );
            }
            // generic fallback
            mk(
                goal,
                "mock",
                &[
                    (
                        "Understand the goal and identify success criteria",
                        Some("reason"),
                        false,
                        &[],
                    ),
                    (
                        "Decompose the goal into concrete steps",
                        Some("reason"),
                        false,
                        &[0],
                    ),
                    (
                        "Select agents with the required capabilities",
                        Some("agent.select"),
                        false,
                        &[1],
                    ),
                    (
                        "Execute steps and track progress",
                        Some("agent.dispatch"),
                        false,
                        &[2],
                    ),
                    (
                        "Verify the outcome against the success criteria",
                        Some("verify"),
                        false,
                        &[3],
                    ),
                ],
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Store (journal-backed)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Journal {
    pub goals: Vec<Goal>,
    pub plans: Vec<Plan>,
}

pub struct Store {
    journal: Journal,
    path: Option<PathBuf>,
}

impl Store {
    pub fn load(path: Option<PathBuf>) -> Store {
        let journal = path
            .as_ref()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Store { journal, path }
    }

    pub fn save(&self) {
        if let Some(p) = &self.path {
            if let Ok(body) = serde_json::to_string_pretty(&self.journal) {
                let _ = agentos_common::atomic_write(p, body.as_bytes());
            }
        }
    }

    pub fn submit_goal(&mut self, text: &str, planner: &dyn Planner) -> (Goal, Plan) {
        let goal = Goal {
            id: new_id(),
            text: text.to_string(),
            created_at_ms: now_ms(),
            status: GoalStatus::Accepted,
        };
        let mut plan = planner.plan(&goal);
        plan.refresh_ready();
        let goal = Goal {
            status: GoalStatus::Planned,
            ..goal
        };
        self.journal.goals.push(goal.clone());
        self.journal.plans.push(plan.clone());
        self.save();
        (goal, plan)
    }

    pub fn goals(&self) -> Vec<Goal> {
        self.journal.goals.clone()
    }
    pub fn plans(&self) -> Vec<Plan> {
        self.journal.plans.clone()
    }
    pub fn tasks(&self) -> Vec<Task> {
        self.journal
            .plans
            .iter()
            .flat_map(|p| p.tasks.clone())
            .collect()
    }

    pub fn update_task(
        &mut self,
        task_id: &str,
        to: TaskStatus,
        note: Option<String>,
    ) -> Result<Task, String> {
        let mut updated_plan = None;
        let mut result = None;
        for (index, plan) in self.journal.plans.iter_mut().enumerate() {
            if let Some(task) = plan.tasks.iter_mut().find(|task| task.id == task_id) {
                if !task.status.can_transition(to) {
                    return Err(format!(
                        "illegal task transition {:?} -> {:?} for {task_id}",
                        task.status, to
                    ));
                }
                task.status = to;
                if note.is_some() {
                    task.note = note.clone();
                }
                result = Some(task.clone());
                updated_plan = Some(index);
                break;
            }
        }
        let Some(index) = updated_plan else {
            return Err(format!("unknown task {task_id:?}"));
        };
        // The mutable task borrow has ended before this second operation.
        self.journal.plans[index].refresh_ready();
        self.save();
        Ok(result.expect("updated task result is present"))
    }
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

fn main() {
    let paths = agentos_common::Paths::detect();
    let mut socket = paths.socket(agentos_common::sockets::ORCHESTRATOR);
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
    agentos_common::logger_init("ai-orchestrator");
    agentos_common::info(
        "ai-orchestrator starting (planner: mock — deterministic Phase 1 planner)",
    );

    // Planner selection: config key `planner` in <etc>/agentos.json, only
    // "mock" exists in Phase 1; an LLM planner registers here in Phase 2.
    let planner: Box<dyn Planner> = Box::new(planners::MockPlanner);
    let bus_socket = run_dir.join(agentos_common::sockets::BUS);

    let store = Arc::new(Mutex::new(Store::load(Some(
        state_dir.join("goals").join("goals.json"),
    ))));

    let handler: Handler = {
        let store = store.clone();
        let bus_socket = bus_socket.clone();
        Arc::new(move |req| {
            let p = &req.params;
            match req.method.as_str() {
                "ping" => RpcResponse::ok(
                    req.id,
                    serde_json::json!({"service":"ai-orchestrator","version":agentos_common::VERSION,"planner":"mock"}),
                ),
                "goals.submit" => {
                    let text = p["text"].as_str().unwrap_or_default().trim().to_string();
                    if text.is_empty() || text.len() > 500 {
                        return RpcResponse::err(
                            req.id,
                            agentos_ipc::ERR_INVALID_PARAMS,
                            "text must be 1..500 chars",
                        );
                    }
                    let (goal, plan) = store.lock().unwrap().submit_goal(&text, planner.as_ref());
                    let _ = agentos_ipc::call(
                        &bus_socket,
                        "bus.send",
                        serde_json::json!({
                            "from": "system:orchestrator",
                            "to": "topic:goals",
                            "kind": "goal.planned",
                            "payload": { "goal_id": goal.id, "plan_id": plan.id, "tasks": plan.tasks.len() }
                        }),
                    );
                    RpcResponse::ok(req.id, serde_json::json!({ "goal": goal, "plan": plan }))
                }
                "goals.list" => RpcResponse::ok(
                    req.id,
                    serde_json::json!({ "goals": store.lock().unwrap().goals() }),
                ),
                "plans.list" => RpcResponse::ok(
                    req.id,
                    serde_json::json!({ "plans": store.lock().unwrap().plans() }),
                ),
                "tasks.list" => RpcResponse::ok(
                    req.id,
                    serde_json::json!({ "tasks": store.lock().unwrap().tasks() }),
                ),
                "tasks.update" => {
                    let id = p["id"].as_str().unwrap_or_default().to_string();
                    let status = p["status"].as_str().unwrap_or_default().to_string();
                    let Ok(status) =
                        serde_json::from_value::<TaskStatus>(serde_json::Value::String(status))
                    else {
                        return RpcResponse::err(
                            req.id,
                            agentos_ipc::ERR_INVALID_PARAMS,
                            "invalid task status",
                        );
                    };
                    let note = p["note"].as_str().map(str::to_string);
                    match store.lock().unwrap().update_task(&id, status, note) {
                        Ok(t) => {
                            RpcResponse::ok(req.id, serde_json::to_value(t).unwrap_or_default())
                        }
                        Err(e) => RpcResponse::err(req.id, agentos_ipc::ERR_INVALID_STATE, e),
                    }
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
        agentos_common::error(&format!("ai-orchestrator fatal: {e}"));
        std::process::exit(1);
    }
}

use std::sync::Arc;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_common::new_id;

    fn goal(text: &str) -> Goal {
        Goal {
            id: new_id(),
            text: text.into(),
            created_at_ms: 1,
            status: GoalStatus::Accepted,
        }
    }

    #[test]
    fn mock_planner_is_deterministic() {
        let p = planners::MockPlanner;
        let g = goal("Organize my downloads folder");
        let a = p.plan(&g);
        let b = p.plan(&g);
        assert_eq!(a, b, "same goal text must produce identical plans");
        assert_eq!(
            a.tasks.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
            b.tasks.iter().map(|t| t.id.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn planner_families() {
        let p = planners::MockPlanner;
        let file_plan = p.plan(&goal("clean up my files"));
        assert!(file_plan
            .tasks
            .iter()
            .any(|t| t.capability.as_deref() == Some("fs.scan")));
        let sum_plan = p.plan(&goal("summarize this document"));
        assert!(sum_plan
            .tasks
            .iter()
            .any(|t| t.capability.as_deref() == Some("text.generate")));
        let net_plan = p.plan(&goal("fetch the latest news from the web"));
        assert!(
            net_plan
                .tasks
                .iter()
                .any(|t| t.status == TaskStatus::Blocked),
            "network tasks must be blocked in Phase 1"
        );
        let gen = p.plan(&goal("do a thing"));
        assert!(gen.tasks.len() >= 3);
    }

    #[test]
    fn plan_graphs_are_valid_and_acyclic() {
        let p = planners::MockPlanner;
        for text in [
            "organize files",
            "summarize the report",
            "fetch news",
            "whatever",
        ] {
            let plan = p.plan(&goal(text));
            assert!(
                plan.validate_graph().is_ok(),
                "plan for {text:?} must be valid"
            );
        }
    }

    #[test]
    fn dependency_chain_blocks_until_done() {
        let mut plan = Plan {
            id: "plan-1".into(),
            goal_id: "g".into(),
            created_at_ms: 0,
            planner: "mock".into(),
            tasks: vec![
                Task {
                    id: "t1".into(),
                    plan_id: "plan-1".into(),
                    title: "a".into(),
                    capability: None,
                    status: TaskStatus::Pending,
                    depends_on: vec![],
                    note: None,
                },
                Task {
                    id: "t2".into(),
                    plan_id: "plan-1".into(),
                    title: "b".into(),
                    capability: None,
                    status: TaskStatus::Pending,
                    depends_on: vec!["t1".into()],
                    note: None,
                },
            ],
        };
        plan.refresh_ready();
        assert_eq!(plan.tasks[0].status, TaskStatus::Ready);
        assert_eq!(plan.tasks[1].status, TaskStatus::Pending);
        plan.tasks[0].status = TaskStatus::Done;
        plan.refresh_ready();
        assert_eq!(plan.tasks[1].status, TaskStatus::Ready);
    }

    #[test]
    fn cyclic_plan_is_rejected() {
        let plan = Plan {
            id: "p".into(),
            goal_id: "g".into(),
            created_at_ms: 0,
            planner: "mock".into(),
            tasks: vec![
                Task {
                    id: "a".into(),
                    plan_id: "p".into(),
                    title: "a".into(),
                    capability: None,
                    status: TaskStatus::Pending,
                    depends_on: vec!["b".into()],
                    note: None,
                },
                Task {
                    id: "b".into(),
                    plan_id: "p".into(),
                    title: "b".into(),
                    capability: None,
                    status: TaskStatus::Pending,
                    depends_on: vec!["a".into()],
                    note: None,
                },
            ],
        };
        assert!(plan.validate_graph().is_err());
    }

    #[test]
    fn task_status_transitions() {
        use TaskStatus::*;
        assert!(Ready.can_transition(Running));
        assert!(Running.can_transition(Done));
        assert!(!Done.can_transition(Running));
        assert!(!Failed.can_transition(Running));
        assert!(Failed.can_transition(Pending));
        assert!(Blocked.can_transition(Ready));
    }

    #[test]
    fn journal_roundtrip_and_update() {
        let dir = std::env::temp_dir().join(format!("agentos-orch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("goals.json");
        let mut s = Store::load(Some(path.clone()));
        let (g, p) = s.submit_goal("organize my files", &planners::MockPlanner);
        assert_eq!(g.status, GoalStatus::Planned);
        let t0 = p.tasks[0].id.clone();
        let t = s
            .update_task(&t0, TaskStatus::Done, Some("done".into()))
            .unwrap();
        assert_eq!(t.status, TaskStatus::Done);
        assert!(
            s.update_task(&t0, TaskStatus::Running, None).is_err(),
            "Done is terminal"
        );
        // reload from disk
        let s2 = Store::load(Some(path));
        assert_eq!(s2.goals().len(), 1);
        assert_eq!(s2.tasks().len(), p.tasks.len());
        assert_eq!(
            s2.tasks().iter().find(|t| t.id == t0).unwrap().status,
            TaskStatus::Done
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
