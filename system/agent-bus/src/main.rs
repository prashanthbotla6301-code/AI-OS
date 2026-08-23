//! AgentOS agent-bus.
//!
//! The future inter-agent communication fabric:
//!
//! ```text
//! Agent A ──▶ agent-bus ──▶ permission check (agent.invoke) ──▶ Agent B
//!                       └──▶ topic subscribers (system services, UI)
//! ```
//!
//! Phase 1 implements the real message structures, routing, a recent-message
//! audit ring, and subscription delivery over each subscriber's socket.
//! Direct agent→agent sends are already gated by permission-manager
//! (`agent.invoke` on the sender, deny-by-default).
//!
//! Addresses: `agent:<agent-id>` direct, `system:<service>` for OS services,
//! `topic:<name>` for pub/sub.

use agentos_common::{new_id, now_ms};
use agentos_ipc::{Event, RpcRequest, RpcResponse};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub const RECENT_CAP: usize = 256;

// ---------------------------------------------------------------------------
// Envelope — the canonical inter-agent message
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Envelope {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub ts_ms: u64,
    pub from: String,
    pub to: String,
    pub kind: String,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

impl Envelope {
    pub fn new(
        from: impl Into<String>,
        to: impl Into<String>,
        kind: impl Into<String>,
        payload: serde_json::Value,
    ) -> Envelope {
        Envelope {
            id: new_id(),
            ts_ms: now_ms(),
            from: from.into(),
            to: to.into(),
            kind: kind.into(),
            payload,
            trace_id: None,
        }
    }
}

/// Does a subscriber filter match this envelope? `None` = receive everything.
/// A filter matches on the `to` address or the `kind` (exact or prefix `x*`).
pub fn filter_matches(filter: &str, env: &Envelope) -> bool {
    if let Some(prefix) = filter.strip_suffix('*') {
        env.to.starts_with(prefix) || env.kind.starts_with(prefix)
    } else {
        env.to == filter || env.kind == filter
    }
}

// ---------------------------------------------------------------------------
// Bus state
// ---------------------------------------------------------------------------

struct Subscriber {
    stream: UnixStream,
    filters: Option<Vec<String>>,
}

pub struct Bus {
    subscribers: Vec<Subscriber>,
    recent: VecDeque<Envelope>,
    permissions_socket: PathBuf,
    denied: u64,
    delivered: u64,
}

impl Bus {
    pub fn new(permissions_socket: PathBuf) -> Bus {
        Bus {
            subscribers: Vec::new(),
            recent: VecDeque::new(),
            permissions_socket,
            denied: 0,
            delivered: 0,
        }
    }

    pub fn recent(&self, limit: usize) -> Vec<Envelope> {
        self.recent.iter().rev().take(limit).cloned().collect()
    }

    /// Send an envelope: permission-check agent senders, then deliver.
    /// Returns Ok(envelope id) or the denial reason.
    pub fn send(&mut self, mut env: Envelope) -> Result<String, String> {
        // Permission gate: only agents need consent to talk; system services
        // are trusted infrastructure (they already run under the OS's control).
        if env.from.starts_with("agent:") || env.from.starts_with("com.") {
            let res = agentos_ipc::call(
                &self.permissions_socket,
                "permissions.check",
                serde_json::json!({"subject": env.from, "permission": "agent.invoke", "access": "write", "resource": env.to}),
            );
            let ok = matches!(res, Ok(v) if v["decision"] == "allow");
            if !ok {
                self.denied += 1;
                return Err(format!(
                    "sender {} lacks agent.invoke for {} (deny-by-default)",
                    env.from, env.to
                ));
            }
        }
        if env.id.is_empty() {
            env.id = new_id();
        }
        if env.ts_ms == 0 {
            env.ts_ms = now_ms();
        }
        // deliver
        let mut delivered = 0;
        for s in self.subscribers.iter_mut() {
            let wants = match &s.filters {
                None => true,
                Some(fs) => fs.iter().any(|f| filter_matches(f, &env)),
            };
            if wants {
                let ev = Event {
                    event: "bus.deliver".into(),
                    params: serde_json::to_value(&env).unwrap_or_default(),
                };
                let mut line = serde_json::to_string(&ev).unwrap_or_default();
                line.push('\n');
                if s.stream
                    .write_all(line.as_bytes())
                    .and_then(|_| s.stream.flush())
                    .is_ok()
                {
                    delivered += 1;
                }
            }
        }
        self.delivered += delivered as u64;
        self.recent.push_back(env.clone());
        while self.recent.len() > RECENT_CAP {
            self.recent.pop_front();
        }
        Ok(env.id)
    }
}

// ---------------------------------------------------------------------------
// Daemon (custom loop: RPC + subscriptions on the same socket)
// ---------------------------------------------------------------------------

struct SharedBus(Mutex<Bus>);

fn main() {
    let paths = agentos_common::Paths::detect();
    let mut socket = paths.socket(agentos_common::sockets::BUS);
    let mut run_dir = paths.run_dir;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from).unwrap_or_default(),
            "--run-dir" => run_dir = args.next().map(PathBuf::from).unwrap_or_default(),
            _ => {}
        }
    }
    agentos_common::logger_init("agent-bus");
    agentos_common::info("agent-bus starting");

    agentos_ipc::clean_stale_socket(&socket).expect("bus socket path unusable");
    std::fs::create_dir_all(socket.parent().unwrap()).ok();
    let listener = UnixListener::bind(&socket).expect("cannot bind bus socket");
    agentos_ipc::set_socket_permissions(&socket).expect("cannot apply bus socket policy");

    let bus = Arc::new(SharedBus(Mutex::new(Bus::new(
        run_dir.join(agentos_common::sockets::PERMISSIONS),
    ))));

    for stream in listener.incoming().flatten() {
        let bus = bus.clone();
        std::thread::spawn(move || {
            let _ = handle_conn(stream, bus);
        });
    }
}

fn handle_conn(stream: UnixStream, bus: Arc<SharedBus>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(());
        }
        if n > agentos_ipc::MAX_FRAME_BYTES {
            let resp = RpcResponse::err(
                0,
                agentos_ipc::ERR_INVALID_PARAMS,
                "request frame exceeds 1 MiB",
            );
            reply(&mut writer, &resp)?;
            return Ok(());
        }
        let Ok(req) = serde_json::from_str::<RpcRequest>(&line) else {
            let resp = RpcResponse::err(0, agentos_ipc::ERR_INVALID_PARAMS, "malformed request");
            let mut out = serde_json::to_string(&resp)?;
            out.push('\n');
            writer.write_all(out.as_bytes())?;
            continue;
        };
        match req.method.as_str() {
            "ping" => {
                let (subscribers, recent, delivered, denied) = {
                    let b = bus.0.lock().unwrap();
                    (b.subscribers.len(), b.recent.len(), b.delivered, b.denied)
                };
                let resp = RpcResponse::ok(
                    req.id,
                    serde_json::json!({"service":"agent-bus","version":agentos_common::VERSION,"subscribers":subscribers,"recent":recent,"delivered":delivered,"denied":denied}),
                );
                reply(&mut writer, &resp)?;
            }
            "bus.send" => {
                let env: Envelope = match serde_json::from_value::<Envelope>(req.params.clone()) {
                    Ok(mut e) => {
                        if e.id.is_empty() {
                            e.id = agentos_common::new_id();
                        }
                        e
                    }
                    Err(err) => {
                        let resp = RpcResponse::err(
                            req.id,
                            agentos_ipc::ERR_INVALID_PARAMS,
                            format!("invalid envelope: {err}"),
                        );
                        reply(&mut writer, &resp)?;
                        continue;
                    }
                };
                let result = bus.0.lock().unwrap().send(env);
                let resp = match result {
                    Ok(id) => RpcResponse::ok(req.id, serde_json::json!({ "id": id })),
                    Err(reason) => RpcResponse::err(req.id, agentos_ipc::ERR_PERMISSION, reason),
                };
                reply(&mut writer, &resp)?;
            }
            "bus.recent" => {
                let limit = req.params["limit"]
                    .as_u64()
                    .unwrap_or(50)
                    .min(RECENT_CAP as u64) as usize;
                let recent = bus.0.lock().unwrap().recent(limit);
                let resp = RpcResponse::ok(req.id, serde_json::json!({ "envelopes": recent }));
                reply(&mut writer, &resp)?;
            }
            "bus.subscribe" => {
                let filters: Option<Vec<String>> = req.params["filters"].as_array().map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                });
                // ack the subscription, then flip this connection into
                // push-mode; further lines from it are ignored.
                let resp = RpcResponse::ok(
                    req.id,
                    serde_json::json!({"subscribed": true, "filters": filters}),
                );
                reply(&mut writer, &resp)?;
                let mut b = bus.0.lock().unwrap();
                // NOTE: no dedup by peer address (SocketAddr lacks PartialEq);
                // a subscribing connection stops sending requests anyway.
                b.subscribers.push(Subscriber {
                    stream: writer.try_clone()?,
                    filters,
                });
                return Ok(()); // connection now receives events only
            }
            _ => {
                let resp = RpcResponse::err(
                    req.id,
                    agentos_ipc::ERR_UNKNOWN_METHOD,
                    format!("unknown method {}", req.method),
                );
                reply(&mut writer, &resp)?;
            }
        }
    }
}

fn reply(w: &mut UnixStream, resp: &RpcResponse) -> std::io::Result<()> {
    let mut out = serde_json::to_string(resp)?;
    out.push('\n');
    w.write_all(out.as_bytes())?;
    w.flush()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn env(from: &str, to: &str, kind: &str) -> Envelope {
        Envelope {
            id: "e1".into(),
            ts_ms: 42,
            from: from.into(),
            to: to.into(),
            kind: kind.into(),
            payload: serde_json::json!({"x":1}),
            trace_id: Some("t0".into()),
        }
    }

    #[test]
    fn envelope_roundtrips_through_json() {
        let e = env("agent:com.a", "agent:com.b", "msg.direct");
        let s = serde_json::to_string(&e).unwrap();
        let back: Envelope = serde_json::from_str(&s).unwrap();
        assert_eq!(e, back);
    }

    #[test]
    fn filter_matching() {
        let e = env("agent:a", "topic:goals", "goal.planned");
        assert!(filter_matches("topic:goals", &e));
        assert!(filter_matches("goal.planned", &e));
        assert!(filter_matches("goal*", &e));
        assert!(filter_matches("topic:*", &e));
        assert!(!filter_matches("agent:com.b", &e));
    }

    #[test]
    fn system_senders_bypass_agent_invoke_but_agents_need_grant() {
        // No live permission-manager: agent senders must fail closed.
        let mut bus = Bus::new(PathBuf::from("/nonexistent/permissions.sock"));
        let sys = bus
            .send(env("system:core", "topic:events", "sys.test"))
            .unwrap();
        assert!(!sys.is_empty());
        assert_eq!(bus.delivered, 0, "no subscribers yet, still accepted");
        let err = bus
            .send(env("com.untrusted.agent", "agent:com.other", "msg.direct"))
            .unwrap_err();
        assert!(err.contains("agent.invoke"));
        assert_eq!(bus.denied, 1);
    }

    #[test]
    fn recent_ring_bounded() {
        let mut bus = Bus::new(PathBuf::from("/nonexistent/x.sock"));
        for i in 0..(RECENT_CAP + 50) {
            bus.send(Envelope {
                id: format!("e{i}"),
                ts_ms: 0,
                from: "system:t".into(),
                to: "topic:t".into(),
                kind: "k".into(),
                payload: serde_json::Value::Null,
                trace_id: None,
            })
            .unwrap();
        }
        assert_eq!(bus.recent(RECENT_CAP + 10).len(), RECENT_CAP);
        // newest first
        assert_eq!(bus.recent(1)[0].id, format!("e{}", RECENT_CAP + 49));
    }
}
