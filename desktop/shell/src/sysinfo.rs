//! Live system snapshot for the desktop: CPU/mem sampling, service health,
//! agents, goals/tasks, bus traffic — polled over AgentOS IPC in a worker
//! thread so the UI thread never blocks on a slow service.

use agentos_common::Paths;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct Snapshot {
    pub ok: bool,
    pub version: String,
    pub kernel: String,
    pub hostname: String,
    pub uptime_s: u64,
    pub cpu_percent: f32,
    pub mem_total_kb: u64,
    pub mem_avail_kb: u64,
    pub cpu_count: usize,
    pub services: Vec<ServiceRow>,
    pub agents: Vec<AgentRow>,
    pub goals: Vec<GoalRow>,
    pub tasks: Vec<TaskRow>,
    pub bus_recent: usize,
    pub bus_denied: u64,
    pub models: Vec<(String, String, String)>, // id, kind, model
    pub permissions_policy: Value,
    pub grants: Value,
    pub updated_ms: u64,
}

#[derive(Clone)]
pub struct ServiceRow {
    pub name: String,
    pub state: String,
    pub pid: Option<u32>,
    pub restarts: u32,
}

#[derive(Clone)]
pub struct AgentRow {
    pub id: String,
    pub name: String,
    pub version: String,
    pub state: String,
    pub detail: Option<String>,
    pub engine: String,
    pub capabilities: Vec<String>,
    pub permissions: Vec<String>,
    /// Text output of the last execution (native executor result).
    pub output: Option<String>,
}

#[derive(Clone)]
pub struct GoalRow {
    pub id: String,
    pub text: String,
    pub status: String,
    pub tasks: usize,
    pub plan_tasks: Vec<TaskRow>,
}

#[derive(Clone)]
pub struct TaskRow {
    pub id: String,
    pub title: String,
    pub capability: String,
    pub status: String,
}

pub struct Sampler {
    pub snap: Arc<Mutex<Snapshot>>,
    pub dirty: Arc<AtomicBool>,
    paths: Paths,
}

fn cpu_sample() -> (u64, u64) {
    // (total, idle) jiffies from /proc/stat
    if let Ok(s) = std::fs::read_to_string("/proc/stat") {
        if let Some(line) = s.lines().next() {
            let vals: Vec<u64> = line
                .split_whitespace()
                .skip(1)
                .filter_map(|v| v.parse().ok())
                .collect();
            let total: u64 = vals.iter().sum();
            let idle = vals.get(3).copied().unwrap_or(0) + vals.get(4).copied().unwrap_or(0);
            return (total, idle);
        }
    }
    (0, 0)
}

impl Sampler {
    pub fn start() -> Sampler {
        let paths = Paths::detect();
        let s = Sampler {
            snap: Arc::new(Mutex::new(Snapshot::default())),
            dirty: Arc::new(AtomicBool::new(true)),
            paths,
        };
        let snap = s.snap.clone();
        let dirty = s.dirty.clone();
        let paths = s.paths.clone();
        std::thread::spawn(move || {
            let mut last_cpu = cpu_sample();

            loop {
                let mut out = Snapshot::default();
                out.cpu_count = std::fs::read_dir("/sys/devices/system/cpu")
                    .map(|d| {
                        d.filter_map(|e| e.ok())
                            .filter(|e| e.file_name().to_string_lossy().starts_with("cpu"))
                            .count()
                    })
                    .unwrap_or(1)
                    .max(1);
                // memory
                if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
                    for line in s.lines() {
                        if let Some(v) = line.strip_prefix("MemTotal:") {
                            out.mem_total_kb = v
                                .trim()
                                .split(' ')
                                .next()
                                .and_then(|x| x.parse().ok())
                                .unwrap_or(0);
                        }
                        if let Some(v) = line.strip_prefix("MemAvailable:") {
                            out.mem_avail_kb = v
                                .trim()
                                .split(' ')
                                .next()
                                .and_then(|x| x.parse().ok())
                                .unwrap_or(0);
                        }
                    }
                }
                // cpu delta
                let now_cpu = cpu_sample();
                let d_total = now_cpu.0.saturating_sub(last_cpu.0);
                let d_idle = now_cpu.1.saturating_sub(last_cpu.1);
                if d_total > 0 {
                    out.cpu_percent =
                        (1.0 - (d_idle as f32 / d_total as f32)).clamp(0.0, 1.0) * 100.0;
                }
                last_cpu = now_cpu;

                // ---- IPC round ----
                let call = |sock: &str, method: &str, params: Value| {
                    agentos_ipc::call(&paths.socket(sock), method, params)
                };
                if let Ok(v) = call(
                    agentos_common::sockets::CORE,
                    "status",
                    serde_json::json!({}),
                ) {
                    out.ok = true;
                    out.version = v["version"].as_str().unwrap_or_default().to_string();
                    out.kernel = v["kernel"].as_str().unwrap_or_default().to_string();
                    out.hostname = v["hostname"].as_str().unwrap_or_default().to_string();
                    out.uptime_s = v["uptime_s"].as_u64().unwrap_or(0);
                }
                if let Ok(v) = call(
                    agentos_common::sockets::CORE,
                    "services",
                    serde_json::json!({}),
                ) {
                    if let Some(arr) = v["services"].as_array() {
                        out.services = arr
                            .iter()
                            .map(|s| ServiceRow {
                                name: s["name"].as_str().unwrap_or("?").into(),
                                state: s["state"].as_str().unwrap_or("?").into(),
                                pid: s["pid"].as_u64().map(|p| p as u32),
                                restarts: s["restarts"].as_u64().unwrap_or(0) as u32,
                            })
                            .collect();
                    }
                }
                if let Ok(v) = call(
                    agentos_common::sockets::RUNTIME,
                    "agents.list",
                    serde_json::json!({}),
                ) {
                    if let Some(arr) = v["agents"].as_array() {
                        out.agents = arr
                            .iter()
                            .map(|a| AgentRow {
                                id: a["id"].as_str().unwrap_or("?").into(),
                                name: a["name"].as_str().unwrap_or("?").into(),
                                version: a["version"].as_str().unwrap_or("?").into(),
                                state: a["state"].as_str().unwrap_or("?").into(),
                                detail: a["detail"].as_str().map(str::to_string),
                                engine: a["engine"].as_str().unwrap_or("?").into(),
                                capabilities: a["capabilities"]
                                    .as_array()
                                    .map(|c| {
                                        c.iter()
                                            .filter_map(|x| x.as_str().map(str::to_string))
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                permissions: a["requested_permissions"]
                                    .as_array()
                                    .map(|c| {
                                        c.iter()
                                            .filter_map(|x| {
                                                x["permission"].as_str().map(str::to_string)
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                output: a["output"]["text"].as_str().map(str::to_string),
                            })
                            .collect();
                    }
                }
                if let Ok(v) = call(
                    agentos_common::sockets::ORCHESTRATOR,
                    "plans.list",
                    serde_json::json!({}),
                ) {
                    if let Some(plans) = v["plans"].as_array() {
                        for p in plans {
                            let tasks: Vec<TaskRow> = p["tasks"]
                                .as_array()
                                .map(|ts| {
                                    ts.iter()
                                        .map(|t| TaskRow {
                                            id: t["id"].as_str().unwrap_or("?").into(),
                                            title: t["title"].as_str().unwrap_or("?").into(),
                                            capability: t["capability"]
                                                .as_str()
                                                .unwrap_or("—")
                                                .into(),
                                            status: t["status"].as_str().unwrap_or("?").into(),
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            out.tasks.extend(tasks.clone());
                            out.goals.push(GoalRow {
                                id: p["goal_id"].as_str().unwrap_or("?").into(),
                                text: String::new(),
                                status: String::new(),
                                tasks: tasks.len(),
                                plan_tasks: tasks,
                            });
                        }
                    }
                }
                if let Ok(v) = call(
                    agentos_common::sockets::ORCHESTRATOR,
                    "goals.list",
                    serde_json::json!({}),
                ) {
                    if let Some(gs) = v["goals"].as_array() {
                        for (i, g) in gs.iter().enumerate() {
                            if let Some(row) = out.goals.get_mut(i) {
                                row.id = g["id"].as_str().unwrap_or("?").into();
                                row.text = g["text"].as_str().unwrap_or("?").into();
                                row.status = g["status"].as_str().unwrap_or("?").into();
                            }
                        }
                    }
                }
                if let Ok(v) = call(
                    agentos_common::sockets::BUS,
                    "bus.recent",
                    serde_json::json!({"limit": 1}),
                ) {
                    out.bus_recent = v["envelopes"].as_array().map(|a| a.len()).unwrap_or(0);
                }
                if let Ok(v) = call(
                    agentos_common::sockets::MODELS,
                    "models.list",
                    serde_json::json!({}),
                ) {
                    if let Some(arr) = v["providers"].as_array() {
                        out.models = arr
                            .iter()
                            .filter(|p| p["enabled"].as_bool().unwrap_or(false))
                            .map(|p| {
                                (
                                    p["id"].as_str().unwrap_or("?").into(),
                                    p["kind"].as_str().unwrap_or("?").into(),
                                    p["model"].as_str().unwrap_or("?").into(),
                                )
                            })
                            .collect();
                    }
                }
                if let Ok(v) = call(
                    agentos_common::sockets::PERMISSIONS,
                    "policy.get",
                    serde_json::json!({}),
                ) {
                    out.permissions_policy = v;
                }
                if let Ok(v) = call(
                    agentos_common::sockets::PERMISSIONS,
                    "grants.list",
                    serde_json::json!({}),
                ) {
                    out.grants = v;
                }
                out.updated_ms = agentos_common::now_ms();
                *snap.lock().unwrap() = out;
                dirty.store(true, Ordering::Release);
                std::thread::sleep(std::time::Duration::from_millis(900));
            }
        });
        s
    }
}
