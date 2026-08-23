//! Font loading, glyph caching and text drawing (DejaVu family, embedded).

use crate::fb::{Color, Framebuffer};
use ab_glyph::{point, Font, FontVec, Glyph, PxScale, ScaleFont};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub const SANS: &[u8] = include_bytes!("../../../rootfs/usr/share/agentos/fonts/DejaVuSans.ttf");
pub const SANS_BOLD: &[u8] =
    include_bytes!("../../../rootfs/usr/share/agentos/fonts/DejaVuSans-Bold.ttf");
pub const MONO: &[u8] =
    include_bytes!("../../../rootfs/usr/share/agentos/fonts/DejaVuSansMono.ttf");
pub const MONO_BOLD: &[u8] =
    include_bytes!("../../../rootfs/usr/share/agentos/fonts/DejaVuSansMono-Bold.ttf");

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Face {
    Sans,
    SansBold,
    Mono,
    MonoBold,
}

struct CachedGlyph {
    bmp: Vec<u8>,
    w: usize,
    h: usize,
    x0: i32,
    y0: i32,
    advance: f32,
}

pub struct Fonts {
    sans: FontVec,
    sans_bold: FontVec,
    mono: FontVec,
    mono_bold: FontVec,
    cache: Mutex<HashMap<(Face, char, u16), Arc<CachedGlyph>>>,
}

static FONTS: OnceLock<Fonts> = OnceLock::new();

pub fn init() -> Result<(), String> {
    if FONTS.get().is_some() {
        return Ok(());
    }
    let sans = FontVec::try_from_vec(SANS.to_vec()).map_err(|e| e.to_string())?;
    let sans_bold = FontVec::try_from_vec(SANS_BOLD.to_vec()).map_err(|e| e.to_string())?;
    let mono = FontVec::try_from_vec(MONO.to_vec()).map_err(|e| e.to_string())?;
    let mono_bold = FontVec::try_from_vec(MONO_BOLD.to_vec()).map_err(|e| e.to_string())?;
    let _ = FONTS.set(Fonts {
        sans,
        sans_bold,
        mono,
        mono_bold,
        cache: Mutex::new(HashMap::new()),
    });
    Ok(())
}

fn fonts() -> &'static Fonts {
    FONTS.get().expect("fonts not initialised")
}

fn font(face: Face) -> &'static FontVec {
    let f = fonts();
    match face {
        Face::Sans => &f.sans,
        Face::SansBold => &f.sans_bold,
        Face::Mono => &f.mono,
        Face::MonoBold => &f.mono_bold,
    }
}

/// Baseline offset from the top of a text line, in pixels.
pub fn ascent(face: Face, px: u16) -> i32 {
    font(face).as_scaled(px as f32).ascent().ceil() as i32
}

pub fn line_height(face: Face, px: u16) -> i32 {
    let f = font(face).as_scaled(px as f32);
    (f.ascent() - f.descent() + f.line_gap()).ceil() as i32
}

fn get_glyph(face: Face, ch: char, px: u16) -> Option<Arc<CachedGlyph>> {
    let key = (face, ch, px);
    if let Some(g) = fonts().cache.lock().unwrap().get(&key) {
        return Some(g.clone());
    }
    let font = font(face);
    let gid = font.glyph_id(ch);
    if gid.0 == 0 && ch != '\0' {
        return None; // no glyph in font at all
    }
    let scaled = font.as_scaled(px as f32);
    let advance = scaled.h_advance(gid);
    let glyph: Glyph = gid.with_scale_and_position(PxScale::from(px as f32), point(256.0, 256.0));
    match font.outline_glyph(glyph) {
        Some(outlined) => {
            let bounds = outlined.px_bounds();
            let x0 = bounds.min.x.floor() as i32;
            let y0 = bounds.min.y.floor() as i32;
            let w = (bounds.max.x.ceil() as i32 - x0).max(0) as usize;
            let h = (bounds.max.y.ceil() as i32 - y0).max(0) as usize;
            let mut bmp = vec![0u8; w * h];
            outlined.draw(|x, y, coverage| {
                if (x as usize) < w && (y as usize) < h {
                    bmp[y as usize * w + x as usize] = (coverage * 255.0).clamp(0.0, 255.0) as u8;
                }
            });
            let cached = Arc::new(CachedGlyph {
                bmp,
                w,
                h,
                x0: x0 - 256,
                y0: y0 - 256,
                advance,
            });
            fonts().cache.lock().unwrap().insert(key, cached.clone());
            Some(cached)
        }
        None => {
            // no outline (space etc.): advance-only glyph
            let cached = Arc::new(CachedGlyph {
                bmp: Vec::new(),
                w: 0,
                h: 0,
                x0: 0,
                y0: 0,
                advance,
            });
            fonts().cache.lock().unwrap().insert(key, cached.clone());
            Some(cached)
        }
    }
}

/// Draw text with top-left at (x, y). Returns total advance width.
pub fn draw_text(
    fb: &mut Framebuffer,
    face: Face,
    px: u16,
    x: i32,
    y: i32,
    text: &str,
    color: Color,
) -> i32 {
    let base = y + ascent(face, px);
    let mut cx = x as f32;
    for ch in text.chars() {
        let g = match get_glyph(face, ch, px).or_else(|| get_glyph(face, '?', px)) {
            Some(g) => g,
            None => {
                cx += (px as f32) * 0.6;
                continue;
            }
        };
        fb.blit_aa((cx as i32) + g.x0, base + g.y0, &g.bmp, g.w, g.h, color);
        cx += g.advance;
    }
    cx.ceil() as i32 - x
}

/// Draw text clipped to max_w (truncates with an ellipsis).
pub fn draw_text_clipped(
    fb: &mut Framebuffer,
    face: Face,
    px: u16,
    x: i32,
    y: i32,
    text: &str,
    color: Color,
    max_w: i32,
) {
    if text_width(face, px, text) <= max_w {
        draw_text(fb, face, px, x, y, text, color);
        return;
    }
    let mut t = text.to_string();
    while t.len() > 1 && text_width(face, px, &(t.clone() + "…")) > max_w {
        t.pop();
    }
    draw_text(fb, face, px, x, y, &(t + "…"), color);
}

pub fn text_width(face: Face, px: u16, text: &str) -> i32 {
    let mut w = 0f32;
    for ch in text.chars() {
        if let Some(g) = get_glyph(face, ch, px).or_else(|| get_glyph(face, '?', px)) {
            w += g.advance;
        }
    }
    w.ceil() as i32
}
