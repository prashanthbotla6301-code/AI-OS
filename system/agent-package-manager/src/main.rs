//! AgentOS agent-package-manager (apm).
//!
//! Manages `.agent` packages: validate → install → register → uninstall.
//!
//! Phase 1 contract:
//! * packages are **directories** (`.agent` archives/zip + store downloads
//!   arrive Phase 2 — see docs/agent-format.md),
//! * installation is *copy + registry*: the package's code is NEVER executed,
//!   parsed only by the schema validator (defense in depth: validation errors
//!   can never lead to execution because nothing executes),
//! * the registry records a sha256 of manifest.json so tampering with an
//!   installed manifest is detectable (`apm.verify`) — groundwork for
//!   package signing in Phase 2.
//!
//! The daemon runs as root (it writes the system agent store); the runtime,
//! which merely *reads* installs, runs unprivileged.

use agentos_common::now_ms;
use agentos_ipc::{serve, Handler, RpcResponse};
use agentos_schema::{manifest_digest, validate_package_dir};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegistryEntry {
    pub id: String,
    pub version: String,
    /// Path relative to the agents root: `<id>/<version>`
    pub rel_path: String,
    pub installed_at_ms: u64,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RegistryFile {
    pub installed: Vec<RegistryEntry>,
}

pub struct Store {
    root: PathBuf, // <state>/agents
    registry_path: PathBuf,
    registry: RegistryFile,
    bus_socket: PathBuf,
    runtime_socket: PathBuf,
}

pub fn load_registry(path: &Path) -> RegistryFile {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let metadata = fs::symlink_metadata(&from)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("symbolic links are not installable: {}", from.display()),
            ));
        }
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to)?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("special files are not installable: {}", from.display()),
            ));
        }
    }
    Ok(())
}

impl Store {
    pub fn open(state_dir: &Path, run_dir: &Path) -> Store {
        let root = state_dir.join("agents");
        fs::create_dir_all(&root).ok();
        let registry_path = root.join("registry.json");
        let registry = load_registry(&registry_path);
        Store {
            root,
            registry_path,
            registry,
            bus_socket: run_dir.join(agentos_common::sockets::BUS),
            runtime_socket: run_dir.join(agentos_common::sockets::RUNTIME),
        }
    }

    fn save(&self) -> Result<(), String> {
        let body = serde_json::to_string_pretty(&self.registry).map_err(|e| e.to_string())?;
        agentos_common::atomic_write(&self.registry_path, body.as_bytes())
            .map_err(|e| e.to_string())
    }

    fn emit(&self, kind: &str, payload: serde_json::Value) {
        let _ = agentos_ipc::call(
            &self.bus_socket,
            "bus.send",
            serde_json::json!({"from": "system:apm", "to": "topic:packages", "kind": kind, "payload": payload}),
        );
    }

    pub fn list(&self) -> Vec<RegistryEntry> {
        self.registry.installed.clone()
    }

    /// Validate a package directory on disk (pure; no side effects).
    pub fn validate(&self, path: &Path) -> agentos_schema::ValidationReport {
        let mut report = validate_package_dir(path);
        if report.valid() {
            if let Ok(m) = agentos_schema::load_manifest(path) {
                if !m.permissions.is_empty() {
                    report.warning(format!(
                        "package requests {} permission(s): {}; none are granted at install time (deny-by-default)",
                        m.permissions.len(),
                        m.permissions.iter().map(|p| p.permission.as_str()).collect::<Vec<_>>().join(", ")
                    ));
                }
            }
        }
        report
    }

    pub fn install(&mut self, src: &Path) -> Result<RegistryEntry, Vec<String>> {
        let report = self.validate(src);
        if !report.valid() {
            return Err(report.errors);
        }
        let source =
            fs::canonicalize(src).map_err(|e| vec![format!("cannot resolve package path: {e}")])?;
        let store_root = fs::canonicalize(&self.root)
            .map_err(|e| vec![format!("cannot resolve agent store: {e}")])?;
        if source == store_root || source.starts_with(&store_root) {
            return Err(vec![
                "refusing to install a package from inside the AgentOS store".into(),
            ]);
        }
        let manifest = agentos_schema::load_manifest(src).map_err(|e| vec![e])?;
        let digest = manifest_digest(src).map_err(|e| vec![e])?;
        let rel = format!("{}/{}", manifest.id, manifest.version);
        let dst = self.root.join(&rel);
        if dst.exists() {
            fs::remove_dir_all(&dst).map_err(|e| vec![format!("reinstall failed: {e}")])?;
        }
        copy_dir_recursive(src, &dst).map_err(|e| vec![format!("copy failed: {e}")])?;
        let entry = RegistryEntry {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            rel_path: rel,
            installed_at_ms: now_ms(),
            manifest_sha256: digest,
        };
        self.registry
            .installed
            .retain(|e| !(e.id == entry.id && e.version == entry.version));
        self.registry.installed.push(entry.clone());
        self.save().map_err(|e| vec![e])?;
        self.emit(
            "agent.installed",
            serde_json::json!({"id": entry.id, "version": entry.version}),
        );
        agentos_common::info(&format!("installed {} {}", entry.id, entry.version));
        Ok(entry)
    }

    pub fn uninstall(&mut self, id: &str, version: Option<&str>) -> Result<usize, String> {
        // refuse to remove running agents
        if let Ok(v) = agentos_ipc::call(
            &self.runtime_socket,
            "agents.get",
            serde_json::json!({"id": id}),
        ) {
            if v["state"].as_str() == Some("Running") || v["state"].as_str() == Some("Starting") {
                return Err(format!(
                    "agent {id} is running; stop it before uninstalling"
                ));
            }
        }
        let matching: Vec<usize> = self
            .registry
            .installed
            .iter()
            .enumerate()
            .filter(|(_, e)| e.id == id && version.map(|v| e.version == v).unwrap_or(true))
            .map(|(i, _)| i)
            .collect();
        if matching.is_empty() {
            return Err(format!(
                "no installed package matches {id:?} {:?}",
                version.unwrap_or("(any)")
            ));
        }
        let mut removed = 0;
        for i in matching.iter().rev() {
            let e = self.registry.installed.remove(*i);
            let dir = self.root.join(&e.rel_path);
            fs::remove_dir_all(&dir).map_err(|err| format!("remove failed: {err}"))?;
            if let Some(parent) = dir.parent() {
                let empty = fs::read_dir(parent)
                    .map(|mut entries| entries.next().is_none())
                    .unwrap_or(false);
                if empty {
                    let _ = fs::remove_dir(parent);
                }
            }
            removed += 1;
        }
        self.save()?;
        self.emit("agent.uninstalled", serde_json::json!({"id": id}));
        Ok(removed)
    }

    /// Re-hash the installed manifest and compare against the registry.
    pub fn verify(&self, id: &str, version: Option<&str>) -> Result<serde_json::Value, String> {
        let e = self
            .registry
            .installed
            .iter()
            .find(|e| e.id == id && version.map(|v| e.version == v).unwrap_or(true))
            .ok_or_else(|| format!("no installed package matches {id:?}"))?;
        let dir = self.root.join(&e.rel_path);
        let current =
            manifest_digest(&dir).map_err(|err| format!("installed manifest unreadable: {err}"))?;
        Ok(serde_json::json!({
            "id": e.id,
            "version": e.version,
            "integrity": if current == e.manifest_sha256 { "ok" } else { "TAMPERED" },
            "expected": e.manifest_sha256,
            "actual": current,
        }))
    }
}

// ---------------------------------------------------------------------------
// Daemon + CLI (same binary: no args = daemon; subcommand = CLI)
// ---------------------------------------------------------------------------

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() > 1 && !argv[1].starts_with("--") {
        cli(&argv[1..]);
        return;
    }

    let paths = agentos_common::Paths::detect();
    let mut socket = paths.socket(agentos_common::sockets::APM);
    let mut state_dir = paths.state_dir;
    let mut run_dir = paths.run_dir;
    let mut args = argv.into_iter().skip(1).peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" => socket = args.next().map(PathBuf::from).unwrap_or_default(),
            "--state-dir" => state_dir = args.next().map(PathBuf::from).unwrap_or_default(),
            "--run-dir" => run_dir = args.next().map(PathBuf::from).unwrap_or_default(),
            _ => {}
        }
    }
    agentos_common::logger_init("agent-package-manager");
    agentos_common::info(
        "agent-package-manager starting (install validates and copies; never executes)",
    );

    let store = Arc::new(Mutex::new(Store::open(&state_dir, &run_dir)));

    let handler: Handler = Arc::new(move |req| {
        let p = &req.params;
        match req.method.as_str() {
            "ping" => RpcResponse::ok(
                req.id,
                serde_json::json!({"service":"agent-package-manager","version":agentos_common::VERSION}),
            ),
            "apm.list" => RpcResponse::ok(
                req.id,
                serde_json::json!({ "installed": store.lock().unwrap().list() }),
            ),
            "apm.validate" => {
                let path = p["path"].as_str().unwrap_or_default();
                let report = store.lock().unwrap().validate(Path::new(path));
                RpcResponse::ok(req.id, serde_json::to_value(report).unwrap_or_default())
            }
            "apm.install" => {
                let path = p["path"].as_str().unwrap_or_default().to_string();
                let mut s = store.lock().unwrap();
                match s.install(Path::new(&path)) {
                    Ok(e) => RpcResponse::ok(req.id, serde_json::to_value(e).unwrap_or_default()),
                    Err(errors) => {
                        RpcResponse::err(req.id, agentos_ipc::ERR_INVALID_PARAMS, errors.join("; "))
                    }
                }
            }
            "apm.uninstall" => {
                let id = p["id"].as_str().unwrap_or_default().to_string();
                let version = p["version"].as_str().map(str::to_string);
                let mut s = store.lock().unwrap();
                match s.uninstall(&id, version.as_deref()) {
                    Ok(n) => RpcResponse::ok(req.id, serde_json::json!({"removed": n})),
                    Err(e) => RpcResponse::err(req.id, agentos_ipc::ERR_INVALID_STATE, e),
                }
            }
            "apm.verify" => {
                let id = p["id"].as_str().unwrap_or_default().to_string();
                let version = p["version"].as_str().map(str::to_string);
                match store.lock().unwrap().verify(&id, version.as_deref()) {
                    Ok(v) => RpcResponse::ok(req.id, v),
                    Err(e) => RpcResponse::err(req.id, agentos_ipc::ERR_NOT_FOUND, e),
                }
            }
            _ => RpcResponse::err(
                req.id,
                agentos_ipc::ERR_UNKNOWN_METHOD,
                format!("unknown method {}", req.method),
            ),
        }
    });

    if let Err(e) = serve(&socket, handler) {
        agentos_common::error(&format!("agent-package-manager fatal: {e}"));
        std::process::exit(1);
    }
}

fn cli(args: &[String]) {
    let paths = agentos_common::Paths::detect();
    let socket = paths.socket(agentos_common::sockets::APM);
    let call = |method: &str, params: serde_json::Value| -> serde_json::Value {
        agentos_ipc::call(&socket, method, params)
            .unwrap_or_else(|e| serde_json::json!({"error": e.message}))
    };
    match args[0].as_str() {
        "list" => println!(
            "{}",
            serde_json::to_string_pretty(&call("apm.list", serde_json::json!({}))).unwrap()
        ),
        "validate" if args.len() > 1 => println!(
            "{}",
            serde_json::to_string_pretty(&call(
                "apm.validate",
                serde_json::json!({"path": args[1]})
            ))
            .unwrap()
        ),
        "install" if args.len() > 1 => println!(
            "{}",
            serde_json::to_string_pretty(&call(
                "apm.install",
                serde_json::json!({"path": args[1]})
            ))
            .unwrap()
        ),
        "uninstall" if args.len() > 1 => println!(
            "{}",
            serde_json::to_string_pretty(&call(
                "apm.uninstall",
                serde_json::json!({"id": args[1]})
            ))
            .unwrap()
        ),
        "verify" if args.len() > 1 => println!(
            "{}",
            serde_json::to_string_pretty(&call("apm.verify", serde_json::json!({"id": args[1]})))
                .unwrap()
        ),
        _ => {
            eprintln!("AgentOS package manager CLI");
            eprintln!("usage: agentos-apm [list | validate <dir> | install <dir> | uninstall <id> | verify <id>]");
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(dir: &Path, id: &str, version: &str) -> PathBuf {
        let p = dir.join(format!("{id}-{version}.agent"));
        fs::create_dir_all(p.join("skills")).unwrap();
        fs::write(
            p.join("manifest.json"),
            format!(r#"{{"format":"agentos","format_version":"1.0.0","id":"{id}","name":"Fix","version":"{version}","runtime":{{"engine":"mock"}},"permissions":[],"capabilities":["fix"]}}"#),
        )
        .unwrap();
        fs::write(p.join("agent.json"), r#"{"entry":{"kind":"builtin:echo"}}"#).unwrap();
        fs::write(p.join("permissions.json"), r#"{"permissions":[]}"#).unwrap();
        fs::write(
            p.join("skills/README.md"),
            "# skills\nPlace skill descriptors here (Phase 2).\n",
        )
        .unwrap();
        p
    }

    fn store(dir: &Path) -> Store {
        Store::open(dir, dir)
    }

    #[test]
    fn install_list_verify_uninstall_roundtrip() {
        let dir = std::env::temp_dir().join(format!("agentos-apm-rt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let pkg = fixture(&dir, "com.test.agent", "1.0.0");
        let mut s = store(&dir);
        let entry = s.install(&pkg).unwrap();
        assert_eq!(entry.id, "com.test.agent");
        assert_eq!(s.list().len(), 1);
        assert!(dir
            .join("agents/com.test.agent/1.0.0/manifest.json")
            .is_file());
        // verify integrity ok
        let v = s.verify("com.test.agent", None).unwrap();
        assert_eq!(v["integrity"], "ok");
        // tamper
        fs::write(dir.join("agents/com.test.agent/1.0.0/manifest.json"), r#"{"format":"agentos","format_version":"1.0.0","id":"com.test.agent","name":"Tampered","version":"1.0.0"}"#).unwrap();
        let v = s.verify("com.test.agent", None).unwrap();
        assert_eq!(v["integrity"], "TAMPERED");
        // uninstall (runtime socket doesn't exist in unit test -> treated as not running)
        let n = s.uninstall("com.test.agent", None).unwrap();
        assert_eq!(n, 1);
        assert!(!dir.join("agents/com.test.agent").exists());
        assert!(s.uninstall("com.test.agent", None).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_package_rejected_with_errors() {
        let dir = std::env::temp_dir().join(format!("agentos-apm-bad-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.agent");
        fs::create_dir_all(&bad).unwrap();
        fs::write(
            bad.join("manifest.json"),
            r#"{"format":"agentos","format_version":"9.0","id":"nope","name":"","version":"x"}"#,
        )
        .unwrap();
        let mut s = store(&dir);
        let errs = s.install(&bad).unwrap_err();
        assert!(!errs.is_empty());
        assert_eq!(s.list().len(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validation_warns_about_requested_permissions() {
        let dir = std::env::temp_dir().join(format!("agentos-apm-warn-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = fixture(&dir, "com.test.net", "1.0.0");
        // add a network request to both manifest and permissions.json
        let m = fs::read_to_string(p.join("manifest.json"))
            .unwrap()
            .replace(
                "\"permissions\":[]",
                "\"permissions\":[{\"permission\":\"network\"}]",
            );
        fs::write(p.join("manifest.json"), m).unwrap();
        fs::write(
            p.join("permissions.json"),
            r#"{"permissions":[{"permission":"network","reason":"needs web"}]}"#,
        )
        .unwrap();
        let s = store(&dir);
        let report = s.validate(&p);
        assert!(report.valid(), "errors: {:?}", report.errors);
        assert!(report.warnings.iter().any(|w| w.contains("network")));
        let _ = fs::remove_dir_all(&dir);
    }
}
