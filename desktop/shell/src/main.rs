//! agentos-shell — the AgentOS desktop.
//!
//! Boots straight onto the framebuffer: no display server, no compositor.
//! Panels talk to the real AgentOS services over the same IPC the rest of
//! the OS uses. Writes a heartbeat (`shell.json`) into /run/agentos so
//! AgentOS Core can supervise the desktop as a first-class service.

mod app;
mod fb;
mod font;
mod input;
mod sysinfo;
mod term;
mod ui;

use input::{Ev, Input};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn main() {
    agentos_common::logger_init("agentos-shell");
    if let Err(e) = font::init() {
        fb::fb_error_hang(&format!("font init failed: {e}"));
    }
    let fbdev = std::env::var("AGENTOS_FB").unwrap_or_else(|_| "/dev/fb0".into());
    let fb = match fb::Framebuffer::open(&fbdev) {
        Ok(f) => f,
        Err(e) => fb::fb_error_hang(&format!("framebuffer: {e}")),
    };
    let (xres, yres) = (fb.xres as i32, fb.yres as i32);
    agentos_common::info(&format!("shell starting on {fbdev} {xres}x{yres}"));

    let mut app = app::App::new(fb);
    let mut input = Input::open(xres, yres);
    let mut pfd = input.poll_fds();
    let (mut mx, mut my) = (512, 384);
    let autotest = std::env::var("AGENTOS_AUTOTEST").ok().as_deref() == Some("1");

    let mut last_beat = Instant::now() - Duration::from_secs(2);
    let mut last_present = Instant::now();
    let started = Instant::now();
    let mut selftest_done = false;

    loop {
        // ---- input ----
        let mut events = Vec::new();
        let mut pollfds = std::mem::take(&mut pfd);
        let r = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as u64, 30) };
        pfd = pollfds;
        if r > 0 {
            input.poll(&mut pfd, &mut mx, &mut my, &mut events);
        }
        for ev in events {
            match ev {
                Ev::Move(x, y) => {
                    app.mouse = (x, y);
                    app.dirty = true;
                }
                Ev::Button {
                    left: true,
                    pressed: true,
                } => {
                    app.on_click(mx, my);
                }
                Ev::Wheel(_) => {}
                Ev::Key(k, _) => app.on_key(k),
                _ => {}
            }
        }

        // ---- sampler / terminal updates mark dirty ----
        if app.sampler.dirty.swap(false, Ordering::AcqRel) {
            app.dirty = true;
        }
        if let Some(t) = &app.terminal {
            if t.changed.swap(false, Ordering::AcqRel) {
                if app.panel == "terminal" {
                    app.dirty = true;
                }
            }
        }

        // ---- render (cap ~30fps) ----
        if app.dirty && last_present.elapsed() >= Duration::from_millis(33) {
            app.render();
            app.dirty = false;
            last_present = Instant::now();
        }

        // ---- heartbeat (1 Hz) ----
        if last_beat.elapsed() >= Duration::from_secs(1) {
            last_beat = Instant::now();
            let hb = serde_json::json!({
                "pid": std::process::id(),
                "frames": app.frames,
                "last_ms": agentos_common::now_ms(),
                "fb": format!("{}x{}", xres, yres),
                "panel": app.panel,
            });
            let _ = agentos_common::atomic_write(
                &app.paths.run_dir.join("shell.json"),
                hb.to_string().as_bytes(),
            );
        }

        // ---- shell-side selftest (in-guest desktop verification) ----
        if autotest && !selftest_done && started.elapsed() > Duration::from_secs(6) {
            selftest_done = true;
            run_selftest(&mut app);
        }

        // keep terminal alive reference
        if app.terminal.is_none() && app.panel == "terminal" {
            app.dirty = true;
        }
    }
}

fn run_selftest(app: &mut app::App) {
    let report = |name: &str, ok: bool, detail: &str| {
        let line = format!(
            "[AGENTOS-TEST] {} desktop.{name} — {detail}",
            if ok { "PASS" } else { "FAIL" },
        );
        if let Ok(mut c) = std::fs::OpenOptions::new().write(true).open("/dev/console") {
            use std::io::Write;
            let _ = writeln!(c, "{line}");
        }
        println!("{line}");
        agentos_common::info(&line);
    };
    // 1. filesystem panel works
    match std::fs::read_dir(&app.files_path) {
        Ok(rd) => {
            let n = rd.count();
            report(
                "files-browse",
                n > 0,
                &format!("{n} entries in {}", app.files_path),
            )
        }
        Err(e) => report("files-browse", false, &e.to_string()),
    }
    // 2. terminal PTY round-trip: echo a marker and read it back
    app.ensure_terminal(900, 500);
    let marker_ok = if let Some(t) = &app.terminal {
        t.write_input(b"echo __AGENTOS_PTY_OK__\r");
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut found = false;
        while Instant::now() < deadline && !found {
            let snap = t.snapshot();
            let text: String = snap.grid[..]
                .iter()
                .map(|c| c.ch)
                .collect::<String>()
                .chars()
                .rev()
                .take(600)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if text.contains("__AGENTOS_PTY_OK__") {
                found = true;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        found
    } else {
        false
    };
    report(
        "terminal-pty",
        marker_ok,
        "echo round-trip through /bin/busybox sh",
    );
    // 3. settings: read config through core
    let p = agentos_common::Paths::detect();
    let st = agentos_ipc::call(
        &p.socket(agentos_common::sockets::CORE),
        "status",
        serde_json::json!({}),
    );
    match st {
        Ok(v) => report(
            "settings-core-status",
            v["version"].is_string(),
            &format!("core {} up", v["version"].as_str().unwrap_or("?")),
        ),
        Err(e) => report("settings-core-status", false, &e.message),
    }
    // 4. service round trip through one IPC call from the desktop process
    let ag = agentos_ipc::call(
        &p.socket(agentos_common::sockets::RUNTIME),
        "agents.list",
        serde_json::json!({}),
    );
    report(
        "desktop-ipc",
        ag.is_ok(),
        "runtime answered agents.list from shell process",
    );
}
