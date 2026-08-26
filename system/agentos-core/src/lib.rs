//! AgentOS Core — the system's root process and service supervisor.
//!
//! In the shipped OS, `agentos-core` runs as **PID 1** (the kernel execs
//! `/init`, which is this binary). It is therefore the owner of:
//!
//! * system initialization — mount `/proc /sys /dev /run /tmp`, devpts,
//!   hostname, device node permissions for the desktop shell,
//! * service lifecycle — spawn every AgentOS system service from
//!   `/etc/agentos/agentos.json`, supervise with restart backoff,
//! * configuration — own `/etc/agentos/agentos.json` (get/set over IPC),
//! * system state — uptime, service states, boot info in `/run/agentos/`,
//! * IPC coordination — the socket registry (`core.sock`) every other
//!   service and the desktop talk to,
//! * shutdown/reboot — SIGTERM => orderly poweroff, SIGINT/SIGUSR1 => reboot.
//!
//! `--dev` runs the identical supervisor against developer-supplied
//! directories on a host (used by integration tests and development).

use agentos_common::Paths;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Configuration (the OS's service manifest)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceSpec {
    pub name: String,
    pub bin: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// "root" | "agent" (resolved against the image's /etc/passwd)
    #[serde(default = "default_user")]
    pub user: String,
    pub socket: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_user() -> String {
    "agent".into()
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShellSpec {
    #[serde(default = "default_shell_bin")]
    pub bin: String,
    #[serde(default = "default_shell_user")]
    pub user: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_shell_bin() -> String {
    "/usr/bin/agentos-shell".into()
}
fn default_shell_user() -> String {
    "agent".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TtySpec {
    pub dev: String,
    #[serde(default = "default_user")]
    pub user: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SystemConfig {
    pub version: u32,
    /// Startup order matters: permission authority and bus come first.
    pub services: Vec<ServiceSpec>,
    pub shell: ShellSpec,
    #[serde(default)]
    pub ttys: Vec<TtySpec>,
    #[serde(default = "default_planner")]
    pub planner: String,
    #[serde(default = "default_hostname")]
    pub hostname: String,
}

fn default_planner() -> String {
    "mock".into()
}
fn default_hostname() -> String {
    "agentos".into()
}

/// The default OS configuration — matches rootfs/etc/agentos/agentos.json
/// which ships in the image. Kept here so a missing config still boots.
pub fn default_config() -> SystemConfig {
    serde_json::from_str(include_str!("../../../rootfs/etc/agentos/agentos.json"))
        .expect("bundled default config must parse")
}

impl SystemConfig {
    pub fn load(etc_dir: &std::path::Path) -> SystemConfig {
        let p = etc_dir.join("agentos.json");
        fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(default_config)
    }
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SvcState {
    Starting,
    Running,
    Backoff,
    Exited,
    Disabled,
}

#[derive(Debug, Clone, Serialize)]
pub struct SvcStatus {
    pub name: String,
    pub state: SvcState,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub last_exit: Option<i32>,
    pub enabled: bool,
}

struct SvcRuntime {
    spec: ServiceSpec,
    child: Option<Child>,
    state: SvcState,
    restarts: u32,
    last_exit: Option<i32>,
    next_spawn: Option<Instant>,
    stable_since: Option<Instant>,
    /// set when the operator disabled the service
    stopped_by_operator: bool,
}

pub struct Supervisor {
    pub cfg: SystemConfig,
    pub paths: Paths,
    /// dev mode: never touch mounts/uids/syscalls that need root
    pub dev: bool,
    services: Mutex<BTreeMap<String, SvcRuntime>>,
    shell: Mutex<Option<Child>>,
    shell_restarts: Mutex<u32>,
    /// runtime toggle (config.set shell.enabled); init from cfg
    pub shell_enabled: std::sync::atomic::AtomicBool,
    pub boot_id: String,
    pub started: Instant,
    pub autotest: bool,
}

pub const RESTART_BACKOFF_SECS: &[u64] = &[1, 2, 4, 8, 10];
pub const STABLE_AFTER_SECS: u64 = 60;

fn uid_for(user: &str) -> Option<(u32, u32)> {
    match user {
        "root" => Some((0, 0)),
        "agent" => Some((1000, 1000)),
        _ => None,
    }
}

impl Supervisor {
    pub fn new(cfg: SystemConfig, paths: Paths, dev: bool, autotest: bool) -> Supervisor {
        let shell_on = cfg.shell.enabled;
        Supervisor {
            cfg,
            paths,
            dev,
            services: Mutex::new(BTreeMap::new()),
            shell: Mutex::new(None),
            shell_restarts: Mutex::new(0),
            shell_enabled: std::sync::atomic::AtomicBool::new(shell_on),
            boot_id: agentos_common::new_id(),
            started: Instant::now(),
            autotest,
        }
    }

    fn service_args(spec: &ServiceSpec, paths: &Paths) -> Vec<String> {
        vec![
            "--socket".into(),
            paths.run_dir.join(&spec.socket).display().to_string(),
            "--run-dir".into(),
            paths.run_dir.display().to_string(),
            "--state-dir".into(),
            paths.state_dir.display().to_string(),
            "--etc-dir".into(),
            paths.etc_dir.display().to_string(),
        ]
    }

    pub fn spawn_initial(&self) {
        // Do not hold the service-map mutex while spawning: `spawn_service`
        // takes the same lock. The old prototype kept a guard alive through a
        // `drop(&guard)` (which only drops a reference), deadlocking PID 1.
        for spec in self.cfg.services.clone() {
            {
                let mut svcs = self.services.lock().unwrap();
                let rt = SvcRuntime {
                    spec: spec.clone(),
                    child: None,
                    state: SvcState::Starting,
                    restarts: 0,
                    last_exit: None,
                    next_spawn: None,
                    stable_since: None,
                    stopped_by_operator: false,
                };
                svcs.insert(spec.name.clone(), rt);
            }
            self.spawn_service(&spec.name);
        }
    }

    fn spawn_service(&self, name: &str) {
        let mut svcs = self.services.lock().unwrap();
        let Some(rt) = svcs.get_mut(name) else { return };
        if rt.stopped_by_operator || !rt.spec.enabled {
            rt.state = if rt.stopped_by_operator {
                SvcState::Exited
            } else {
                SvcState::Disabled
            };
            return;
        }
        let spec = rt.spec.clone();
        let log_file = self.paths.log_dir.join(format!("{}.out", spec.name));
        let args = Self::service_args(&spec, &self.paths);
        let user = spec.user.clone();
        let dev = self.dev;

        let mut cmd = Command::new(&spec.bin);
        cmd.args(spec.args).args(&args);
        cmd.env("PATH", "/bin:/sbin:/usr/bin:/usr/sbin");
        cmd.env("TERM", "linux");
        cmd.env("AGENTOS_VERSION", agentos_common::VERSION);
        cmd.env("AGENTOS_BOOT_ID", &self.boot_id);
        cmd.env("USER", &user);
        cmd.env("LOGNAME", &user);
        cmd.env(
            "HOME",
            if user == "agent" {
                "/home/agent"
            } else {
                "/root"
            },
        );
        if !dev {
            cmd.env("AGENTOS_SOCKET_GID", "1000");
        }
        if let Ok(f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)
        {
            cmd.stdout(f.try_clone().unwrap());
            cmd.stderr(f);
        } else {
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::null());
        }
        if let Some((uid, gid)) = uid_for(&user) {
            if !dev && uid != 0 {
                let g = gid;
                let u = uid;
                unsafe {
                    cmd.pre_exec(move || {
                        // drop supplementary groups, then gid, then uid
                        if libc::setgroups(0, std::ptr::null()) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        if libc::setgid(g) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        if libc::setuid(u) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
        } else {
            agentos_common::warn(&format!(
                "unknown user {:?} for service {}; running as root",
                user, spec.name
            ));
        }
        // own session so core's signals don't broadcast to services
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(child) => {
                agentos_common::info(&format!(
                    "service {} started (pid {}, user {})",
                    spec.name,
                    child.id(),
                    user
                ));
                rt.child = Some(child);
                rt.state = SvcState::Running;
                rt.next_spawn = None;
                if rt.stable_since.is_none() {
                    rt.stable_since = Some(Instant::now());
                }
            }
            Err(e) => {
                agentos_common::error(&format!("service {} failed to spawn: {e}", spec.name));
                rt.state = SvcState::Exited;
                rt.last_exit = Some(-1);
                rt.restarts += 1;
                rt.next_spawn = Some(
                    Instant::now()
                        + Duration::from_secs(
                            RESTART_BACKOFF_SECS
                                [(rt.restarts as usize - 1).min(RESTART_BACKOFF_SECS.len() - 1)],
                        ),
                );
            }
        }
    }

    /// Called by the reaper when a child exits.
    pub fn handle_exit(&self, pid: u32, code: i32) {
        // shell?
        {
            let mut sh = self.shell.lock().unwrap();
            if sh.as_ref().map(|c| c.id()) == Some(pid) {
                *sh = None;
                let mut n = self.shell_restarts.lock().unwrap();
                *n += 1;
                drop(n);
                drop(sh);
                if *self.shell_restarts.lock().unwrap() < 8 {
                    agentos_common::warn("agentos-shell exited; respawning in 2s");
                    std::thread::spawn(|| {
                        std::thread::sleep(Duration::from_secs(2));
                        // main loop respawns via poll tick
                    });
                } else {
                    agentos_common::error("agentos-shell exited 8 times; leaving framebuffer console (tty1) available");
                }
                return;
            }
        }
        let mut svcs = self.services.lock().unwrap();
        for (name, rt) in svcs.iter_mut() {
            if rt.child.as_ref().map(|c| c.id()) == Some(pid) {
                // the child has already been reaped by waitpid in the loop
                drop(rt.child.take());
                rt.last_exit = Some(code);
                rt.restarts += 1;
                // reset backoff if it had been stable
                if let Some(t) = rt.stable_since {
                    if t.elapsed().as_secs() > STABLE_AFTER_SECS {
                        rt.restarts = 1;
                    }
                }
                let delay = RESTART_BACKOFF_SECS
                    [(rt.restarts as usize - 1).min(RESTART_BACKOFF_SECS.len() - 1)];
                agentos_common::warn(&format!(
                    "service {} exited (code {code}); restart #{}/{} in {delay}s",
                    name, rt.restarts, "∞",
                ));
                rt.state = SvcState::Backoff;
                rt.next_spawn = Some(Instant::now() + Duration::from_secs(delay as u64));
                rt.stable_since = None;
                return;
            }
        }
    }

    /// Kick any due respawns; returns the delay until next decision point.
    pub fn tick(&self) {
        {
            let svcs = self.services.lock().unwrap();
            let due: Vec<String> = svcs
                .iter()
                .filter(|(_, rt)| rt.next_spawn.map(|t| t <= Instant::now()).unwrap_or(false))
                .map(|(n, _)| n.clone())
                .collect();
            drop(svcs);
            for name in due {
                self.spawn_service(&name);
            }
        }
        // respawn shell if enabled and dead and under the crash limit
        if self
            .shell_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
            && *self.shell_restarts.lock().unwrap() < 8
        {
            let need = self.shell.lock().unwrap().is_none();
            if need {
                self.spawn_shell();
            }
        }
    }

    pub fn spawn_shell(&self) {
        let user = self.cfg.shell.user.clone();
        let dev = self.dev;
        let log_file = self.paths.log_dir.join("shell.out");
        let mut cmd = Command::new(&self.cfg.shell.bin);
        cmd.env("PATH", "/bin:/sbin:/usr/bin:/usr/sbin");
        cmd.env("TERM", "linux");
        cmd.env("AGENTOS_VERSION", agentos_common::VERSION);
        cmd.env("AGENTOS_BOOT_ID", &self.boot_id);
        cmd.env("USER", &user);
        cmd.env("LOGNAME", &user);
        cmd.env(
            "HOME",
            if user == "agent" {
                "/home/agent"
            } else {
                "/root"
            },
        );
        if self.autotest {
            cmd.env("AGENTOS_AUTOTEST", "1");
        }
        if let Ok(f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)
        {
            cmd.stdout(f.try_clone().unwrap());
            cmd.stderr(f);
        }
        if let Some((uid, gid)) = uid_for(&user) {
            if !dev && uid != 0 {
                let (g, u) = (gid, uid);
                unsafe {
                    cmd.pre_exec(move || {
                        libc::setgroups(0, std::ptr::null());
                        libc::setgid(g);
                        libc::setuid(u);
                        Ok(())
                    });
                }
            }
        }
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(c) => {
                agentos_common::info(&format!("agentos-shell started (pid {})", c.id()));
                *self.shell.lock().unwrap() = Some(c);
            }
            Err(e) => agentos_common::error(&format!("agentos-shell spawn failed: {e}")),
        }
    }

    /// Spawn an interactive root shell on a tty (debug console + serial).
    /// Phase 1 auto-logs-in as root on these consoles; a real login is Phase 2.
    pub fn spawn_tty(&self, dev_path: &str, _user: &str) {
        if self.dev {
            return; // no tty takeover in dev mode
        }
        let tty = PathBuf::from(dev_path);
        let Ok(f) = fs::OpenOptions::new().read(true).write(true).open(&tty) else {
            agentos_common::warn(&format!("cannot open {dev_path}; skipping console"));
            return;
        };
        let fd = f.into_raw_fd();
        let stdin_file = unsafe { std::fs::File::from_raw_fd(fd) };
        let stdout_file = unsafe { std::fs::File::from_raw_fd(libc::dup(fd)) };
        let stderr_file = unsafe { std::fs::File::from_raw_fd(libc::dup(fd)) };
        let mut cmd = Command::new("/bin/busybox");
        cmd.args(["sh", "-l"]);
        cmd.env("PATH", "/bin:/sbin:/usr/bin:/usr/sbin");
        cmd.env("TERM", "linux");
        cmd.env("PS1", "agentos# ");
        cmd.stdin(std::process::Stdio::from(stdin_file));
        cmd.stdout(std::process::Stdio::from(stdout_file));
        cmd.stderr(std::process::Stdio::from(stderr_file));
        unsafe {
            cmd.pre_exec(move || {
                libc::setsid();
                // acquire controlling tty (fd 0 is the tty)
                libc::ioctl(0, libc::TIOCSCTTY, 0u64);
                Ok(())
            });
        }
        let _ = cmd.spawn();
    }

    pub fn statuses(&self) -> Vec<SvcStatus> {
        let svcs = self.services.lock().unwrap();
        svcs.values()
            .map(|rt| SvcStatus {
                name: rt.spec.name.clone(),
                state: rt.state.clone(),
                pid: rt.child.as_ref().map(|c| c.id()),
                restarts: rt.restarts,
                last_exit: rt.last_exit,
                enabled: rt.spec.enabled,
            })
            .collect()
    }

    pub fn shell_pid(&self) -> Option<u32> {
        self.shell.lock().unwrap().as_ref().map(|c| c.id())
    }

    /// Operator stop: kill + disable until restarted.
    pub fn stop_service(&self, name: &str) -> Result<(), String> {
        let mut svcs = self.services.lock().unwrap();
        let rt = svcs
            .get_mut(name)
            .ok_or_else(|| format!("unknown service {name:?}"))?;
        rt.stopped_by_operator = true;
        if let Some(mut c) = rt.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        rt.state = SvcState::Exited;
        Ok(())
    }

    pub fn restart_service(&self, name: &str) -> Result<(), String> {
        {
            let mut svcs = self.services.lock().unwrap();
            let rt = svcs
                .get_mut(name)
                .ok_or_else(|| format!("unknown service {name:?}"))?;
            rt.stopped_by_operator = false;
            rt.spec.enabled = true;
            if let Some(mut c) = rt.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            rt.state = SvcState::Starting;
            rt.restarts = 0;
        }
        self.spawn_service(name);
        Ok(())
    }

    /// Orderly shutdown: stop children, sync, then power off / reboot.
    /// In dev mode just exits the process.
    pub fn shutdown(&self, reboot: bool) {
        agentos_common::info(&format!("shutdown requested (reboot={reboot})"));
        {
            let mut svcs = self.services.lock().unwrap();
            for rt in svcs.values_mut() {
                rt.stopped_by_operator = true;
                if let Some(c) = rt.child.as_mut() {
                    let _ = c.kill();
                }
            }
        }
        if let Some(mut sh) = self.shell.lock().unwrap().take() {
            let _ = sh.kill();
        }
        // give children a moment to flush logs
        std::thread::sleep(Duration::from_millis(1500));
        let _ = std::fs::File::open("/").and_then(|f| f.sync_all());
        if self.dev {
            agentos_common::info("dev mode: exiting");
            std::process::exit(0);
        }
        unsafe {
            libc::sync();
            libc::reboot(if reboot {
                libc::RB_AUTOBOOT
            } else {
                libc::RB_POWER_OFF
            });
        }
        // if reboot syscall returned (e.g. permission), force kernel panic path
        std::process::exit(if reboot { 0 } else { 1 });
    }
}

use std::os::fd::{FromRawFd, IntoRawFd};

// ---------------------------------------------------------------------------
// PID-1 system initialization (real mode only)
// ---------------------------------------------------------------------------

pub fn mount_all() -> Result<(), String> {
    unsafe fn mnt(src: &str, tgt: &str, fs: &str, flags: u64, data: &str) -> Result<(), String> {
        let c_src = std::ffi::CString::new(src).map_err(|e| e.to_string())?;
        let c_tgt = std::ffi::CString::new(tgt).map_err(|e| e.to_string())?;
        let c_fs = std::ffi::CString::new(fs).map_err(|e| e.to_string())?;
        let c_data = std::ffi::CString::new(data).map_err(|e| e.to_string())?;
        let res = libc::mount(
            c_src.as_ptr(),
            c_tgt.as_ptr(),
            c_fs.as_ptr(),
            flags,
            c_data.as_ptr() as *const core::ffi::c_void,
        );
        if res != 0 {
            Err(format!(
                "mount {src} on {tgt} ({fs}): {}",
                std::io::Error::last_os_error()
            ))
        } else {
            Ok(())
        }
    }
    for dir in [
        "/dev", "/dev/pts", "/dev/shm", "/proc", "/sys", "/run", "/tmp",
    ] {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    unsafe {
        // Filesystem type strings are proc/sysfs/etc.
        mnt("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID, "mode=755")?;
        // devtmpfs replaces the initramfs /dev directory, so recreate mount
        // points after it is mounted rather than relying on hidden entries.
        std::fs::create_dir_all("/dev/pts").map_err(|e| e.to_string())?;
        std::fs::create_dir_all("/dev/shm").map_err(|e| e.to_string())?;
        mnt(
            "proc",
            "/proc",
            "proc",
            libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
            "",
        )?;
        mnt(
            "sysfs",
            "/sys",
            "sysfs",
            libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
            "",
        )?;
        mnt(
            "devpts",
            "/dev/pts",
            "devpts",
            libc::MS_NOSUID | libc::MS_NOEXEC,
            "gid=5,mode=620,ptmxmode=0666",
        )?;
        mnt(
            "tmpfs",
            "/run",
            "tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV,
            "mode=755,size=32m",
        )?;
        mnt(
            "tmpfs",
            "/tmp",
            "tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV,
            "mode=1777,size=64m",
        )?;
    }
    Ok(())
}

/// Give the unprivileged AgentOS service group a writable runtime directory.
/// Socket files remain individually controlled by the IPC layer; this directory
/// mode is needed for shell heartbeats and service socket creation.
pub fn prepare_runtime_dir(paths: &Paths, real_boot: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&paths.run_dir, fs::Permissions::from_mode(0o770))
        .map_err(|e| format!("chmod runtime directory: {e}"))?;
    if real_boot {
        let c_path = std::ffi::CString::new(paths.run_dir.as_os_str().as_bytes())
            .map_err(|_| "runtime path contains NUL".to_string())?;
        let result = unsafe { libc::chown(c_path.as_ptr(), u32::MAX, 1000) };
        if result != 0 {
            return Err(format!(
                "chown runtime directory: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

/// Set ownership/mode for the state subtrees before dropping service users.
/// The orchestrator gets one dedicated group-writable journal directory; the
/// package and permission stores remain root-owned.
pub fn prepare_state_dirs(paths: &Paths, real_boot: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let agents_dir = paths.agents_dir();
    let goals_dir = paths.goals_dir();
    let permissions_dir = paths.permissions_dir();
    let models_dir = paths.models_dir();
    let entries = [
        (&paths.state_dir, 0o750, true),
        (&agents_dir, 0o750, true),
        (&goals_dir, 0o770, true),
        (&permissions_dir, 0o700, false),
        (&models_dir, 0o750, true),
    ];
    for (path, mode, share_with_agent_group) in entries {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|e| format!("chmod state path {}: {e}", path.display()))?;
        if real_boot && share_with_agent_group {
            let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
                .map_err(|_| "state path contains NUL".to_string())?;
            let result = unsafe { libc::chown(c_path.as_ptr(), u32::MAX, 1000) };
            if result != 0 {
                return Err(format!(
                    "chown state path {}: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }
        }
    }
    Ok(())
}

pub fn set_hostname(name: &str) -> Result<(), String> {
    let c = std::ffi::CString::new(name).map_err(|_| "invalid hostname".to_string())?;
    let r = unsafe { libc::sethostname(c.as_ptr(), c.as_bytes().len() as libc::size_t) };
    if r != 0 {
        return Err(format!("sethostname: {}", std::io::Error::last_os_error()));
    }
    let _ = agentos_common::atomic_write(std::path::Path::new("/etc/hostname"), name.as_bytes());
    Ok(())
}

/// The desktop shell runs as the unprivileged `agent` user; grant it the
/// framebuffer, DRI and input devices. (Phase 1: mode 0666 — the roadmap
/// tightens this with a device group + ACL scheme, see docs/security.md.)
pub fn relax_device_permissions() {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    for dev in [
        "/dev/fb0",
        "/dev/dri/card0",
        "/dev/dri/renderD128",
        "/dev/input/event0",
        "/dev/input/event1",
        "/dev/input/event2",
        "/dev/input/event3",
        "/dev/input/mice",
    ] {
        if let Ok(md) = fs::metadata(dev) {
            if md.file_type().is_char_device() || dev.contains("dri") {
                let _ = fs::set_permissions(dev, fs::Permissions::from_mode(0o666));
            }
        }
    }
    // scan any extra event nodes (USB tablets etc.)
    if let Ok(rd) = fs::read_dir("/dev/input") {
        for e in rd.flatten() {
            let p = e.path();
            if p.to_string_lossy().starts_with("/dev/input/event") {
                let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o666));
            }
        }
    }
}

pub fn kernel_version() -> String {
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut uts) } == 0 {
        let to_str = |bytes: &[i8]| -> String {
            bytes
                .iter()
                .take_while(|&&b| b != 0)
                .map(|&b| b as u8 as char)
                .collect()
        };
        format!("{} {}", to_str(&uts.sysname), to_str(&uts.release))
    } else {
        "unknown".into()
    }
}

pub fn read_cmdline_flag(flag: &str) -> bool {
    fs::read_to_string("/proc/cmdline")
        .map(|cmdline| {
            cmdline.split_whitespace().any(|token| {
                token == flag
                    || token
                        .strip_prefix(&format!("{flag}="))
                        .map(|value| matches!(value, "1" | "true" | "yes" | "on"))
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

pub fn console_println(msg: &str) {
    if let Ok(mut c) = fs::OpenOptions::new().write(true).open("/dev/console") {
        let _ = writeln!(c, "{msg}");
    }
    // dev mode / tests: also stdout
    println!("{msg}");
}

// ---------------------------------------------------------------------------
// Signal plumbing for PID 1
// ---------------------------------------------------------------------------

pub static SIGNAL_PIPE: (AtomicI32, AtomicI32) = (AtomicI32::new(-1), AtomicI32::new(-1));
pub static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn handle_signal(sig: i32) {
    PENDING_SIGNAL.store(sig, Ordering::SeqCst);
    let wfd = SIGNAL_PIPE.1.load(Ordering::SeqCst);
    if wfd >= 0 {
        let b = [b'x'];
        unsafe {
            let _ = libc::write(wfd, b.as_ptr() as *const core::ffi::c_void, 1);
        }
    }
}

pub fn install_signal_handlers() {
    unsafe {
        let mut pipe_fds = [0i32; 2];
        if libc::pipe(pipe_fds.as_mut_ptr()) != 0 {
            agentos_common::error("cannot create signal pipe");
            std::process::exit(1);
        }
        SIGNAL_PIPE.0.store(pipe_fds[0], Ordering::SeqCst);
        SIGNAL_PIPE.1.store(pipe_fds[1], Ordering::SeqCst);
        let sigs = [
            libc::SIGCHLD,
            libc::SIGTERM,
            libc::SIGINT,
            libc::SIGUSR1,
            libc::SIGUSR2,
        ];
        for s in sigs {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = handle_signal as usize;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(s, &sa, std::ptr::null_mut()) != 0 {
                agentos_common::error("sigaction failed");
                std::process::exit(1);
            }
        }
        // PID 1 ignores SIGPIPE etc. by default in most setups; be explicit.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

pub fn reap_zombies(sup: &Supervisor) {
    loop {
        let status: i32 = 0;
        let mut st = status;
        let pid = unsafe { libc::waitpid(-1, &mut st, libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
        let code = if libc::WIFEXITED(st) {
            libc::WEXITSTATUS(st)
        } else if libc::WIFSIGNALED(st) {
            128 + libc::WTERMSIG(st)
        } else {
            0
        };
        sup.handle_exit(pid as u32, code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn bundled_configuration_has_required_services() {
        let config = default_config();
        assert_eq!(config.version, 1);
        assert_eq!(config.hostname, "agentos");
        assert!(config
            .services
            .iter()
            .any(|service| service.name == "agent-runtime"));
        assert!(config.shell.enabled);
    }

    #[test]
    fn configuration_loads_override_and_falls_back_on_corruption() {
        let dir = std::env::temp_dir().join(format!("agentos-core-config-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("agentos.json"),
            r#"{"version":1,"hostname":"test-host","planner":"mock","services":[],"shell":{"bin":"/bin/false","user":"agent","enabled":false},"ttys":[]}"#,
        )
        .unwrap();
        let loaded = SystemConfig::load(&dir);
        assert_eq!(loaded.hostname, "test-host");
        assert!(loaded.services.is_empty());
        fs::write(dir.join("agentos.json"), "not-json").unwrap();
        let fallback = SystemConfig::load(&dir);
        assert_eq!(fallback.hostname, "agentos");
        let _ = fs::remove_dir_all(&dir);
    }
}
