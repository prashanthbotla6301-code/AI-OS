//! UI kit for the shell: theme, layout constants, widget primitives, and the
//! hit-region list rebuilt each frame (immediate-mode style).

use crate::fb::{Color, Framebuffer};
use crate::font::{self, Face};

pub const TOPBAR_H: i32 = 64;
pub const DOCK_W: i32 = 100;
pub const BOTBAR_H: i32 = 30;
pub const PAD: i32 = 16;

pub struct Theme {
    pub bg: Color,
    pub panel: Color,
    pub card: Color,
    pub card_hover: Color,
    pub border: Color,
    pub text: Color,
    pub text_muted: Color,
    pub accent: Color,
    pub accent2: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    pub chip_bg: Color,
}

impl Theme {
    pub fn new(accent_hex: u32) -> Theme {
        Theme {
            bg: Color::hex(0x0b0f14),
            panel: Color::hex(0x11161d),
            card: Color::hex(0x161d26),
            card_hover: Color::hex(0x1c2430),
            border: Color::hex(0x1f2937),
            text: Color::hex(0xe6edf3),
            text_muted: Color::hex(0x8b98a5),
            accent: Color::hex(accent_hex),
            accent2: Color::hex(0x6366f1),
            ok: Color::hex(0x3ddc84),
            warn: Color::hex(0xf5c542),
            err: Color::hex(0xf0564f),
            chip_bg: Color::hex(0x0e131a),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Action {
    SwitchPanel(&'static str),
    OpenStatus,
    CloseOverlay,
    AgentStart(String),
    AgentStop(String),
    AgentInstall(String),
    OpenInstallModal,
    RefreshAgents,
    SubmitGoal,
    SuggestGoal(&'static str),
    FileEnter(String),
    FileUp,
    FileRefresh,
    HostnameApply,
    ServiceRestart(String),
    Accent(u32),
    Shutdown,
    Reboot,
    ConfirmShutdown,
    CancelModal,
    ScrollTerminal(i32),
    TerminalClear,
}

pub struct Hit {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub action: Action,
}

#[derive(Default)]
pub struct Hits {
    pub rects: Vec<Hit>,
}

impl Hits {
    pub fn add(&mut self, x: i32, y: i32, w: i32, h: i32, action: Action) {
        self.rects.push(Hit { x, y, w, h, action });
    }
    pub fn find(&self, x: i32, y: i32) -> Option<&Hit> {
        self.rects
            .iter()
            .rev()
            .find(|h| x >= h.x && x < h.x + h.w && y >= h.y && y < h.y + h.h)
    }
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

pub struct Ctx<'a> {
    pub fb: &'a mut Framebuffer,
    pub theme: &'a Theme,
    pub hits: &'a mut Hits,
    pub mouse: (i32, i32),
}

pub fn button(
    ctx: &mut Ctx,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    label: &str,
    enabled: bool,
    action: Action,
) {
    let t = &ctx.theme;
    let hovered =
        ctx.mouse.0 >= x && ctx.mouse.0 < x + w && ctx.mouse.1 >= y && ctx.mouse.1 < y + h;
    let fill = if !enabled {
        t.card
    } else if hovered {
        t.accent.with_alpha(0x28)
    } else {
        t.card
    };
    ctx.fb.round_rect(x, y, w, h, 6, fill);
    ctx.fb.rect_outline(
        x,
        y,
        w,
        h,
        if enabled {
            t.border
        } else {
            t.border.with_alpha(0x60)
        },
    );
    let color = if enabled {
        t.text
    } else {
        t.text_muted.with_alpha(0x70)
    };
    let tw = font::text_width(Face::SansBold, 12, label);
    font::draw_text(
        ctx.fb,
        Face::SansBold,
        12,
        x + (w - tw) / 2,
        y + (h - font::line_height(Face::SansBold, 12)) / 2,
        label,
        color,
    );
    if enabled {
        ctx.hits.add(x, y, w, h, action);
    }
}

pub fn chip(ctx: &mut Ctx, x: i32, y: i32, label: &str, fg: Color, bg: Color, px: u16) -> i32 {
    let w = font::text_width(Face::Sans, px, label) + 14;
    let h = font::line_height(Face::Sans, px) + 6;
    ctx.fb.round_rect(x, y, w, h, (h / 2).min(8), bg);
    font::draw_text(ctx.fb, Face::Sans, px, x + 7, y + 3, label, fg);
    w
}

pub fn badge(ctx: &mut Ctx, x: i32, y: i32, state: &str) -> i32 {
    let (label, color) = match state {
        "Running" => ("● running", ctx.theme.ok),
        "Ready" => ("● ready", ctx.theme.accent),
        "Starting" => ("● starting", ctx.theme.warn),
        "Installed" => ("● installed", ctx.theme.text_muted),
        "Blocked" => ("⛔ blocked", ctx.theme.err),
        "Failed" => ("✕ failed", ctx.theme.err),
        "Stopped" => ("■ stopped", ctx.theme.text_muted),
        "Suspended" => ("‖ suspended", ctx.theme.warn),
        "Stopping" => ("■ stopping", ctx.theme.warn),
        other => (other, ctx.theme.text_muted),
    };
    chip(ctx, x, y, label, color, ctx.theme.chip_bg, 11)
}

pub fn card(ctx: &mut Ctx, x: i32, y: i32, w: i32, h: i32) {
    let hovered =
        ctx.mouse.0 >= x && ctx.mouse.0 < x + w && ctx.mouse.1 >= y && ctx.mouse.1 < y + h;
    let fill = if hovered {
        ctx.theme.card_hover
    } else {
        ctx.theme.card
    };
    ctx.fb.round_rect(x, y, w, h, 10, fill);
    ctx.fb.rect_outline(x, y, w, h, ctx.theme.border);
}

pub fn panel_header(ctx: &mut Ctx, x: i32, y: i32, w: i32, title: &str, subtitle: &str) {
    font::draw_text(ctx.fb, Face::SansBold, 20, x, y, title, ctx.theme.text);
    font::draw_text(
        ctx.fb,
        Face::Sans,
        12,
        x,
        y + 30,
        subtitle,
        ctx.theme.text_muted,
    );
    ctx.fb.fill_rect(x, y + 52, w, 1, ctx.theme.border);
}

/// Single-line text field with caret.
pub fn text_field(
    ctx: &mut Ctx,
    x: i32,
    y: i32,
    w: i32,
    text: &str,
    focused: bool,
    placeholder: &str,
) {
    let h = 40;
    ctx.fb.round_rect(x, y, w, h, 8, ctx.theme.chip_bg);
    ctx.fb.rect_outline(
        x,
        y,
        w,
        h,
        if focused {
            ctx.theme.accent
        } else {
            ctx.theme.border
        },
    );
    let display = if text.is_empty() && !focused {
        font::draw_text_clipped(
            ctx.fb,
            Face::Sans,
            13,
            x + 10,
            y + (h - font::line_height(Face::Sans, 13)) / 2,
            placeholder,
            ctx.theme.text_muted.with_alpha(0x60),
            w - 20,
        );
        return;
    } else {
        text
    };
    let mut dx = x + 10;
    let max_w = w - 24;
    let mut shown = String::new();
    for c in display.chars().rev() {
        let add = if c.is_control() { 1 } else { 0 };
        let _ = add;
        let mut cand = c.to_string();
        cand.push_str(&shown);
        if font::text_width(Face::Sans, 13, &cand) > max_w {
            break;
        }
        shown = cand;
    }
    // reverse back
    let shown: String = shown.chars().rev().collect();
    let _ = &mut dx;
    font::draw_text(
        ctx.fb,
        Face::Sans,
        13,
        dx,
        y + (h - font::line_height(Face::Sans, 13)) / 2,
        &shown,
        ctx.theme.text,
    );
    if focused {
        let cw = font::text_width(Face::Sans, 13, &shown).min(max_w);
        let cy = y + 9;
        ctx.fb
            .fill_rect(x + 10 + cw + 1, cy, 2, h - 18, ctx.theme.accent);
    }
}

/// A row in a list/table.
pub fn list_row(
    ctx: &mut Ctx,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    selected: bool,
    action: Option<Action>,
) {
    let hovered =
        ctx.mouse.0 >= x && ctx.mouse.0 < x + w && ctx.mouse.1 >= y && ctx.mouse.1 < y + h;
    let fill = if selected {
        ctx.theme.accent.with_alpha(0x22)
    } else if hovered {
        ctx.theme.card_hover
    } else {
        Color::rgb(0, 0, 0)
    };
    if fill.3 == 0 {
        ctx.fb.round_rect(x, y, w, h, 6, ctx.theme.card);
    } else {
        ctx.fb.round_rect(x, y, w, h, 6, fill);
    }
    if let Some(a) = action {
        ctx.hits.add(x, y, w, h, a);
    }
}

/// The AgentOS logo mark: hexagon outline with an inner agent-node lattice.
pub fn draw_logo(fb: &mut Framebuffer, cx: i32, cy: i32, r: i32, accent: Color, accent2: Color) {
    // hexagon vertices
    let pts: [(f32, f32); 6] = {
        let mut p = [(0f32, 0f32); 6];
        for (i, slot) in p.iter_mut().enumerate() {
            let a = (i as f32) * std::f32::consts::PI / 3.0 - std::f32::consts::PI / 6.0;
            *slot = ((a.cos() * r as f32), (a.sin() * r as f32));
        }
        p
    };
    // filled hexagon (scanline)
    for dy in -r..=r {
        let y = cy + dy;
        let mut min = r as f32;
        let mut max = -(r as f32);
        for i in 0..6 {
            let (x0, y0) = pts[i];
            let (x1, y1) = pts[(i + 1) % 6];
            if (y0 - dy as f32).abs() < 8.0
                || (y1 - dy as f32).abs() < 8.0
                || (y0 <= dy as f32 && y1 >= dy as f32)
                || (y1 <= dy as f32 && y0 >= dy as f32)
            {
                // x at intersection
                if (y1 - y0).abs() > 0.001 {
                    let t = (dy as f32 - y0) / (y1 - y0);
                    if (0.0..=1.0).contains(&t) {
                        let x = x0 + t * (x1 - x0);
                        min = min.min(x);
                        max = max.max(x);
                    }
                }
            }
        }
        if max > min {
            for dx in (min.ceil() as i32)..=(max.floor() as i32) {
                fb.set_px(cx + dx, y, accent.with_alpha(0x2a));
            }
        }
    }
    // outline
    for i in 0..6 {
        let (x0, y0) = pts[i];
        let (x1, y1) = pts[(i + 1) % 6];
        fb.line(
            cx + x0 as i32,
            cy + y0 as i32,
            cx + x1 as i32,
            cy + y1 as i32,
            accent,
        );
    }
    // inner lattice: center node + 3 spokes
    for &(dx, dy) in &[
        (0, 0),
        (0, -(r / 2)),
        (r * 3 / 5 - 1, r / 4),
        (-(r * 3 / 5) + 1, r / 4),
    ] {
        fb.circle(cx + dx, cy + dy, (r / 7).max(2), accent2);
        if dx == 0 && dy == 0 {
            continue;
        }
        fb.line(cx, cy, cx + dx, cy + dy, accent2.with_alpha(0x90));
    }
}

pub fn draw_cursor(fb: &mut Framebuffer, x: i32, y: i32) {
    // simple arrow
    let rows: [(i32, i32); 8] = [
        (0, 2),
        (1, 4),
        (2, 6),
        (3, 8),
        (4, 10),
        (5, 7),
        (8, 6),
        (10, 5),
    ];
    for (i, (dy, w)) in rows.iter().enumerate() {
        fb.fill_rect(x + i as i32, y + *dy, *w, 2, Color::rgb(250, 251, 253));
    }
    fb.fill_rect(x + 1, y + 12, 5, 2, Color::rgb(250, 251, 253));
}
