//! Shared foundation for all AgentOS system components.
//!
//! This crate is deliberately tiny: branding constants, canonical filesystem
//! paths (with dev-mode overrides), a minimal file+stderr logger, and
//! dependency-free unique id generation. No business logic lives here.

use std::fs::{self, OpenOptions};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// AgentOS semantic version (Phase 1 development build).
pub const VERSION: &str = "0.1.0-dev";
/// Human-facing product name.
pub const NAME: &str = "AgentOS";
/// Kernel base line AgentOS Phase 1 is built on.
pub const KERNEL_LINE: &str = "linux-6.12-lts";

/// Well-known service socket names (files under `run_dir`).
pub mod sockets {
    pub const CORE: &str = "core.sock";
    pub const RUNTIME: &str = "runtime.sock";
    pub const ORCHESTRATOR: &str = "orchestrator.sock";
    pub const APM: &str = "apm.sock";
    pub const PERMISSIONS: &str = "permissions.sock";
    pub const BUS: &str = "bus.sock";
    pub const MODELS: &str = "models.sock";
}

/// Canonical AgentOS filesystem layout.
///
/// Real (PID 1 / on-disk OS) defaults:
///   /run/agentos      runtime sockets and state (tmpfs)
///   /var/lib/agentos  persistent state (agents, permissions, goals, models)
///   /var/log/agentos  service logs
///   /etc/agentos      system configuration
///
/// Every base can be overridden with an environment variable so the exact
/// same binaries can run in `--dev` mode on a developer host or in tests
/// without touching real system paths.
#[derive(Debug, Clone)]
pub struct Paths {
    pub run_dir: PathBuf,
    pub state_dir: PathBuf,
    pub etc_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Paths::detect()
    }
}

impl Paths {
    pub fn detect() -> Self {
        let run_dir = env_path("AGENTOS_RUN_DIR", "/run/agentos");
        let state_dir = env_path("AGENTOS_STATE_DIR", "/var/lib/agentos");
        let etc_dir = env_path("AGENTOS_ETC_DIR", "/etc/agentos");
        let log_dir = env_path("AGENTOS_LOG_DIR", "/var/log/agentos");
        Paths {
            run_dir,
            state_dir,
            etc_dir,
            log_dir,
        }
    }

    /// Explicit paths (used by `agentos-core --dev` when spawning services).
    pub fn new(
        run_dir: impl Into<PathBuf>,
        state_dir: impl Into<PathBuf>,
        etc_dir: impl Into<PathBuf>,
        log_dir: impl Into<PathBuf>,
    ) -> Self {
        Paths {
            run_dir: run_dir.into(),
            state_dir: state_dir.into(),
            etc_dir: etc_dir.into(),
            log_dir: log_dir.into(),
        }
    }

    pub fn socket(&self, name: &str) -> PathBuf {
        self.run_dir.join(name)
    }

    /// Directory where installed agent packages live:
    /// `<state_dir>/agents/<agent-id>/<version>/`
    pub fn agents_dir(&self) -> PathBuf {
        self.state_dir.join("agents")
    }

    pub fn permissions_dir(&self) -> PathBuf {
        self.state_dir.join("permissions")
    }

    pub fn goals_dir(&self) -> PathBuf {
        self.state_dir.join("goals")
    }

    pub fn goals_journal(&self) -> PathBuf {
        self.goals_dir().join("goals.json")
    }

    pub fn models_dir(&self) -> PathBuf {
        self.state_dir.join("models.d")
    }

    pub fn system_config(&self) -> PathBuf {
        self.etc_dir.join("agentos.json")
    }

    pub fn permissions_policy(&self) -> PathBuf {
        self.etc_dir.join("permissions-default.json")
    }

    pub fn models_config(&self) -> PathBuf {
        self.etc_dir.join("models.json")
    }

    /// Ensure the runtime directories exist (best effort; in dev mode they are
    /// created under the overridden roots).
    pub fn ensure_runtime_dirs(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.run_dir)?;
        fs::create_dir_all(&self.state_dir)?;
        fs::create_dir_all(self.agents_dir())?;
        fs::create_dir_all(self.goals_dir())?;
        fs::create_dir_all(self.permissions_dir())?;
        fs::create_dir_all(self.models_dir())?;
        fs::create_dir_all(&self.log_dir)?;
        Ok(())
    }
}

fn env_path(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

/// Milliseconds since Unix epoch (all components run on UTC; the Phase 1
/// desktop renders UTC and timezone support is roadmap work).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Compact unique id: 16 hex chars mixing time, pid and a process counter.
/// Deterministic enough for Phase 1 identifiers; not a security primitive.
pub fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0x2c1b);
    let c = COUNTER.fetch_add(0x9e3779b97f4a7c15, Ordering::Relaxed);
    let t = now_ms();
    let pid = std::process::id() as u64;
    let mut x = t ^ (pid << 48) ^ c.rotate_left(17);
    // splitmix64 finalizer for decent bit dispersion
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^= x >> 31;
    format!("{:016x}", x)
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

struct Logger {
    file: Option<std::fs::File>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Initialise the process logger. Lines go to `<log_dir>/<name>.log` (best
/// effort) and always to stderr (which `agentos-core` routes to the per-service
/// log file and, for PID 1, to the kernel console).
pub fn logger_init(name: &str) {
    let paths = Paths::detect();
    let file = fs::create_dir_all(&paths.log_dir).ok().and_then(|_| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.log_dir.join(format!("{name}.log")))
            .ok()
    });
    let _ = LOGGER.set(Logger { file });
}

fn log(level: Level, msg: &str) {
    let line = format!(
        "[{:>10} {} {}] {}",
        now_ms() / 1000,
        level.tag(),
        std::process::id(),
        msg
    );
    eprintln!("{line}");
    if let Some(logger) = LOGGER.get() {
        if let Some(f) = logger.file.as_ref() {
            if let Ok(mut h) = f.try_clone() {
                use std::io::Write as _;
                let _ = writeln!(h, "{line}");
            }
        }
    }
}

pub fn info(msg: &str) {
    log(Level::Info, msg);
}
pub fn warn(msg: &str) {
    log(Level::Warn, msg);
}
pub fn error(msg: &str) {
    log(Level::Error, msg);
}

/// Read a file to string if present (convenience used across services).
pub fn read_to_string_opt(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Atomic file write: write to `<path>.tmp` then rename over `path`.
pub fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_hex() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn paths_respect_env_overrides() {
        // Not parallel-safe with other env tests; env manipulation here is fine
        // because tests in this crate that touch env run serially by default
        // per-process. We scope via unique var values and restore nothing
        // (process-local).
        std::env::set_var("AGENTOS_RUN_DIR", "/tmp/agentos-test-run");
        let p = Paths::detect();
        assert_eq!(p.run_dir, PathBuf::from("/tmp/agentos-test-run"));
        assert_eq!(
            p.socket(sockets::CORE),
            PathBuf::from("/tmp/agentos-test-run/core.sock")
        );
        std::env::remove_var("AGENTOS_RUN_DIR");
    }
}
