//! AgentOS package format v1 — canonical types and validation.
//!
//! An `.agent` package is a directory:
//!
//! ```text
//! video-editor.agent/
//!   manifest.json      <- required, validated here (schema: manifest-v1)
//!   agent.json         <- required, runtime entry/interface/sandbox spec
//!   permissions.json   <- required, explicit permission declarations
//!   runtime/           <- engine assets (descriptors; the `native` engine
//!                         declares its deterministic task inline instead)
//!   tools/  skills/  workflows/  models/  ui/  assets/
//! ```
//!
//! Format rules (enforced here and mirrored in packages/schema/*.schema.json):
//! * unknown permissions are a hard error,
//! * `runtime.engine` may be `mock` | `native` | `process` | `wasm`;
//!   `native` (Phase 2A) executes a declarative, deterministic task spec
//!   *inside* the agent-runtime sandbox (see [`native`]); `mock` is simulated
//!   by the runtime without executing a payload; `process`/`wasm` install but
//!   fail closed until their sandboxed engines ship,
//! * `native` packages must carry a parseable `native.task` entry — a
//!   malformed task rejects the whole package,
//! * manifests are validated structurally; nothing here trusts the package.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

pub const FORMAT_NAME: &str = "agentos";
pub const FORMAT_VERSION_MAJOR: u64 = 1;
pub const FORMAT_VERSION_MINOR: u64 = 0;

/// The fine-grained permission namespace (Phase 1 set, from the AgentOS
/// security architecture). These strings are the wire/manifest identifiers.
pub mod permission {
    pub const FILESYSTEM_READ: &str = "filesystem.read";
    pub const FILESYSTEM_WRITE: &str = "filesystem.write";
    pub const NETWORK: &str = "network";
    pub const MICROPHONE: &str = "microphone";
    pub const CAMERA: &str = "camera";
    pub const GPU_COMPUTE: &str = "gpu.compute";
    pub const SYSTEM_SETTINGS: &str = "system.settings";
    pub const PROCESS_EXECUTE: &str = "process.execute";
    pub const AGENT_INVOKE: &str = "agent.invoke";
    pub const MEMORY_READ: &str = "memory.read";
    pub const MEMORY_WRITE: &str = "memory.write";

    pub const ALL: &[&str] = &[
        FILESYSTEM_READ,
        FILESYSTEM_WRITE,
        NETWORK,
        MICROPHONE,
        CAMERA,
        GPU_COMPUTE,
        SYSTEM_SETTINGS,
        PROCESS_EXECUTE,
        AGENT_INVOKE,
        MEMORY_READ,
        MEMORY_WRITE,
    ];

    pub fn is_known(p: &str) -> bool {
        ALL.contains(&p)
    }
}

/// Access levels a permission can be requested/granted at. `write` implies
/// `read` for filesystem/memory-style permissions.
pub mod access {
    pub const READ: &str = "read";
    pub const WRITE: &str = "write";
    pub fn is_valid(a: &str) -> bool {
        a == READ || a == WRITE
    }
    /// Does `granted` satisfy `requested`?
    pub fn satisfies(granted: &str, requested: &str) -> bool {
        if granted == requested {
            return true;
        }
        granted == WRITE && requested == READ
    }
}

/// Execution engines the runtime understands. `native` (Phase 2A) executes a
/// declarative task inside the sandboxed agent runtime; `mock` is simulated
/// by the runtime without executing a payload; `process` and `wasm` install
/// and register but fail closed until their sandboxed engines exist.
pub mod engine {
    pub const MOCK: &str = "mock";
    pub const NATIVE: &str = "native";
    pub const PROCESS: &str = "process";
    pub const WASM: &str = "wasm";
    pub const ALL: &[&str] = &[MOCK, NATIVE, PROCESS, WASM];
    pub fn is_known(e: &str) -> bool {
        ALL.contains(&e)
    }
    /// Engines for which agent-runtime has an executor in this release.
    /// Everything else fails closed at start time.
    pub fn is_executable(e: &str) -> bool {
        e == MOCK || e == NATIVE
    }
}

// ---------------------------------------------------------------------------
// Native engine task spec (`native` packages)
// ---------------------------------------------------------------------------

/// The native engine: a deterministic, declarative task executed inside the
/// agent-runtime process. A task is a bounded list of pure operations — there
/// are no I/O, process, network, or model primitives, so a native task can
/// never reach outside the runtime sandbox. Results are announced on the
/// agent bus, which requires the `agent.invoke` permission like any other
/// agent-originated bus use.
pub mod native {
    use serde::{Deserialize, Serialize};

    /// `entry.kind` identifier for native packages.
    pub const TASK_KIND: &str = "native.task";
    /// Hard cap on operations per task (install-time and run-time).
    pub const MAX_TASK_OPS: usize = 64;
    /// Hard cap on the size of a task's text output, in bytes.
    pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;

    /// One deterministic operation. `say` appends a fixed message to the
    /// task output; `fail` deterministically aborts the task (used to model
    /// and test execution failure paths). The set is deliberately tiny.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    #[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
    pub enum Op {
        Say {
            /// Message must be present and bounded (<= 4 KiB).
            message: String,
        },
        Fail {
            /// Deterministic failure reason surfaced in the agent record.
            message: String,
        },
    }

    /// The task body: `entry = {"kind": "native.task", "task": {"ops": [...]}}`.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    pub struct Task {
        pub ops: Vec<Op>,
    }

    /// The full native entry descriptor.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    pub struct Entry {
        pub kind: String,
        pub task: Task,
    }

    /// Parse and structurally validate a native entry. Enforces the op-count
    /// and message-size budgets so validation (not just execution) rejects
    /// oversized or malformed tasks.
    pub fn parse_entry(entry: &serde_json::Value) -> Result<Entry, String> {
        let e: Entry = serde_json::from_value(entry.clone())
            .map_err(|err| format!("entry is not a valid {TASK_KIND:?}: {err}"))?;
        if e.kind != TASK_KIND {
            return Err(format!(
                "entry.kind must be {TASK_KIND:?} (got {:?})",
                e.kind
            ));
        }
        if e.task.ops.is_empty() {
            return Err("native task must contain at least one op".into());
        }
        if e.task.ops.len() > MAX_TASK_OPS {
            return Err(format!(
                "native task exceeds {MAX_TASK_OPS} ops (got {})",
                e.task.ops.len()
            ));
        }
        for op in &e.task.ops {
            let message = match op {
                Op::Say { message } | Op::Fail { message } => message,
            };
            if message.is_empty() {
                return Err("native op message must not be empty".into());
            }
            if message.len() > 4096 {
                return Err("native op message must be at most 4096 bytes".into());
            }
        }
        Ok(e)
    }

    /// Is `entry` a native entry at all (kind check only, no full parse)?
    pub fn is_native_entry(entry: &serde_json::Value) -> bool {
        entry.get("kind").and_then(|k| k.as_str()) == Some(TASK_KIND)
    }
}

// ---------------------------------------------------------------------------
// Manifest (manifest.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Developer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RuntimeSpec {
    /// One of [`engine::ALL`]; defaults to `mock`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// Engine-specific entry descriptor (kept opaque at the schema level).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<serde_json::Value>,
    /// Minimum AgentOS version this agent supports (semver).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_agentos: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PermissionRequest {
    pub permission: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub format: String,
    pub format_version: String,
    /// Reverse-DNS identifier, e.g. `com.agentos.hello`.
    pub id: String,
    pub name: String,
    /// Semver `MAJOR.MINOR.PATCH`.
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub developer: Developer,
    #[serde(default)]
    pub runtime: RuntimeSpec,
    #[serde(default)]
    pub permissions: Vec<PermissionRequest>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

// ---------------------------------------------------------------------------
// agent.json + permissions.json
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SandboxSpec {
    #[serde(default)]
    pub fs: Vec<String>,
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub exec: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    #[serde(default)]
    pub entry: serde_json::Value,
    #[serde(default)]
    pub interface: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PermissionsFile {
    #[serde(default)]
    pub permissions: Vec<PermissionRequest>,
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ValidationReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl ValidationReport {
    pub fn valid(&self) -> bool {
        self.errors.is_empty()
    }
    pub fn error(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }
    pub fn warning(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }
    pub fn merge(&mut self, other: ValidationReport) {
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
    }
}

fn valid_agent_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 200 {
        return false;
    }
    // scheme-less reverse-DNS: lowercase segments separated by dots,
    // at least two segments, each 1..63 of [a-z0-9_-], no leading/trailing '-'.
    let segments: Vec<&str> = id.split('.').collect();
    if segments.len() < 2 {
        return false;
    }
    segments.iter().all(|s| {
        !s.is_empty()
            && s.len() <= 63
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
            && !s.starts_with('-')
            && !s.ends_with('-')
    })
}

fn valid_capability(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 64
        && c.chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '.' || ch == '-')
}

/// The package format deliberately uses numeric `MAJOR.MINOR.PATCH` strings.
/// `semver::Version` also accepts pre-releases/build metadata, but accepting
/// those here would disagree with the published JSON schema.
fn strict_numeric_semver(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        && semver::Version::parse(value).is_ok()
}

/// Validate a parsed manifest structurally. Returns errors (blocking) and
/// warnings (non-blocking).
pub fn validate_manifest(m: &Manifest) -> ValidationReport {
    let mut r = ValidationReport::default();
    if m.format != FORMAT_NAME {
        r.error(format!(
            "manifest.format must be \"{}\" (got {:?})",
            FORMAT_NAME, m.format
        ));
    }
    match semver::Version::parse(&m.format_version) {
        Ok(v) if strict_numeric_semver(&m.format_version) && v.major == FORMAT_VERSION_MAJOR => {
            if v.minor > FORMAT_VERSION_MINOR {
                r.warning(format!("format_version {}.{} is newer than supported {}.{}; validating with forward tolerance", v.major, v.minor, FORMAT_VERSION_MAJOR, FORMAT_VERSION_MINOR));
            }
        }
        _ => r.error(format!("manifest.format_version must be numeric semver MAJOR.MINOR.PATCH with major={FORMAT_VERSION_MAJOR} (got {:?})", m.format_version)),
    }
    if !valid_agent_id(&m.id) {
        r.error(format!(
            "manifest.id must be reverse-DNS, e.g. \"com.vendor.agent\" (got {:?})",
            m.id
        ));
    }
    if m.name.trim().is_empty() || m.name.len() > 100 {
        r.error(format!(
            "manifest.name must be 1..100 chars (got {:?})",
            m.name
        ));
    }
    if !strict_numeric_semver(&m.version) {
        r.error(format!(
            "manifest.version must be numeric semver MAJOR.MINOR.PATCH (got {:?})",
            m.version
        ));
    }
    if let Some(min) = &m.runtime.min_agentos {
        if !strict_numeric_semver(min) {
            r.error(format!(
                "runtime.min_agentos must be numeric semver MAJOR.MINOR.PATCH (got {:?})",
                min
            ));
        }
    }
    for p in &m.permissions {
        if !permission::is_known(&p.permission) {
            r.error(format!(
                "unknown permission {:?} (known: {:?})",
                p.permission,
                permission::ALL
            ));
        }
        if let Some(a) = &p.access {
            if !access::is_valid(a) {
                r.error(format!(
                    "permission {}.access must be \"read\" or \"write\" (got {:?})",
                    p.permission, a
                ));
            }
        }
    }
    for c in &m.capabilities {
        if !valid_capability(c) {
            r.error(format!(
                "invalid capability {:?} (lowercase letters, digits, '.', '-', max 64)",
                c
            ));
        }
    }
    if let Some(entry) = &m.runtime.entry {
        if !entry.is_object() {
            r.error("runtime.entry must be an object");
        }
    }
    match &m.runtime.engine {
        None => r.warning("runtime.engine missing; defaulting to \"mock\""),
        Some(e) if !engine::is_known(e) => r.error(format!("unknown runtime.engine {:?} (known: {:?})", e, engine::ALL)),
        Some(e) if !engine::is_executable(e) => r.warning(format!("runtime.engine {e:?} installs but will NOT execute until its sandboxed engine ships (executable now: \"mock\", \"native\")")),
        _ => {}
    }
    r
}

const MAX_PACKAGE_FILES: usize = 4096;
const MAX_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;

/// Walk the package without following links. A package is data in Phase 1;
/// accepting symlinks or device nodes would let an install copy escape its
/// source tree or introduce special files into the agent store.
fn validate_package_tree(path: &Path, report: &mut ValidationReport) {
    let mut stack = vec![path.to_path_buf()];
    let mut files = 0usize;
    let mut bytes = 0u64;
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                report.error(format!(
                    "cannot read package directory {}: {e}",
                    dir.display()
                ));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    report.error(format!(
                        "cannot inspect package entry in {}: {e}",
                        dir.display()
                    ));
                    continue;
                }
            };
            let item = entry.path();
            let metadata = match fs::symlink_metadata(&item) {
                Ok(metadata) => metadata,
                Err(e) => {
                    report.error(format!(
                        "cannot inspect package entry {}: {e}",
                        item.display()
                    ));
                    continue;
                }
            };
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                report.error(format!(
                    "symbolic links are not allowed in packages: {}",
                    item.display()
                ));
                continue;
            }
            if file_type.is_dir() {
                stack.push(item);
                continue;
            }
            if !file_type.is_file() {
                report.error(format!(
                    "special files are not allowed in packages: {}",
                    item.display()
                ));
                continue;
            }
            files += 1;
            bytes = bytes.saturating_add(metadata.len());
            if files > MAX_PACKAGE_FILES {
                report.error(format!(
                    "package contains more than {MAX_PACKAGE_FILES} files"
                ));
                return;
            }
            if bytes > MAX_PACKAGE_BYTES {
                report.error(format!(
                    "package exceeds the {MAX_PACKAGE_BYTES} byte Phase 1 limit"
                ));
                return;
            }
        }
    }
}

/// Validate a package directory on disk: required files present, JSON parses,
/// manifests structurally valid, permissions coherent.
/// Does NOT execute or trust anything inside the package.
pub fn validate_package_dir(path: &Path) -> ValidationReport {
    let mut r = ValidationReport::default();
    let root_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => {
            r.error(format!(
                "package path is not a directory: {}",
                path.display()
            ));
            return r;
        }
    };
    if !root_metadata.file_type().is_dir() {
        r.error(format!(
            "package path is not a directory: {}",
            path.display()
        ));
        return r;
    }
    validate_package_tree(path, &mut r);
    let manifest_path = path.join("manifest.json");
    if !manifest_path.is_file() {
        r.error("missing manifest.json");
        return r;
    }
    let manifest_str = match fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(e) => {
            r.error(format!("cannot read manifest.json: {e}"));
            return r;
        }
    };
    let manifest: Manifest = match serde_json::from_str(&manifest_str) {
        Ok(m) => m,
        Err(e) => {
            r.error(format!(
                "manifest.json is not valid JSON for the schema: {e}"
            ));
            return r;
        }
    };
    r.merge(validate_manifest(&manifest));

    let agent_path = path.join("agent.json");
    if !agent_path.is_file() {
        r.error("missing agent.json");
    } else {
        match fs::read_to_string(&agent_path).ok().and_then(|s| {
            let value: serde_json::Value = serde_json::from_str(&s).ok()?;
            if !value
                .get("entry")
                .map(|entry| entry.is_object())
                .unwrap_or(false)
            {
                return None;
            }
            serde_json::from_value::<AgentSpec>(value).ok()
        }) {
            Some(spec) => {
                // A native package must carry a fully valid native entry: the
                // task is the executable contract, so a malformed one is an
                // install-blocking error, not a runtime surprise.
                if manifest.runtime.engine.as_deref() == Some(engine::NATIVE) {
                    if let Err(e) = native::parse_entry(&spec.entry) {
                        r.error(format!("agent.json: {e}"));
                    }
                }
            }
            None => {
                r.error("agent.json is not valid JSON for the schema (entry object is required)")
            }
        }
    }

    let perms_path = path.join("permissions.json");
    let mut perms_file = PermissionsFile::default();
    if !perms_path.is_file() {
        r.error("missing permissions.json");
    } else {
        match fs::read_to_string(&perms_path).ok().and_then(|s| {
            let value: serde_json::Value = serde_json::from_str(&s).ok()?;
            if !value
                .get("permissions")
                .map(|permissions| permissions.is_array())
                .unwrap_or(false)
            {
                return None;
            }
            serde_json::from_value::<PermissionsFile>(value).ok()
        }) {
            Some(pf) => perms_file = pf,
            None => r.error(
                "permissions.json is not valid JSON for the schema (permissions array is required)",
            ),
        }
    }
    for p in &perms_file.permissions {
        if !permission::is_known(&p.permission) {
            r.error(format!(
                "permissions.json: unknown permission {:?}",
                p.permission
            ));
        }
    }
    // Every manifest permission must be declared in permissions.json too —
    // the explicit file is the authoritative declaration surface.
    let declared: std::collections::BTreeSet<String> = perms_file
        .permissions
        .iter()
        .map(|p| p.permission.clone())
        .collect();
    for p in &manifest.permissions {
        if !declared.contains(&p.permission) {
            r.error(format!(
                "manifest requests permission {:?} not declared in permissions.json",
                p.permission
            ));
        }
    }
    if perms_file.permissions.is_empty() && !manifest.permissions.is_empty() {
        // unreachable given the check above, kept as belt-and-braces
        r.error("permissions.json is empty but manifest requests permissions");
    }
    r
}

/// Load + validate a manifest from a package dir (used by the runtime and apm).
pub fn load_manifest(path: &Path) -> Result<Manifest, String> {
    let s = fs::read_to_string(path.join("manifest.json"))
        .map_err(|e| format!("cannot read manifest.json: {e}"))?;
    serde_json::from_str(&s).map_err(|e| format!("manifest.json invalid: {e}"))
}

/// sha256 hex digest of the raw manifest.json bytes — the Phase 1 integrity
/// anchor for the install registry (signing arrives Phase 2).
pub fn manifest_digest(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(path.join("manifest.json"))
        .map_err(|e| format!("cannot read manifest.json: {e}"))?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_manifest() -> Manifest {
        serde_json::from_value(serde_json::json!({
            "format": "agentos",
            "format_version": "1.0.0",
            "id": "com.agentos.hello",
            "name": "Hello Agent",
            "version": "1.0.0",
            "developer": {"name": "AgentOS Project"},
            "runtime": {"engine": "mock"},
            "permissions": [],
            "capabilities": ["greeting"]
        }))
        .unwrap()
    }

    #[test]
    fn valid_manifest_passes() {
        let r = validate_manifest(&good_manifest());
        assert!(r.valid(), "unexpected errors: {:?}", r.errors);
    }

    #[test]
    fn bad_format_rejected() {
        let mut m = good_manifest();
        m.format = "other-os".into();
        assert!(!validate_manifest(&m).valid());
    }

    #[test]
    fn bad_format_version_rejected() {
        let mut m = good_manifest();
        m.format_version = "2.0".into();
        assert!(!validate_manifest(&m).valid());
    }

    #[test]
    fn bad_semver_rejected() {
        let mut m = good_manifest();
        m.version = "one-point-oh".into();
        assert!(!validate_manifest(&m).valid());
    }

    #[test]
    fn bad_id_rejected() {
        for bad in ["hello", "Com.Example.Hello", "com..double", "com.-lead", ""] {
            let mut m = good_manifest();
            m.id = bad.into();
            assert!(
                !validate_manifest(&m).valid(),
                "id {bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn unknown_permission_rejected() {
        let mut m = good_manifest();
        m.permissions = vec![PermissionRequest {
            permission: "filesystem.root".into(),
            access: None,
            resource: None,
            reason: None,
        }];
        assert!(!validate_manifest(&m).valid());
    }

    #[test]
    fn all_documented_permissions_are_known() {
        for p in permission::ALL {
            assert!(permission::is_known(p));
        }
        assert_eq!(permission::ALL.len(), 11);
    }

    #[test]
    fn access_satisfaction_rules() {
        assert!(access::satisfies("read", "read"));
        assert!(access::satisfies("write", "read")); // write implies read
        assert!(!access::satisfies("read", "write"));
    }

    #[test]
    fn non_mock_engine_warns_but_installs() {
        let mut m = good_manifest();
        m.runtime.engine = Some("process".into());
        let r = validate_manifest(&m);
        assert!(r.valid());
        assert!(!r.warnings.is_empty());
    }

    #[test]
    fn native_engine_is_validated_and_executable() {
        let mut m = good_manifest();
        m.runtime.engine = Some(engine::NATIVE.into());
        let r = validate_manifest(&m);
        assert!(
            r.valid(),
            "native must install without errors: {:?}",
            r.errors
        );
        assert!(
            r.warnings.is_empty(),
            "native is executable now; no warning expected: {:?}",
            r.warnings
        );
        assert!(engine::is_executable(engine::NATIVE));
        assert!(engine::is_executable(engine::MOCK));
        assert!(!engine::is_executable(engine::PROCESS));
        assert!(!engine::is_executable(engine::WASM));
    }

    // -----------------------------------------------------------------------
    // Native task spec
    // -----------------------------------------------------------------------

    fn native_entry_json(ops: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"kind": "native.task", "task": {"ops": ops}})
    }

    #[test]
    fn native_entry_parses_and_budgets() {
        let e = native::parse_entry(&native_entry_json(serde_json::json!([
            {"op": "say", "message": "Hello from AgentOS"}
        ])))
        .unwrap();
        assert_eq!(e.task.ops.len(), 1);
        assert_eq!(
            e.task.ops[0],
            native::Op::Say {
                message: "Hello from AgentOS".into()
            }
        );

        // unknown op shape -> rejected
        assert!(native::parse_entry(&native_entry_json(serde_json::json!([
            {"op": "exec", "cmd": "rm -rf /"}
        ])))
        .is_err());
        // unknown field on an op -> rejected
        assert!(native::parse_entry(&native_entry_json(serde_json::json!([
            {"op": "say", "message": "x", "extra": 1}
        ])))
        .is_err());
        // wrong kind -> rejected
        assert!(native::parse_entry(&serde_json::json!({
            "kind": "native.other", "task": {"ops": [{"op": "say", "message": "x"}]}
        }))
        .is_err());
        // empty task -> rejected
        assert!(native::parse_entry(&native_entry_json(serde_json::json!([]))).is_err());
        // oversize budget -> rejected
        let many: Vec<_> = (0..65)
            .map(|_| serde_json::json!({"op": "say", "message": "m"}))
            .collect();
        assert!(native::parse_entry(&native_entry_json(serde_json::Value::Array(many))).is_err());
        // fail op is a declared, valid op
        assert!(native::parse_entry(&native_entry_json(serde_json::json!([
            {"op": "fail", "message": "boom"}
        ])))
        .is_ok());
    }

    #[test]
    fn native_package_with_malformed_task_is_rejected() {
        let base =
            std::env::temp_dir().join(format!("agentos-schema-native-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let pkg = base.join("native.agent");
        write_package(
            &pkg,
            r#"{"format":"agentos","format_version":"1.0.0","id":"com.x.native","name":"Native","version":"1.0.0","runtime":{"engine":"native","entry":{"kind":"native.task","task":{"ops":[{"op":"say","message":"hi"}]}}},"permissions":[],"capabilities":["greeting"]}"#,
            r#"{"entry":{"kind":"native.task","task":{"ops":[{"op":"explode","target":"/"}]}}}"#,
            r#"{"permissions":[]}"#,
        );
        let r = validate_package_dir(&pkg);
        assert!(!r.valid(), "malformed native task must reject the package");
        assert!(r.errors.iter().any(|e| e.contains("native.task")));
        let _ = fs::remove_dir_all(&base);
    }

    fn write_package(dir: &Path, manifest: &str, agent: &str, perms: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("manifest.json"), manifest).unwrap();
        fs::write(dir.join("agent.json"), agent).unwrap();
        fs::write(dir.join("permissions.json"), perms).unwrap();
    }

    #[test]
    fn package_dir_validation() {
        let base = std::env::temp_dir().join(format!("agentos-schema-pkg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let good = base.join("good.agent");
        write_package(
            &good,
            r#"{"format":"agentos","format_version":"1.0.0","id":"com.agentos.hello","name":"Hello","version":"1.2.3","runtime":{"engine":"mock"},"permissions":[],"capabilities":["greeting"]}"#,
            r#"{"entry":{"kind":"builtin:echo"},"interface":["bus.receive"],"sandbox":{"fs":[],"network":false,"exec":false}}"#,
            r#"{"permissions":[]}"#,
        );
        let r = validate_package_dir(&good);
        assert!(
            r.valid(),
            "errors: {:?} warnings: {:?}",
            r.errors,
            r.warnings
        );

        let mismatch = base.join("mismatch.agent");
        write_package(
            &mismatch,
            r#"{"format":"agentos","format_version":"1.0.0","id":"com.agentos.netdemo","name":"NetDemo","version":"0.1.0","runtime":{"engine":"mock"},"permissions":[{"permission":"network"}],"capabilities":["fetch"]}"#,
            r#"{"entry":{"kind":"builtin:echo"},"interface":[]}"#,
            r#"{"permissions":[]}"#, // forgot to declare network
        );
        let r = validate_package_dir(&mismatch);
        assert!(!r.valid());
        assert!(r
            .errors
            .iter()
            .any(|e| e.contains("not declared in permissions.json")));

        let missing = base.join("missing.agent");
        write_package(&missing, r#"{"format":"agentos"}"#, "{}", "{}");
        // manifest.json exists but fails schema parse -> error, no panic
        let r = validate_package_dir(&missing);
        assert!(!r.valid());

        let nodir = base.join("nope.agent");
        let r = validate_package_dir(&nodir);
        assert!(!r.valid());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    #[cfg(unix)]
    fn package_symlinks_are_rejected() {
        let base = std::env::temp_dir().join(format!("agentos-schema-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let package = base.join("link.agent");
        write_package(
            &package,
            r#"{"format":"agentos","format_version":"1.0.0","id":"com.x.link","name":"Link","version":"1.0.0"}"#,
            r#"{"entry":{"kind":"builtin:echo"}}"#,
            r#"{"permissions":[]}"#,
        );
        fs::create_dir_all(package.join("assets")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", package.join("assets/escape")).unwrap();
        let report = validate_package_dir(&package);
        assert!(!report.valid());
        assert!(report
            .errors
            .iter()
            .any(|error| error.contains("symbolic links")));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn digest_is_stable_and_content_sensitive() {
        let base = std::env::temp_dir().join(format!("agentos-schema-dig-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let p = base.join("a.agent");
        write_package(
            &p,
            r#"{"format":"agentos","format_version":"1.0.0","id":"com.x.a","name":"A","version":"1.0.0"}"#,
            "{}",
            "{}",
        );
        let d1 = manifest_digest(&p).unwrap();
        let d2 = manifest_digest(&p).unwrap();
        assert_eq!(d1, d2);
        assert_eq!(d1.len(), 64);
        fs::write(p.join("manifest.json"), r#"{"format":"agentos","format_version":"1.0.0","id":"com.x.a","name":"B","version":"1.0.0"}"#).unwrap();
        let d3 = manifest_digest(&p).unwrap();
        assert_ne!(d1, d3);
        let _ = fs::remove_dir_all(&base);
    }
}
