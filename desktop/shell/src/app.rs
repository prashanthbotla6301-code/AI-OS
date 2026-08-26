//! The AgentOS desktop application: top bar, dock, panels, overlays, and the
//! immediate-mode event/render loop.

use crate::fb::{Color, Framebuffer};
use crate::font::{self, Face};
use crate::input::Key;
use crate::sysinfo::Sampler;
use crate::term;
use crate::ui::*;
use agentos_common::Paths;
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PANELS: [(&str, &str); 6] = [
    ("Agents", "agents"),
    ("Goals", "goals"),
    ("Tasks", "tasks"),
    ("Files", "files"),
    ("Terminal", "terminal"),
    ("Settings", "settings"),
];

pub struct App {
    pub fb: Framebuffer,
    pub theme: Theme,
    pub mouse: (i32, i32),
    pub panel: &'static str,
    pub hits: Hits,
    pub sampler: Sampler,
    pub dirty: bool,
    // Goals panel
    pub goal_input: String,
    pub goal_input_focused: bool,
    pub goal_feedback: Option<(bool, String)>,
    // Files panel
    pub files_path: String,
    pub files_selected: Option<String>,
    pub files_last_click: (Option<String>, Instant),
    pub files_error: Option<String>,
    // Terminal
    pub terminal: Option<Arc<term::Term>>,
    // Settings
    pub hostname_input: String,
    pub hostname_focused: bool,
    pub settings_tab: u8,
    // Modals / overlays
    pub overlay: Overlay,
    pub install_entries: Vec<(String, bool, String)>, // (path, valid, report)
    pub shutdown_confirm: bool,
    pub frames: u64,
    pub started: Instant,
    pub accent_hex: u32,
    pub paths: Paths,
}

#[derive(PartialEq)]
pub enum Overlay {
    None,
    Status,
    Install,
}

pub fn content_rect(xres: i32, yres: i32) -> (i32, i32, i32, i32) {
    let x = DOCK_W + PAD;
    let y = TOPBAR_H + PAD;
    (x, y, xres - x - PAD, yres - BOTBAR_H - PAD - y)
}

impl App {
    pub fn new(fb: Framebuffer) -> App {
        let theme = Theme::new(0x2dd4bf);
        let sampler = Sampler::start();
        App {
            fb,
            theme,
            mouse: (512, 384),
            panel: "agents",
            hits: Hits::default(),
            sampler,
            dirty: true,
            goal_input: String::new(),
            goal_input_focused: true,
            goal_feedback: None,
            files_path: "/".into(),
            files_selected: None,
            files_last_click: (None, Instant::now()),
            files_error: None,
            terminal: None,
            hostname_input: String::new(),
            hostname_focused: false,
            settings_tab: 0,
            overlay: Overlay::None,
            install_entries: Vec::new(),
            shutdown_confirm: false,
            frames: 0,
            started: Instant::now(),
            accent_hex: 0x2dd4bf,
            paths: Paths::detect(),
        }
    }

    fn snap(&self) -> crate::sysinfo::Snapshot {
        self.sampler.snap.lock().unwrap().clone()
    }

    pub fn ensure_terminal(&mut self, w: i32, h: i32) {
        if self.terminal.is_none() {
            let cw = font::text_width(Face::Mono, 13, "0").max(4);
            let lh = font::line_height(Face::Mono, 13).max(10);
            let cols = ((w - 24) / cw).max(20) as usize;
            let rows = ((h - 24) / lh).max(10) as usize;
            match term::spawn(cols, rows, None) {
                Ok(t) => self.terminal = Some(t),
                Err(e) => agentos_common::error(&format!("terminal spawn failed: {e}")),
            }
        } else if let Some(t) = &self.terminal {
            let cw = font::text_width(Face::Mono, 13, "0").max(4);
            let lh = font::line_height(Face::Mono, 13).max(10);
            let cols = ((w - 24) / cw).max(20) as usize;
            let rows = ((h - 24) / lh).max(10) as usize;
            t.resize(cols, rows);
        }
    }

    // -----------------------------------------------------------------------
    // Frame
    // -----------------------------------------------------------------------

    pub fn render(&mut self) {
        let xres = self.fb.xres as i32;
        let yres = self.fb.yres as i32;
        self.hits = Hits::default();
        let snap = self.snap();
        let (cx, cy, cw, ch) = content_rect(xres, yres);

        self.fb.clear(self.theme.bg);
        self.render_topbar(xres, &snap);
        self.render_dock(xres, yres);

        match self.panel {
            "agents" => self.panel_agents(cx, cy, cw, ch, &snap),
            "goals" => self.panel_goals(cx, cy, cw, ch, &snap),
            "tasks" => self.panel_tasks(cx, cy, cw, ch, &snap),
            "files" => self.panel_files(cx, cy, cw, ch),
            "terminal" => self.panel_terminal(cx, cy, cw, ch),
            "settings" => self.panel_settings(cx, cy, cw, ch, &snap),
            _ => self.panel_agents(cx, cy, cw, ch, &snap),
        }

        self.render_bottombar(xres, yres, &snap);
        match self.overlay {
            Overlay::Status => self.overlay_status(xres, yres, &snap),
            Overlay::Install => self.overlay_install(xres, yres),
            Overlay::None => {}
        }
        if self.shutdown_confirm {
            self.modal_shutdown(xres, yres);
        }
        draw_cursor(&mut self.fb, self.mouse.0, self.mouse.1);
        self.fb.present();
        self.frames += 1;
    }

    fn render_topbar(&mut self, xres: i32, snap: &crate::sysinfo::Snapshot) {
        let t = &self.theme;
        self.fb.fill_rect(0, 0, xres, TOPBAR_H, t.panel);
        self.fb.fill_rect(0, TOPBAR_H - 1, xres, 1, t.border);
        draw_logo(&mut self.fb, 34, TOPBAR_H / 2, 22, t.accent, t.accent2);
        font::draw_text(&mut self.fb, Face::SansBold, 22, 66, 12, "AgentOS", t.text);
        let vw = font::text_width(Face::Sans, 11, "v0.1.0-dev · Phase 1");
        font::draw_text(
            &mut self.fb,
            Face::Sans,
            11,
            66 + font::text_width(Face::SansBold, 22, "AgentOS") + 10,
            20,
            "v0.1.0-dev · Phase 1",
            t.text_muted,
        );
        let _ = vw;

        // right side: status chips
        let mut x = xres - 16;
        let clock = format_utc(snap);
        let cw = font::text_width(Face::Sans, 13, &clock) + 16;
        x -= cw;
        font::draw_text(&mut self.fb, Face::Sans, 13, x + 8, 16, &clock, t.text);
        // services chip (clickable -> status overlay)
        let running = snap
            .services
            .iter()
            .filter(|s| s.state == "running")
            .count();
        let label = format!("core {}/{} ●", running, snap.services.len().max(6));
        let w = font::text_width(Face::Sans, 12, &label) + 22;
        x -= w + 8;
        let healthy = running == snap.services.len() && snap.services.len() > 0;
        let dot = if healthy { t.ok } else { t.warn };
        self.fb.round_rect(x, 14, w, 24, 12, t.chip_bg);
        self.fb.circle(x + 12, 26, 4, dot);
        font::draw_text(
            &mut self.fb,
            Face::Sans,
            12,
            x + 20,
            17,
            &label,
            t.text_muted,
        );
        self.hits.add(x, 14, w, 24, Action::OpenStatus);
        // mem + cpu
        let used = snap.mem_total_kb.saturating_sub(snap.mem_avail_kb);
        let mem = format!(
            "MEM {:.0}%",
            if snap.mem_total_kb > 0 {
                (used * 100 / snap.mem_total_kb) as i32
            } else {
                0
            }
        );
        let cpu = format!("CPU {:.0}%", snap.cpu_percent as i32);
        for s in [mem, cpu] {
            let w = font::text_width(Face::Sans, 12, &s) + 18;
            x -= w + 8;
            self.fb.round_rect(x, 14, w, 24, 12, t.chip_bg);
            font::draw_text(&mut self.fb, Face::Sans, 12, x + 9, 17, &s, t.text_muted);
        }
    }

    fn render_dock(&mut self, _xres: i32, yres: i32) {
        let t = &self.theme;
        self.fb.fill_rect(0, 0, DOCK_W, yres, t.panel);
        self.fb.fill_rect(DOCK_W - 1, 0, 1, yres, t.border);
        let mut y = TOPBAR_H + 18;
        for (label, id) in PANELS {
            let active = self.panel == id;
            if active {
                self.fb
                    .round_rect(6, y, DOCK_W - 12, 64, 10, t.accent.with_alpha(0x20));
                self.fb.fill_rect(6, y + 14, 3, 36, t.accent);
            }
            draw_dock_icon(
                &mut self.fb,
                DOCK_W / 2,
                y + 20,
                id,
                if active { t.accent } else { t.text_muted },
            );
            let lw = font::text_width(Face::Sans, 11, label);
            font::draw_text(
                &mut self.fb,
                Face::Sans,
                11,
                (DOCK_W - lw) / 2,
                y + 40,
                label,
                if active { t.text } else { t.text_muted },
            );
            self.hits
                .add(6, y, DOCK_W - 12, 64, Action::SwitchPanel(id));
            y += 72;
        }
        // power at bottom
        let py = yres - BOTBAR_H - 66;
        draw_power_icon(&mut self.fb, DOCK_W / 2, py + 16, t.text_muted);
        let lw = font::text_width(Face::Sans, 11, "Power");
        font::draw_text(
            &mut self.fb,
            Face::Sans,
            11,
            (DOCK_W - lw) / 2,
            py + 36,
            "Power",
            t.text_muted,
        );
        self.hits.add(6, py, DOCK_W - 12, 60, Action::Shutdown);
    }

    fn render_bottombar(&mut self, xres: i32, yres: i32, snap: &crate::sysinfo::Snapshot) {
        let t = &self.theme;
        self.fb
            .fill_rect(0, yres - BOTBAR_H, xres, BOTBAR_H, t.panel);
        self.fb.fill_rect(0, yres - BOTBAR_H, xres, 1, t.border);
        let running = snap
            .services
            .iter()
            .filter(|s| s.state == "running")
            .count();
        let left = if snap.ok {
            format!(
                "AgentOS Core ● running — {} service(s) up, uptime {}s",
                running,
                fmt_uptime(snap.uptime_s)
            )
        } else {
            "AgentOS Core ◌ starting…".to_string()
        };
        font::draw_text(
            &mut self.fb,
            Face::Sans,
            11,
            12,
            yres - BOTBAR_H + 8,
            &left,
            t.text_muted,
        );
        let hint =
            "mouse: navigate panels · terminal accepts keyboard · tty1/console = debug shell";
        let hw = font::text_width(Face::Sans, 11, hint);
        font::draw_text(
            &mut self.fb,
            Face::Sans,
            11,
            xres - hw - 12,
            yres - BOTBAR_H + 8,
            hint,
            t.text_muted.with_alpha(0x70),
        );
    }

    // -----------------------------------------------------------------------
    // Panels
    // -----------------------------------------------------------------------

    fn panel_agents(&mut self, x: i32, y: i32, w: i32, h: i32, snap: &crate::sysinfo::Snapshot) {
        let mut ctx = Ctx {
            fb: &mut self.fb,
            theme: &self.theme,
            hits: &mut self.hits,
            mouse: self.mouse,
        };
        panel_header(&mut ctx, x, y, w, "Agents", "Native agent packages installed on this system (Phase 2A: native + mock executors inside the AgentOS sandbox)");
        button(
            &mut ctx,
            x + w - 190,
            y + 8,
            88,
            34,
            "Install…",
            true,
            Action::OpenInstallModal,
        );
        button(
            &mut ctx,
            x + w - 96,
            y + 8,
            88,
            34,
            "Refresh",
            true,
            Action::RefreshAgents,
        );

        let mut yy = y + 70;
        if snap.agents.is_empty() {
            card(&mut ctx, x, yy, w, 90);
            let msg = "No agents installed. Use Install… to add one from /opt/agentos/packages.";
            font::draw_text_clipped(
                ctx.fb,
                Face::Sans,
                14,
                x + 20,
                yy + 34,
                msg,
                ctx.theme.text_muted,
                w - 40,
            );
            return;
        }
        for a in &snap.agents {
            if yy > y + h - 110 {
                break;
            }
            card(&mut ctx, x, yy, w, 96);
            font::draw_text(
                ctx.fb,
                Face::SansBold,
                15,
                x + 20,
                yy + 12,
                &a.name,
                ctx.theme.text,
            );
            let idv = format!("{} · v{} · engine {}", a.id, a.version, a.engine);
            font::draw_text(
                ctx.fb,
                Face::Sans,
                11,
                x + 20,
                yy + 34,
                &idv,
                ctx.theme.text_muted,
            );
            let mut bx = x + 20;
            bx += badge(&mut ctx, bx, yy + 54, &a.state);
            let cap_color = ctx.theme.accent2;
            let cap_bg = ctx.theme.chip_bg;
            for c in a.capabilities.iter().take(4) {
                bx += chip(&mut ctx, bx, yy + 54, c, cap_color, cap_bg, 10) + 6;
                if bx > x + w - 300 {
                    break;
                }
            }
            if !a.permissions.is_empty() {
                let pl = format!("requests: {}", a.permissions.join(", "));
                font::draw_text(ctx.fb, Face::Sans, 10, bx + 6, yy + 56, &pl, ctx.theme.warn);
            }
            if let Some(o) = &a.output {
                let res = format!("result: \u{201c}{}\u{201d}", o);
                font::draw_text_clipped(
                    ctx.fb,
                    Face::SansBold,
                    10,
                    x + 20,
                    yy + 78,
                    &res,
                    ctx.theme.ok,
                    w - 260,
                );
            } else if let Some(d) = &a.detail {
                font::draw_text_clipped(
                    ctx.fb,
                    Face::Sans,
                    10,
                    x + 20,
                    yy + 78,
                    d,
                    ctx.theme.text_muted,
                    w - 260,
                );
            }
            let running = a.state == "Running";
            if running {
                button(
                    &mut ctx,
                    x + w - 96,
                    yy + 30,
                    78,
                    32,
                    "Stop",
                    true,
                    Action::AgentStop(a.id.clone()),
                );
            } else {
                button(
                    &mut ctx,
                    x + w - 96,
                    yy + 30,
                    78,
                    32,
                    "Start",
                    true,
                    Action::AgentStart(a.id.clone()),
                );
            }
            yy += 108;
        }
    }

    fn panel_goals(&mut self, x: i32, y: i32, w: i32, h: i32, snap: &crate::sysinfo::Snapshot) {
        let mut ctx = Ctx {
            fb: &mut self.fb,
            theme: &self.theme,
            hits: &mut self.hits,
            mouse: self.mouse,
        };
        panel_header(&mut ctx, x, y, w, "Goals", "Tell the OS what you want; the orchestrator produces a plan (Phase 1 planner: deterministic mock; LLM planner is a drop-in Phase 2)");
        // input row
        let fy = y + 66;
        let field_w = w - 130;
        ctx.hits.add(x, fy, field_w, 40, Action::SubmitGoal); // focus on click
        text_field(
            &mut ctx,
            x,
            fy,
            field_w,
            &self.goal_input,
            self.goal_input_focused,
            "type a goal, e.g. organize my downloads folder",
        );
        button(
            &mut ctx,
            x + field_w + 12,
            fy,
            110,
            40,
            "Submit",
            !self.goal_input.trim().is_empty(),
            Action::SubmitGoal,
        );
        // suggestions
        let mut sx = x;
        let sy = fy + 54;
        for (g, act) in [
            (
                "organize my downloads folder",
                "organize my downloads folder",
            ),
            ("summarize a document", "summarize this document for me"),
            (
                "fetch the news (will be blocked)",
                "fetch the latest news from the web",
            ),
        ] {
            let cw = font::text_width(Face::Sans, 11, g) + 20;
            if sx + cw > x + w {
                break;
            }
            button(&mut ctx, sx, sy, cw, 28, g, true, Action::SuggestGoal(act));
            sx += cw + 8;
        }
        if let Some((ok, msg)) = &self.goal_feedback {
            let color = if *ok { ctx.theme.ok } else { ctx.theme.err };
            let msg = msg.clone();
            font::draw_text_clipped(ctx.fb, Face::Sans, 11, x, sy + 36, &msg, color, w);
        }

        // goals list
        let mut yy = sy + 62;
        for g in snap.goals.iter().rev().take(4) {
            if yy > y + h - 60 {
                break;
            }
            card(&mut ctx, x, yy, w, 92);
            font::draw_text_clipped(
                ctx.fb,
                Face::SansBold,
                13,
                x + 16,
                yy + 10,
                &format!("Goal: {}", g.text),
                ctx.theme.text,
                w - 32,
            );
            let meta = format!("status {} · {} task(s)", g.status, g.tasks);
            font::draw_text(
                ctx.fb,
                Face::Sans,
                11,
                x + 16,
                yy + 30,
                &meta,
                ctx.theme.text_muted,
            );
            let mut tx = x + 16;
            let mut ty = yy + 50;
            for t in g.plan_tasks.iter().take(6) {
                let (mark, color) = status_mark(&t.status);
                let s = format!("{} {}", mark, t.title);
                let tw = font::text_width(Face::Sans, 10, &s) + 14;
                if tx + tw > x + w - 16 {
                    tx = x + 16;
                    ty += 18;
                    if ty > yy + 78 {
                        break;
                    }
                }
                font::draw_text(ctx.fb, Face::Sans, 10, tx, ty, &s, color);
                tx += tw;
            }
            yy += 100;
        }
    }

    fn panel_tasks(&mut self, x: i32, y: i32, w: i32, h: i32, snap: &crate::sysinfo::Snapshot) {
        let mut ctx = Ctx {
            fb: &mut self.fb,
            theme: &self.theme,
            hits: &mut self.hits,
            mouse: self.mouse,
        };
        panel_header(&mut ctx, x, y, w, "Tasks", "All tasks across plans — dependency graph executed bottom-up (dispatch to agents is Phase 2)");
        let mut yy = y + 70;
        // header row
        font::draw_text(
            ctx.fb,
            Face::SansBold,
            11,
            x + 8,
            yy,
            "STATUS",
            ctx.theme.text_muted,
        );
        font::draw_text(
            ctx.fb,
            Face::SansBold,
            11,
            x + 130,
            yy,
            "TASK",
            ctx.theme.text_muted,
        );
        font::draw_text(
            ctx.fb,
            Face::SansBold,
            11,
            x + w - 200,
            yy,
            "CAPABILITY",
            ctx.theme.text_muted,
        );
        yy += 14;
        if snap.tasks.is_empty() {
            card(&mut ctx, x, yy, w, 70);
            font::draw_text(
                ctx.fb,
                Face::Sans,
                13,
                x + 18,
                yy + 26,
                "No tasks yet — submit a goal on the Goals panel.",
                ctx.theme.text_muted,
            );
            return;
        }
        for t in snap
            .tasks
            .iter()
            .rev()
            .take((((h - 100) / 30).max(3)) as usize)
        {
            let (mark, color) = status_mark(&t.status);
            list_row(&mut ctx, x, yy, w, 28, false, None);
            font::draw_text(ctx.fb, Face::Sans, 11, x + 8, yy + 7, mark, color);
            font::draw_text_clipped(
                ctx.fb,
                Face::Sans,
                12,
                x + 130,
                yy + 6,
                &t.title,
                ctx.theme.text,
                w - 340,
            );
            font::draw_text_clipped(
                ctx.fb,
                Face::Sans,
                11,
                x + w - 200,
                yy + 7,
                &t.capability,
                ctx.theme.accent2,
                190,
            );
            yy += 30;
        }
    }

    fn panel_files(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let mut ctx = Ctx {
            fb: &mut self.fb,
            theme: &self.theme,
            hits: &mut self.hits,
            mouse: self.mouse,
        };
        panel_header(&mut ctx, x, y, w, "Files", "Browse the live AgentOS root filesystem (read-only view; agent-mediated writes arrive with permissions)");
        // nav bar
        let ny = y + 62;
        button(&mut ctx, x, ny, 80, 32, "Up", true, Action::FileUp);
        button(
            &mut ctx,
            x + 88,
            ny,
            92,
            32,
            "Refresh",
            true,
            Action::FileRefresh,
        );
        font::draw_text_clipped(
            ctx.fb,
            Face::Mono,
            12,
            x + 196,
            ny + 8,
            &self.files_path,
            ctx.theme.accent,
            w - 210,
        );

        let mut yy = ny + 44;
        let mut entries: Vec<(String, bool, u64, String)> = Vec::new();
        match std::fs::read_dir(&self.files_path) {
            Ok(rd) => {
                let mut raw: Vec<_> = rd.flatten().collect();
                raw.sort_by_key(|e| {
                    (
                        e.path().is_dir() == false,
                        e.file_name().to_string_lossy().to_string(),
                    )
                });
                for e in raw {
                    let name = e.file_name().to_string_lossy().to_string();
                    let full = e.path().display().to_string();
                    let is_dir = e.path().is_dir();
                    let (size, mtime) = match std::fs::metadata(e.path()) {
                        Ok(m) => (m.len(), {
                            let secs = m
                                .modified()
                                .ok()
                                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                                .map(|d| d.as_secs())
                                .unwrap_or(0);
                            fmt_time(secs)
                        }),
                        Err(_) => (0, "?".into()),
                    };
                    entries.push((name, is_dir, size, mtime));
                    let _ = full;
                }
                self.files_error = None;
            }
            Err(e) => {
                self.files_error = Some(e.to_string());
            }
        }
        if let Some(err) = &self.files_error {
            card(&mut ctx, x, yy, w, 60);
            font::draw_text_clipped(
                ctx.fb,
                Face::Sans,
                12,
                x + 16,
                yy + 22,
                &format!("cannot open directory: {err}"),
                ctx.theme.err,
                w - 32,
            );
            return;
        }
        font::draw_text(
            ctx.fb,
            Face::SansBold,
            11,
            x + 8,
            yy,
            "NAME",
            ctx.theme.text_muted,
        );
        font::draw_text(
            ctx.fb,
            Face::SansBold,
            11,
            x + w - 320,
            yy,
            "SIZE",
            ctx.theme.text_muted,
        );
        font::draw_text(
            ctx.fb,
            Face::SansBold,
            11,
            x + w - 200,
            yy,
            "MODIFIED",
            ctx.theme.text_muted,
        );
        yy += 14;
        for (name, is_dir, size, mtime) in
            entries.iter().take((((h - (yy - y)) / 28).max(4)) as usize)
        {
            let path = join_path(&self.files_path, name);
            let selected = self.files_selected.as_deref() == Some(path.as_str());
            list_row(
                &mut ctx,
                x,
                yy,
                w,
                26,
                selected,
                Some(Action::FileEnter(path.clone())),
            );
            let color = if *is_dir {
                ctx.theme.accent
            } else {
                ctx.theme.text
            };
            let icon = if *is_dir { "▸" } else { " " };
            font::draw_text_clipped(
                ctx.fb,
                Face::Sans,
                12,
                x + 10,
                yy + 5,
                &format!("{icon} {name}"),
                color,
                w - 340,
            );
            let size_s = if *is_dir {
                "—".to_string()
            } else {
                fmt_size(*size)
            };
            font::draw_text(
                ctx.fb,
                Face::Sans,
                11,
                x + w - 320,
                yy + 6,
                &size_s,
                ctx.theme.text_muted,
            );
            font::draw_text(
                ctx.fb,
                Face::Sans,
                11,
                x + w - 200,
                yy + 6,
                mtime,
                ctx.theme.text_muted,
            );
            yy += 28;
        }
    }

    fn panel_terminal(&mut self, x: i32, y: i32, w: i32, h: i32) {
        self.ensure_terminal(x + w, h - 70);
        let mut ctx = Ctx {
            fb: &mut self.fb,
            theme: &self.theme,
            hits: &mut self.hits,
            mouse: self.mouse,
        };
        panel_header(&mut ctx, x, y, w, "Terminal", "Real shell on a PTY (/bin/busybox sh, login profile) — keyboard input goes here when this panel is open");
        let tx = x;
        let ty = y + 66;
        let tw = w;
        let th = h - 66;
        ctx.fb.round_rect(tx, ty, tw, th, 10, Color::hex(0x0a0e13));
        ctx.fb.rect_outline(tx, ty, tw, th, ctx.theme.border);
        let Some(t) = self.terminal.clone() else {
            font::draw_text(
                ctx.fb,
                Face::Sans,
                12,
                tx + 16,
                ty + 16,
                "terminal unavailable (PTY spawn failed — see serial log)",
                ctx.theme.err,
            );
            return;
        };
        if !t.alive.load(Ordering::Relaxed) {
            font::draw_text(
                ctx.fb,
                Face::Sans,
                12,
                tx + 16,
                ty + 16,
                "shell exited",
                ctx.theme.err,
            );
        }
        let snap = t.snapshot();
        let cw = font::text_width(Face::Mono, 13, "0").max(4);
        let lh = font::line_height(Face::Mono, 13).max(10);
        let ox = tx + 12;
        let oy = ty + 10;
        for r in 0..snap.rows {
            for c in 0..snap.cols {
                let cell = &snap.grid[r * snap.cols + c];
                if cell.ch == ' ' {
                    if cell.bg != 0 {
                        let (br, bg, bb) = term::bg_rgb(cell.bg);
                        ctx.fb.fill_rect(
                            ox + c as i32 * cw,
                            oy + r as i32 * lh,
                            cw,
                            lh,
                            Color::rgb(br, bg, bb),
                        );
                    }
                    continue;
                }
                if cell.bg != 0 {
                    let (br, bg, bb) = term::bg_rgb(cell.bg);
                    ctx.fb.fill_rect(
                        ox + c as i32 * cw,
                        oy + r as i32 * lh,
                        cw,
                        lh,
                        Color::rgb(br, bg, bb),
                    );
                }
                let (mut fr, mut fg, mut fb) = term::fg_rgb(cell.fg);
                if cell.bold {
                    // brighten bold text toward white
                    fr = ((fr as u16 * 4 / 3).min(255)) as u8;
                    fg = ((fg as u16 * 4 / 3).min(255)) as u8;
                    fb = ((fb as u16 * 4 / 3).min(255)) as u8;
                }
                let face = if cell.bold {
                    Face::MonoBold
                } else {
                    Face::Mono
                };
                font::draw_text(
                    ctx.fb,
                    face,
                    13,
                    ox + c as i32 * cw,
                    oy + r as i32 * lh,
                    &cell.ch.to_string(),
                    Color::rgb(fr, fg, fb),
                );
            }
        }
        // cursor block
        let blink = (self.frames / 16) % 2 == 0;
        if blink && t.alive.load(Ordering::Relaxed) {
            ctx.fb.round_rect(
                ox + (snap.cur_col * cw as usize) as i32,
                oy + (snap.cur_row * lh as usize) as i32,
                cw,
                lh,
                2,
                ctx.theme.accent.with_alpha(0x60),
            );
        }
    }

    fn panel_settings(&mut self, x: i32, y: i32, w: i32, _h: i32, snap: &crate::sysinfo::Snapshot) {
        let mut ctx = Ctx {
            fb: &mut self.fb,
            theme: &self.theme,
            hits: &mut self.hits,
            mouse: self.mouse,
        };
        panel_header(
            &mut ctx,
            x,
            y,
            w,
            "Settings",
            "System settings apply immediately through AgentOS Core",
        );
        // tabs
        let tabs = ["About", "System", "Permissions", "Appearance", "Power"];
        let mut tx = x;
        for (i, t) in tabs.iter().enumerate() {
            let tw = font::text_width(Face::SansBold, 12, t) + 28;
            let active = self.settings_tab == i as u8;
            ctx.fb.round_rect(
                tx,
                y + 62,
                tw,
                32,
                8,
                if active {
                    ctx.theme.accent.with_alpha(0x25)
                } else {
                    ctx.theme.chip_bg
                },
            );
            if active {
                ctx.fb.rect_outline(tx, y + 62, tw, 32, ctx.theme.accent);
            }
            font::draw_text(
                ctx.fb,
                Face::SansBold,
                12,
                tx + 14,
                y + 71,
                t,
                if active {
                    ctx.theme.text
                } else {
                    ctx.theme.text_muted
                },
            );
            // tabs are informational; clicking does not switch panels
            ctx.hits
                .add(tx, y + 62, tw, 32, Action::SwitchPanel(self.panel));
            tx += tw + 6;
        }
        let cy0 = y + 108;
        match self.settings_tab {
            0 => {
                // About
                let rows = [
                    ("Product", format!("AgentOS v{}", agentos_common::VERSION)),
                    ("Kernel", snap.kernel.clone()),
                    ("Hostname", snap.hostname.clone()),
                    ("CPU", format!("{} core(s)", snap.cpu_count)),
                    (
                        "Memory",
                        format!(
                            "{} MB total / {} MB available",
                            snap.mem_total_kb / 1024,
                            snap.mem_avail_kb / 1024
                        ),
                    ),
                    ("Boot id", self.paths.run_dir.display().to_string()),
                    ("License", "MIT OR Apache-2.0 (system components)".into()),
                ];
                let mut yy = cy0;
                for (k, v) in rows {
                    card(&mut ctx, x, yy, w / 2 - 6, 44);
                    font::draw_text(
                        ctx.fb,
                        Face::Sans,
                        11,
                        x + 14,
                        yy + 8,
                        k,
                        ctx.theme.text_muted,
                    );
                    font::draw_text_clipped(
                        ctx.fb,
                        Face::SansBold,
                        12,
                        x + 14,
                        yy + 22,
                        &v,
                        ctx.theme.text,
                        w / 2 - 36,
                    );
                    yy += 50;
                }
            }
            1 => {
                // System: hostname + services
                font::draw_text(
                    ctx.fb,
                    Face::SansBold,
                    13,
                    x,
                    cy0,
                    "Hostname",
                    ctx.theme.text,
                );
                let hy = cy0 + 24;
                text_field(
                    &mut ctx,
                    x,
                    hy,
                    320,
                    &self.hostname_input,
                    self.hostname_focused,
                    &snap.hostname,
                );
                button(
                    &mut ctx,
                    x + 332,
                    hy,
                    100,
                    40,
                    "Apply",
                    !self.hostname_input.trim().is_empty(),
                    Action::HostnameApply,
                );
                ctx.hits.add(x, hy, 320, 40, Action::HostnameApply);
                let mut yy = hy + 56;
                font::draw_text(
                    ctx.fb,
                    Face::SansBold,
                    13,
                    x,
                    yy,
                    "Services (supervised by AgentOS Core)",
                    ctx.theme.text,
                );
                yy += 24;
                for s in &snap.services {
                    list_row(&mut ctx, x, yy, w / 2 - 6, 34, false, None);
                    let (mark, color) = if s.state == "running" {
                        ("●", ctx.theme.ok)
                    } else {
                        ("◌", ctx.theme.warn)
                    };
                    font::draw_text(ctx.fb, Face::Sans, 12, x + 10, yy + 9, mark, color);
                    font::draw_text(
                        ctx.fb,
                        Face::Sans,
                        12,
                        x + 28,
                        yy + 9,
                        &s.name,
                        ctx.theme.text,
                    );
                    let meta = format!(
                        "pid {} · restarts {}",
                        s.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                        s.restarts
                    );
                    font::draw_text(
                        ctx.fb,
                        Face::Sans,
                        10,
                        x + 240,
                        yy + 11,
                        &meta,
                        ctx.theme.text_muted,
                    );
                    button(
                        &mut ctx,
                        x + w / 2 - 130,
                        yy + 1,
                        110,
                        30,
                        "Restart",
                        true,
                        Action::ServiceRestart(s.name.clone()),
                    );
                    yy += 38;
                }
            }
            2 => {
                // Permissions
                font::draw_text(ctx.fb, Face::Sans, 12, x, cy0, "Permission policy — every permission is DENY by default; grants are explicit and revocable.", ctx.theme.text_muted);
                let mut yy = cy0 + 24;
                let perms: Vec<(String, Value)> = snap.permissions_policy["permissions"]
                    .as_object()
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                for (p, v) in perms {
                    list_row(&mut ctx, x, yy, w - 120, 28, false, None);
                    font::draw_text(ctx.fb, Face::Mono, 11, x + 10, yy + 7, &p, ctx.theme.text);
                    let d = v["default"].as_str().unwrap_or("?");
                    let (mark, color) = match d {
                        "deny" => ("deny", ctx.theme.err),
                        "ask" => ("ask", ctx.theme.warn),
                        _ => ("allow", ctx.theme.ok),
                    };
                    font::draw_text(ctx.fb, Face::Sans, 11, x + 330, yy + 7, mark, color);
                    yy += 30;
                }
                let grants = snap.grants["grants"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if grants.is_empty() {
                    font::draw_text(ctx.fb, Face::Sans, 11, x, yy + 8, "No active grants. (Granting arrives with the Phase 2 consent UI; today grants are made via agentos-ctl.)", ctx.theme.text_muted);
                } else {
                    for g in grants {
                        let s = format!(
                            "{} → {} ({})",
                            g["subject"].as_str().unwrap_or("?"),
                            g["permission"].as_str().unwrap_or("?"),
                            g["access"].as_str().unwrap_or("?")
                        );
                        font::draw_text(ctx.fb, Face::Sans, 11, x, yy + 8, &s, ctx.theme.ok);
                        yy += 20;
                    }
                }
            }
            3 => {
                font::draw_text(
                    ctx.fb,
                    Face::SansBold,
                    13,
                    x,
                    cy0,
                    "Accent color (session; persisted settings arrive Phase 2)",
                    ctx.theme.text,
                );
                let mut xx = x;
                let yy = cy0 + 34;
                for (hex, name) in [
                    (0x2dd4bf, "teal"),
                    (0x6366f1, "indigo"),
                    (0xf5a623, "amber"),
                    (0xe0564f, "rose"),
                    (0x3ddc84, "green"),
                ] {
                    self.fb.circle(xx + 20, yy + 20, 14, Color::hex(hex));
                    if self.accent_hex == hex {
                        self.fb
                            .circle(xx + 20, yy + 20, 18, Color::hex(0xffffff).with_alpha(0x50));
                    }
                    font::draw_text(
                        &mut self.fb,
                        Face::Sans,
                        11,
                        xx + 4,
                        yy + 42,
                        name,
                        self.theme.text_muted,
                    );
                    self.hits.add(xx, yy, 44, 58, Action::Accent(hex));
                    xx += 56;
                }
            }
            _ => {
                // Power
                font::draw_text(ctx.fb, Face::SansBold, 13, x, cy0, "Power", ctx.theme.text);
                font::draw_text(ctx.fb, Face::Sans, 12, x, cy0 + 24, "Shutdown and reboot are performed by AgentOS Core (PID 1) with orderly service shutdown.", ctx.theme.text_muted);
                button(
                    &mut ctx,
                    x,
                    cy0 + 56,
                    160,
                    42,
                    "Shut down",
                    true,
                    Action::Shutdown,
                );
                button(
                    &mut ctx,
                    x + 172,
                    cy0 + 56,
                    160,
                    42,
                    "Reboot",
                    true,
                    Action::Reboot,
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Overlays
    // -----------------------------------------------------------------------

    fn overlay_status(&mut self, xres: i32, yres: i32, snap: &crate::sysinfo::Snapshot) {
        let w = 460;
        let x = xres - w - 20;
        let y = TOPBAR_H + 12;
        let h = yres - BOTBAR_H - 24 - y;
        // scrim
        self.fb.fill_rect(
            0,
            TOPBAR_H,
            xres,
            yres - BOTBAR_H - TOPBAR_H,
            Color::rgb(0, 0, 0).with_alpha(0x50),
        );
        let mut ctx = Ctx {
            fb: &mut self.fb,
            theme: &self.theme,
            hits: &mut self.hits,
            mouse: self.mouse,
        };
        ctx.fb.round_rect(x, y, w, h, 12, ctx.theme.panel);
        ctx.fb.rect_outline(x, y, w, h, ctx.theme.border);
        font::draw_text(
            ctx.fb,
            Face::SansBold,
            16,
            x + 18,
            y + 14,
            "System status",
            ctx.theme.text,
        );
        button(
            &mut ctx,
            x + w - 90,
            y + 12,
            74,
            30,
            "Close",
            true,
            Action::CloseOverlay,
        );
        let mut yy = y + 56;
        let info = [
            ("Version", format!("AgentOS v{}", agentos_common::VERSION)),
            ("Kernel", snap.kernel.clone()),
            ("Uptime", fmt_uptime(snap.uptime_s)),
            ("Agents", format!("{} installed", snap.agents.len())),
            ("Goals", format!("{} tracked", snap.goals.len())),
        ];
        for (k, v) in info {
            font::draw_text(ctx.fb, Face::Sans, 12, x + 18, yy, k, ctx.theme.text_muted);
            font::draw_text_clipped(
                ctx.fb,
                Face::SansBold,
                12,
                x + 130,
                yy,
                &v,
                ctx.theme.text,
                w - 150,
            );
            yy += 22;
        }
        yy += 8;
        font::draw_text(
            ctx.fb,
            Face::SansBold,
            12,
            x + 18,
            yy,
            "Services",
            ctx.theme.text,
        );
        yy += 22;
        for s in &snap.services {
            let (mark, color) = if s.state == "running" {
                ("●", ctx.theme.ok)
            } else {
                ("◌", ctx.theme.warn)
            };
            font::draw_text(ctx.fb, Face::Sans, 12, x + 22, yy, mark, color);
            font::draw_text(ctx.fb, Face::Sans, 12, x + 40, yy, &s.name, ctx.theme.text);
            let meta = format!("restarts {}", s.restarts);
            font::draw_text(
                ctx.fb,
                Face::Sans,
                11,
                x + w - 140,
                yy,
                &meta,
                ctx.theme.text_muted,
            );
            yy += 20;
        }
        yy += 8;
        font::draw_text(
            ctx.fb,
            Face::SansBold,
            12,
            x + 18,
            yy,
            "Model providers",
            ctx.theme.text,
        );
        yy += 22;
        for (id, kind, model) in &snap.models {
            font::draw_text(
                ctx.fb,
                Face::Mono,
                11,
                x + 22,
                yy,
                &format!("{id} [{kind}] {model}"),
                ctx.theme.accent2,
            );
            yy += 18;
        }
    }

    fn overlay_install(&mut self, xres: i32, yres: i32) {
        let w = 560;
        let h = 380;
        let x = (xres - w) / 2;
        let y = (yres - h) / 2;
        self.fb.fill_rect(
            0,
            TOPBAR_H,
            xres,
            yres - BOTBAR_H - TOPBAR_H,
            Color::rgb(0, 0, 0).with_alpha(0x60),
        );
        let mut ctx = Ctx {
            fb: &mut self.fb,
            theme: &self.theme,
            hits: &mut self.hits,
            mouse: self.mouse,
        };
        ctx.fb.round_rect(x, y, w, h, 12, ctx.theme.panel);
        ctx.fb.rect_outline(x, y, w, h, ctx.theme.border);
        font::draw_text(
            ctx.fb,
            Face::SansBold,
            16,
            x + 18,
            y + 14,
            "Install agent package",
            ctx.theme.text,
        );
        button(
            &mut ctx,
            x + w - 90,
            y + 12,
            74,
            30,
            "Close",
            true,
            Action::CancelModal,
        );
        font::draw_text(ctx.fb, Face::Sans, 11, x + 18, y + 44, "Packages bundled in /opt/agentos/packages — validated by the package manager, executed in the sandboxed runtime (native engine).", ctx.theme.text_muted);
        let mut yy = y + 70;
        if self.install_entries.is_empty() {
            font::draw_text(
                ctx.fb,
                Face::Sans,
                12,
                x + 18,
                yy,
                "no packages found in /opt/agentos/packages",
                ctx.theme.err,
            );
        }
        for (path, valid, report) in &self.install_entries {
            card(&mut ctx, x + 14, yy, w - 28, 84);
            let name = path.rsplit('/').next().unwrap_or(path);
            font::draw_text(
                ctx.fb,
                Face::SansBold,
                13,
                x + 30,
                yy + 10,
                name,
                ctx.theme.text,
            );
            let status = if *valid {
                "valid package".to_string()
            } else {
                format!("INVALID: {report}")
            };
            font::draw_text(
                ctx.fb,
                Face::Sans,
                10,
                x + 30,
                yy + 30,
                &status,
                if *valid { ctx.theme.ok } else { ctx.theme.err },
            );
            font::draw_text(
                ctx.fb,
                Face::Mono,
                10,
                x + 30,
                yy + 46,
                path,
                ctx.theme.text_muted,
            );
            button(
                &mut ctx,
                x + w - 130,
                yy + 26,
                108,
                32,
                "Install",
                *valid,
                Action::AgentInstall(path.clone()),
            );
            yy += 92;
        }
    }

    fn modal_shutdown(&mut self, xres: i32, yres: i32) {
        let w = 420;
        let h = 190;
        let x = (xres - w) / 2;
        let y = (yres - h) / 2;
        self.fb.fill_rect(
            0,
            TOPBAR_H,
            xres,
            yres - BOTBAR_H - TOPBAR_H,
            Color::rgb(0, 0, 0).with_alpha(0x70),
        );
        let mut ctx = Ctx {
            fb: &mut self.fb,
            theme: &self.theme,
            hits: &mut self.hits,
            mouse: self.mouse,
        };
        ctx.fb.round_rect(x, y, w, h, 12, ctx.theme.panel);
        ctx.fb
            .rect_outline(x, y, w, h, ctx.theme.err.with_alpha(0x50));
        font::draw_text(
            ctx.fb,
            Face::SansBold,
            16,
            x + 20,
            y + 18,
            "Shut down AgentOS?",
            ctx.theme.text,
        );
        font::draw_text(
            ctx.fb,
            Face::Sans,
            12,
            x + 20,
            y + 48,
            "AgentOS Core will stop all services, sync, and power off the machine.",
            ctx.theme.text_muted,
        );
        button(
            &mut ctx,
            x + 20,
            y + h - 60,
            120,
            40,
            "Cancel",
            true,
            Action::CancelModal,
        );
        button(
            &mut ctx,
            x + w - 150,
            y + h - 60,
            130,
            40,
            "Shut down",
            true,
            Action::ConfirmShutdown,
        );
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    pub fn on_click(&mut self, x: i32, y: i32) {
        // First check modals by finding within hits (they were added last)
        if let Some(hit) = self.hits.find(x, y) {
            let action = hit.action.clone();
            self.dispatch(action);
        }
    }

    pub fn dispatch(&mut self, action: Action) {
        let p = &self.paths;
        match action {
            Action::SwitchPanel(id) => {
                self.panel = id;
                self.overlay = Overlay::None;
                self.dirty = true;
            }
            Action::OpenStatus => {
                self.overlay = Overlay::Status;
                self.dirty = true;
            }
            Action::CloseOverlay | Action::CancelModal => {
                self.overlay = Overlay::None;
                self.shutdown_confirm = false;
                self.dirty = true;
            }
            Action::RefreshAgents => {
                let _ = agentos_ipc::call(
                    &p.socket(agentos_common::sockets::RUNTIME),
                    "agents.rescan",
                    serde_json::json!({}),
                );
                self.dirty = true;
            }
            Action::AgentStart(id) => {
                let _ = agentos_ipc::call(
                    &p.socket(agentos_common::sockets::RUNTIME),
                    "agents.start",
                    serde_json::json!({"id": id}),
                );
                self.dirty = true;
            }
            Action::AgentStop(id) => {
                let _ = agentos_ipc::call(
                    &p.socket(agentos_common::sockets::RUNTIME),
                    "agents.stop",
                    serde_json::json!({"id": id}),
                );
                self.dirty = true;
            }
            Action::OpenInstallModal => {
                self.install_entries.clear();
                let mut found: Vec<String> = Vec::new();
                if let Ok(rd) = std::fs::read_dir("/opt/agentos/packages") {
                    for e in rd.flatten() {
                        found.push(e.path().display().to_string());
                    }
                }
                found.sort();
                for path in found {
                    let rep = agentos_ipc::call(
                        &p.socket(agentos_common::sockets::APM),
                        "apm.validate",
                        serde_json::json!({"path": path}),
                    );
                    match rep {
                        Ok(v) => {
                            let valid = v["errors"]
                                .as_array()
                                .map(|a| a.is_empty())
                                .unwrap_or(false);
                            let msg = v["errors"]
                                .as_array()
                                .map(|a| {
                                    a.first().and_then(|e| e.as_str()).unwrap_or("").to_string()
                                })
                                .unwrap_or_default();
                            self.install_entries.push((path, valid, msg));
                        }
                        Err(e) => self.install_entries.push((path, false, e.message)),
                    }
                }
                self.overlay = Overlay::Install;
                self.dirty = true;
            }
            Action::AgentInstall(path) => {
                let res = agentos_ipc::call(
                    &p.socket(agentos_common::sockets::APM),
                    "apm.install",
                    serde_json::json!({"path": path}),
                );
                let _ = agentos_ipc::call(
                    &p.socket(agentos_common::sockets::RUNTIME),
                    "agents.rescan",
                    serde_json::json!({}),
                );
                match res {
                    Ok(_) => {
                        self.overlay = Overlay::None;
                        self.panel = "agents";
                    }
                    Err(e) => agentos_common::error(&format!("install failed: {}", e.message)),
                }
                self.dirty = true;
            }
            Action::SubmitGoal => {
                let text = self.goal_input.trim().to_string();
                if text.is_empty() {
                    return;
                }
                let res = agentos_ipc::call(
                    &p.socket(agentos_common::sockets::ORCHESTRATOR),
                    "goals.submit",
                    serde_json::json!({"text": text}),
                );
                match res {
                    Ok(v) => {
                        let n = v["plan"]["tasks"].as_array().map(|a| a.len()).unwrap_or(0);
                        self.goal_feedback = Some((
                            true,
                            format!("planned: {n} task(s) created (planner: mock)"),
                        ));
                        self.goal_input.clear();
                        self.panel = "goals";
                    }
                    Err(e) => self.goal_feedback = Some((false, format!("error: {}", e.message))),
                }
                self.dirty = true;
            }
            Action::SuggestGoal(g) => {
                self.goal_input = g.to_string();
                self.dirty = true;
            }
            Action::FileEnter(path) => {
                let now = Instant::now();
                let is_dbl = self.files_last_click.0.as_deref() == Some(path.as_str())
                    && now.duration_since(self.files_last_click.1) < Duration::from_millis(450);
                self.files_last_click = (Some(path.clone()), now);
                if std::path::Path::new(&path).is_dir() {
                    if is_dbl {
                        self.files_path = path;
                        self.files_selected = None;
                    } else {
                        self.files_selected = Some(path);
                    }
                } else {
                    self.files_selected = Some(path);
                }
                self.dirty = true;
            }
            Action::FileUp => {
                let mut path = std::path::PathBuf::from(&self.files_path);
                path.pop();
                self.files_path = path.display().to_string();
                if self.files_path.is_empty() {
                    self.files_path = "/".into();
                }
                self.dirty = true;
            }
            Action::FileRefresh => {
                self.dirty = true;
            }
            Action::HostnameApply => {
                let name = self.hostname_input.trim().to_string();
                if !name.is_empty() {
                    let _ = agentos_ipc::call(
                        &p.socket(agentos_common::sockets::CORE),
                        "config.set",
                        serde_json::json!({"key":"hostname","value":name}),
                    );
                    self.hostname_input.clear();
                }
                self.dirty = true;
            }
            Action::ServiceRestart(name) => {
                let _ = agentos_ipc::call(
                    &p.socket(agentos_common::sockets::CORE),
                    "service.restart",
                    serde_json::json!({"name": name}),
                );
                self.dirty = true;
            }
            Action::Accent(hex) => {
                self.accent_hex = hex;
                self.theme = Theme::new(hex);
                self.dirty = true;
            }
            Action::Shutdown => {
                self.shutdown_confirm = true;
                self.dirty = true;
            }
            Action::ConfirmShutdown => {
                let _ = agentos_ipc::call(
                    &p.socket(agentos_common::sockets::CORE),
                    "shutdown",
                    serde_json::json!({}),
                );
            }
            Action::Reboot => {
                let _ = agentos_ipc::call(
                    &p.socket(agentos_common::sockets::CORE),
                    "reboot",
                    serde_json::json!({}),
                );
            }
            Action::ScrollTerminal(_) | Action::TerminalClear => {}
        }
    }

    pub fn on_key(&mut self, key: Key) {
        // Terminal panel eats keys
        if self.panel == "terminal" {
            if let Some(t) = &self.terminal {
                match key {
                    Key::Char(c) => {
                        let mut b = [0u8; 4];
                        t.write_input(c.encode_utf8(&mut b).as_bytes());
                    }
                    Key::Enter => t.write_input(b"\r"),
                    Key::Backspace => t.write_input(&[0x7f]),
                    Key::Tab => t.write_input(b"\t"),
                    Key::Up => t.write_input(b"\x1b[A"),
                    Key::Down => t.write_input(b"\x1b[B"),
                    Key::Right => t.write_input(b"\x1b[C"),
                    Key::Left => t.write_input(b"\x1b[D"),
                    Key::Home => t.write_input(b"\x1b[H"),
                    Key::End => t.write_input(b"\x1b[F"),
                    Key::Ctrl(c) => t.write_input(&[c]),
                    _ => {}
                }
            }
            self.dirty = true;
            return;
        }
        // global: F1..F6 switch panels
        match key {
            Key::Fn(1) => self.dispatch(Action::SwitchPanel("agents")),
            Key::Fn(2) => self.dispatch(Action::SwitchPanel("goals")),
            Key::Fn(3) => self.dispatch(Action::SwitchPanel("tasks")),
            Key::Fn(4) => self.dispatch(Action::SwitchPanel("files")),
            Key::Fn(5) => self.dispatch(Action::SwitchPanel("terminal")),
            Key::Fn(6) => self.dispatch(Action::SwitchPanel("settings")),
            Key::Esc => {
                if self.overlay != Overlay::None || self.shutdown_confirm {
                    self.dispatch(Action::CloseOverlay);
                }
            }
            Key::Enter if self.panel == "goals" => self.dispatch(Action::SubmitGoal),
            Key::Char(c) => {
                let focused = if self.panel == "goals" {
                    self.goal_input_focused
                } else {
                    false
                };
                let h_focused = self.settings_tab == 1 && self.hostname_focused;
                if focused && self.goal_input.len() < 200 {
                    self.goal_input.push(c);
                    self.dirty = true;
                } else if h_focused && self.hostname_input.len() < 63 {
                    self.hostname_input.push(c);
                    self.dirty = true;
                }
            }
            Key::Backspace => {
                if self.panel == "goals" && self.goal_input_focused {
                    self.goal_input.pop();
                    self.dirty = true;
                } else if self.settings_tab == 1 && self.hostname_focused {
                    self.hostname_input.pop();
                    self.dirty = true;
                }
            }
            _ => {}
        }
    }
}

fn status_mark(status: &str) -> (&'static str, Color) {
    // can't borrow ctx.theme here; use thread-agnostic colors
    match status {
        "Done" => ("✓", Color::hex(0x3ddc84)),
        "Running" => ("▶", Color::hex(0x2dd4bf)),
        "Ready" => ("○", Color::hex(0x6366f1)),
        "Blocked" => ("⛔", Color::hex(0xf0564f)),
        "Failed" => ("✕", Color::hex(0xf0564f)),
        _ => ("•", Color::hex(0x8b98a5)),
    }
}

fn join_path(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn fmt_size(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} K", n as f64 / 1024.0)
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.1} M", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} G", n as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

fn fmt_time(secs: u64) -> String {
    // crude UTC date (1970 epoch math, good enough for a file listing)
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

pub fn civil_from_days(z: i64) -> (i64, u64, u64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u64, d as u64)
}

fn fmt_uptime(s: u64) -> String {
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

fn format_utc(_snap: &crate::sysinfo::Snapshot) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d) = civil_from_days((now / 86400) as i64);
    let rem = now % 86400;
    format!(
        "{:02}:{:02} UTC · {:04}-{:02}-{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        y,
        mo,
        d
    )
}

// ---------------------------------------------------------------------------
// Dock icons (small vector marks)
// ---------------------------------------------------------------------------

fn draw_dock_icon(fb: &mut Framebuffer, cx: i32, cy: i32, id: &str, color: Color) {
    match id {
        "agents" => {
            draw_logo(fb, cx, cy, 11, color, color);
        }
        "goals" => {
            for (r, a) in [(11, 0x30), (7, 0x60), (3, 0xff)] {
                fb.circle(cx, cy, r, color.with_alpha(a));
            }
        }
        "tasks" => {
            for i in 0..3 {
                let y = cy - 8 + i * 8;
                fb.fill_rect(cx - 10, y, 14, 2, color);
                fb.fill_rect(cx - 10, y, 2, 6, color);
                fb.fill_rect(cx + 2, y, 2, 6, color);
            }
        }
        "files" => {
            fb.round_rect(cx - 11, cy - 8, 22, 16, 2, color.with_alpha(0x30));
            fb.fill_rect(cx - 11, cy - 8, 9, 3, color);
            fb.rect_outline(cx - 11, cy - 8, 22, 16, color);
        }
        "terminal" => {
            fb.rect_outline(cx - 11, cy - 8, 22, 16, color);
            font::draw_text(fb, Face::MonoBold, 10, cx - 6, cy - 6, ">_", color);
        }
        "settings" => {
            fb.circle(cx, cy, 8, color.with_alpha(0x30));
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::PI / 4.0;
                fb.line(
                    cx + (a.cos() * 8.0) as i32,
                    cy + (a.sin() * 8.0) as i32,
                    cx + (a.cos() * 11.0) as i32,
                    cy + (a.sin() * 11.0) as i32,
                    color,
                );
            }
            fb.circle(cx, cy, 4, color);
        }
        _ => fb.circle(cx, cy, 8, color),
    }
}

fn draw_power_icon(fb: &mut Framebuffer, cx: i32, cy: i32, color: Color) {
    fb.circle(cx, cy, 10, Color::rgb(0, 0, 0).with_alpha(0));
    for i in 0..16 {
        let a0 = (i as f32 + 0.35) * std::f32::consts::PI / 8.0;
        let a1 = (i as f32 + 0.65) * std::f32::consts::PI / 8.0;
        fb.line(
            cx + (a0.cos() * 12.0) as i32,
            cy + (a0.sin() * 12.0) as i32,
            cx + (a1.cos() * 12.0) as i32,
            cy + (a1.sin() * 12.0) as i32,
            color,
        );
    }
    fb.line(cx, cy - 12, cx, cy - 2, color);
}
