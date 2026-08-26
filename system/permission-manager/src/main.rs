//! AgentOS permission-manager.
//!
//! The single authority for the fine-grained AgentOS permission model.
//!
//! Model (Phase 1):
//! * 11 well-known permissions (see `agentos_schema::permission`).
//! * Default policy loaded from `/etc/agentos/permissions-default.json`
//!   (shipped: everything DENY — dangerous permissions are disabled by
//!   default by construction).
//! * Grants live in `/var/lib/agentos/permissions/grants.json` and are the
//!   only way an agent obtains anything. Phase 1 grants can only be created
//!   by the local operator via IPC/CLI (roadmap: signed consent flows).
//! * `check(subject, permission, access)` is the hot path used by
//!   agent-runtime (before an agent may run) and agent-bus (before an agent
//!   may invoke another agent).
//!
//! Phase 1 limitations (deliberate, documented):
//! no per-resource grants beyond an optional path-prefix, no expiry
//! enforcement worker (field parsed, enforced on read), no GUI consent flow.

use agentos_ipc::{serve, Handler, RpcResponse};
use agentos_schema::permission as perm;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Types (shared contract; mirrored in docs/security.md)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny,
    /// The operation would be allowed *if the user consents*. Phase 1 treats
    /// Ask as blocked (there is no consent UI yet) and says so.
    Ask,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Grant {
    /// Subject: an agent id (`com.vendor.agent`) or `system:<service>`.
    pub subject: String,
    pub permission: String,
    #[serde(default = "default_access")]
    pub access: String,
    /// Optional resource scope, e.g. a directory prefix for filesystem.read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    pub granted_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn default_access() -> String {
    "write".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PolicyFile {
    default: Decision,
    #[serde(default)]
    permissions: std::collections::BTreeMap<String, PolicyEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PolicyEntry {
    default: Decision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

impl Default for PolicyFile {
    fn default() -> Self {
        // Deny-by-default for every known permission. This map is the
        // authoritative Phase 1 posture; the shipped /etc file mirrors it.
        let mut permissions = std::collections::BTreeMap::new();
        for p in perm::ALL {
            permissions.insert(
                p.to_string(),
                PolicyEntry {
                    default: Decision::Deny,
                    description: Some(
                        "dangerous by default; requires explicit operator grant".into(),
                    ),
                },
            );
        }
        PolicyFile {
            default: Decision::Deny,
            permissions,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub decision: Decision,
    pub reason: String,
}

pub struct State {
    policy: PolicyFile,
    grants: Vec<Grant>,
    grants_path: PathBuf,
}

pub struct Shared(Mutex<State>);

/// A write grant covers the corresponding read capability, but not vice versa.
/// Filesystem and memory intentionally use separate permission names so a
/// package can request/read-only declare them precisely on the wire.
fn permission_grant_satisfies(granted: &str, requested: &str) -> bool {
    granted == requested
        || (granted == perm::FILESYSTEM_WRITE && requested == perm::FILESYSTEM_READ)
        || (granted == perm::MEMORY_WRITE && requested == perm::MEMORY_READ)
}

fn valid_resource(resource: Option<&str>) -> bool {
    resource
        .map(|value| {
            !value.is_empty()
                && value.starts_with('/')
                && !value.split('/').any(|part| part == "..")
                && value.len() <= 4096
        })
        .unwrap_or(true)
}

fn valid_subject(subject: &str) -> bool {
    let value = subject
        .strip_prefix("agent:")
        .or_else(|| subject.strip_prefix("system:"))
        .unwrap_or(subject);
    !value.is_empty()
        && value.len() <= 200
        && subject
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
}

impl Shared {
    pub fn new(state_dir: &std::path::Path, etc_dir: &std::path::Path) -> Shared {
        let dir = state_dir.join("permissions");
        let _ = fs::create_dir_all(&dir);
        let grants_path = dir.join("grants.json");
        let policy_path = etc_dir.join("permissions-default.json");

        let policy = fs::read_to_string(&policy_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        let grants = fs::read_to_string(&grants_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        Shared(Mutex::new(State {
            policy,
            grants,
            grants_path,
        }))
    }

    pub fn check(
        &self,
        subject: &str,
        permission: &str,
        access: &str,
        resource: Option<&str>,
    ) -> Result<CheckResult, String> {
        if !perm::is_known(permission) {
            return Err(format!("unknown permission {permission:?}"));
        }
        if !agentos_schema::access::is_valid(access) {
            return Err(format!(
                "invalid access {access:?} (want \"read\" or \"write\")"
            ));
        }
        let st = self.0.lock().unwrap();
        // 1. Grant lookup — an explicit grant satisfies if access level and
        //    optional resource scope match. `write` grant satisfies `read`.
        for g in &st.grants {
            if g.subject == subject
                && permission_grant_satisfies(&g.permission, permission)
                && agentos_schema::access::satisfies(&g.access, access)
            {
                match (&g.resource, resource) {
                    (None, _) => {
                        return Ok(CheckResult {
                            decision: Decision::Allow,
                            reason: format!("granted to {} at {} access", g.subject, g.access),
                        })
                    }
                    (Some(prefix), Some(res))
                        if res == prefix || res.starts_with(&format!("{prefix}/")) =>
                    {
                        return Ok(CheckResult {
                            decision: Decision::Allow,
                            reason: format!("granted for resource prefix {prefix}"),
                        })
                    }
                    _ => {}
                }
            }
        }
        // 2. Fall back to policy default.
        let entry = st.policy.permissions.get(permission);
        let decision = entry.map(|e| e.default).unwrap_or(st.policy.default);
        let reason = match decision {
            Decision::Allow => "allowed by system policy".to_string(),
            Decision::Ask => format!("permission {permission} requires user consent, which is not available in Phase 1; treat as blocked"),
            Decision::Deny => format!("no grant for subject {subject:?}; permission {permission} is deny-by-default"),
        };
        Ok(CheckResult { decision, reason })
    }

    pub fn grant(&self, g: Grant) -> Result<(), String> {
        if !valid_subject(&g.subject) {
            return Err("subject must be an agent id, agent:<id>, or system:<service>".into());
        }
        if !perm::is_known(&g.permission) {
            return Err(format!("unknown permission {:?}", g.permission));
        }
        if !agentos_schema::access::is_valid(&g.access) {
            return Err("access must be read|write".into());
        }
        if !valid_resource(g.resource.as_deref()) {
            return Err("resource must be an absolute path without '..'".into());
        }
        let mut st = self.0.lock().unwrap();
        st.grants.retain(|x| {
            !(x.subject == g.subject && x.permission == g.permission && x.resource == g.resource)
        });
        st.grants.push(g);
        st.save()
    }

    pub fn revoke(&self, subject: &str, permission: &str) -> Result<usize, String> {
        if !valid_subject(subject) || !perm::is_known(permission) {
            return Err("invalid subject or unknown permission".into());
        }
        let mut st = self.0.lock().unwrap();
        let before = st.grants.len();
        st.grants
            .retain(|g| !(g.subject == subject && g.permission == permission));
        let removed = before - st.grants.len();
        st.save()?;
        Ok(removed)
    }

    pub fn grants(&self) -> Vec<Grant> {
        self.0.lock().unwrap().grants.clone()
    }

    pub fn policy_summary(&self) -> serde_json::Value {
        let st = self.0.lock().unwrap();
        let mut m = serde_json::Map::new();
        for p in perm::ALL {
            let d = st
                .policy
                .permissions
                .get(*p)
                .map(|e| e.default)
                .unwrap_or(st.policy.default);
            m.insert(p.to_string(), serde_json::json!({ "default": d }));
        }
        serde_json::json!({ "default": st.policy.default, "permissions": m })
    }
}

impl State {
    fn save(&mut self) -> Result<(), String> {
        let body = serde_json::to_string_pretty(&self.grants).map_err(|e| e.to_string())?;
        agentos_common::atomic_write(&self.grants_path, body.as_bytes()).map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

fn main() {
    let mut socket = agentos_common::Paths::detect().socket(agentos_common::sockets::PERMISSIONS);
    let mut state_dir = agentos_common::Paths::detect().state_dir;
    let mut etc_dir = agentos_common::Paths::detect().etc_dir;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from).unwrap_or_default(),
            "--state-dir" => state_dir = args.next().map(PathBuf::from).unwrap_or_default(),
            "--etc-dir" => etc_dir = args.next().map(PathBuf::from).unwrap_or_default(),
            _ => {}
        }
    }
    agentos_common::logger_init("permission-manager");
    agentos_common::info("permission-manager starting (deny-by-default)");

    let shared = std::sync::Arc::new(Shared::new(&state_dir, &etc_dir));

    let handler: Handler = {
        let shared = shared.clone();
        Arc::new(move |req| {
            let p = &req.params;
            match req.method.as_str() {
                "ping" => RpcResponse::ok(
                    req.id,
                    serde_json::json!({"service":"permission-manager","version":agentos_common::VERSION}),
                ),
                "permissions.list" => {
                    RpcResponse::ok(req.id, serde_json::json!({ "permissions": perm::ALL }))
                }
                "policy.get" => RpcResponse::ok(req.id, shared.policy_summary()),
                "grants.list" => {
                    RpcResponse::ok(req.id, serde_json::json!({ "grants": shared.grants() }))
                }
                "grants.grant" => {
                    let subject = p["subject"].as_str().unwrap_or("").to_string();
                    let permission = p["permission"].as_str().unwrap_or("").to_string();
                    if subject.is_empty() || permission.is_empty() {
                        return RpcResponse::err(
                            req.id,
                            agentos_ipc::ERR_INVALID_PARAMS,
                            "subject and permission required",
                        );
                    }
                    let g = Grant {
                        subject,
                        permission,
                        access: p["access"].as_str().unwrap_or("write").to_string(),
                        resource: p["resource"].as_str().map(str::to_string),
                        granted_at_ms: agentos_common::now_ms(),
                        note: p["note"].as_str().map(str::to_string),
                    };
                    match shared.grant(g) {
                        Ok(()) => RpcResponse::ok(req.id, serde_json::json!({"granted":true})),
                        Err(e) => RpcResponse::err(req.id, agentos_ipc::ERR_INVALID_PARAMS, e),
                    }
                }
                "grants.revoke" => {
                    let subject = p["subject"].as_str().unwrap_or_default();
                    let permission = p["permission"].as_str().unwrap_or_default();
                    match shared.revoke(subject, permission) {
                        Ok(n) => RpcResponse::ok(req.id, serde_json::json!({"revoked":n})),
                        Err(e) => RpcResponse::err(req.id, agentos_ipc::ERR_INTERNAL, e),
                    }
                }
                "permissions.check" => {
                    let subject = p["subject"].as_str().unwrap_or_default();
                    let permission = p["permission"].as_str().unwrap_or_default();
                    let access = p["access"].as_str().unwrap_or("write");
                    let resource = p["resource"].as_str();
                    match shared.check(subject, permission, access, resource) {
                        Ok(r) => {
                            RpcResponse::ok(req.id, serde_json::to_value(r).unwrap_or_default())
                        }
                        Err(e) => RpcResponse::err(req.id, agentos_ipc::ERR_INVALID_PARAMS, e),
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
        agentos_common::error(&format!("permission-manager fatal: {e}"));
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

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("agentos-perm-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn deny_by_default_with_no_grants() {
        let dir = tmp("deny");
        let s = Shared::new(&dir, &dir);
        let r = s
            .check("com.evil.agent", perm::NETWORK, "write", None)
            .unwrap();
        assert_eq!(r.decision, Decision::Deny);
        assert!(r.reason.contains("deny-by-default"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn grant_then_allow_and_revoke() {
        let dir = tmp("grant");
        let s = Shared::new(&dir, &dir);
        s.grant(Grant {
            subject: "com.good.agent".into(),
            permission: perm::FILESYSTEM_READ.into(),
            access: "read".into(),
            resource: Some("/home/agent/downloads".into()),
            granted_at_ms: 1,
            note: None,
        })
        .unwrap();
        let r = s
            .check(
                "com.good.agent",
                perm::FILESYSTEM_READ,
                "read",
                Some("/home/agent/downloads/file.txt"),
            )
            .unwrap();
        assert_eq!(r.decision, Decision::Allow);
        // outside the resource prefix -> deny
        let r = s
            .check(
                "com.good.agent",
                perm::FILESYSTEM_READ,
                "read",
                Some("/etc/passwd"),
            )
            .unwrap();
        assert_eq!(r.decision, Decision::Deny);
        // write access not covered by a read grant
        let r = s
            .check(
                "com.good.agent",
                perm::FILESYSTEM_WRITE,
                "write",
                Some("/home/agent/downloads/x"),
            )
            .unwrap();
        assert_eq!(r.decision, Decision::Deny);
        // persistence round-trip
        let s2 = Shared::new(&dir, &dir);
        assert_eq!(s2.grants().len(), 1);
        s2.revoke("com.good.agent", perm::FILESYSTEM_READ).unwrap();
        let r = s2
            .check(
                "com.good.agent",
                perm::FILESYSTEM_READ,
                "read",
                Some("/home/agent/downloads"),
            )
            .unwrap();
        assert_eq!(r.decision, Decision::Deny);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_grant_implies_read() {
        let dir = tmp("implies");
        let s = Shared::new(&dir, &dir);
        s.grant(Grant {
            subject: "com.w.agent".into(),
            permission: perm::MEMORY_WRITE.into(),
            access: "write".into(),
            resource: None,
            granted_at_ms: 1,
            note: None,
        })
        .unwrap();
        let r = s
            .check("com.w.agent", perm::MEMORY_READ, "read", None)
            .unwrap();
        assert_eq!(r.decision, Decision::Allow);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_permission_is_an_error_not_a_deny() {
        let dir = tmp("unknown");
        let s = Shared::new(&dir, &dir);
        assert!(s.check("x", "filesystem.root", "write", None).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn shipped_policy_file_denies_everything() {
        // The etc/ policy shipped in the OS image must be deny-by-default.
        let dir = tmp("policy");
        let s = Shared::new(&dir, &dir);
        let summary = s.policy_summary();
        for p in perm::ALL {
            assert_eq!(
                summary["permissions"][p]["default"],
                serde_json::json!("deny"),
                "{p} must default to deny"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
