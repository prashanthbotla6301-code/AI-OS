//! Framebuffer access and software rasterization for the AgentOS shell.
//!
//! The shell renders directly to `/dev/fb0` (virtio-gpu / bochs DRM fbdev
//! emulation, VESA or EFI framebuffer — whatever the kernel exposes) with no
//! X11/Wayland/compositor in between. This is a Phase 1 decision: it makes
//! the desktop the most reliable thing that can possibly boot, on QEMU and
//! real hardware, with tiny resource usage. A Wayland-based shell is on the
//! roadmap once the OS has a reason to need window management.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Color(pub u8, pub u8, pub u8, pub u8); // r, g, b, a

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color(r, g, b, 255)
    }
    pub const fn hex(v: u32) -> Color {
        Color(
            ((v >> 16) & 0xff) as u8,
            ((v >> 8) & 0xff) as u8,
            (v & 0xff) as u8,
            255,
        )
    }
    pub fn with_alpha(self, a: u8) -> Color {
        Color(self.0, self.1, self.2, a)
    }
    pub fn blend_over(self, dst: Color) -> Color {
        let a = self.3 as u16;
        if a == 255 {
            return self;
        }
        if a == 0 {
            return dst;
        }
        let inv = 255 - a;
        Color(
            ((self.0 as u16 * a + dst.0 as u16 * inv) / 255) as u8,
            ((self.1 as u16 * a + dst.1 as u16 * inv) / 255) as u8,
            ((self.2 as u16 * a + dst.2 as u16 * inv) / 255) as u8,
            255,
        )
    }
}

pub struct Framebuffer {
    _file: File,
    pub xres: u32,
    pub yres: u32,
    pub bpp: u32,
    /// memory-mapped scanout
    pub buf: &'static mut [u8],
    _map_len: usize,
    /// shadow buffer we draw into (avoids tearing)
    pub px: Vec<u8>,
    shadow_stride: usize,
    scan_stride: usize,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FbVarScreenInfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: (u32, i32, u32),
    green: (u32, i32, u32),
    blue: (u32, i32, u32),
    transp: (u32, i32, u32),
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left: u32,
    right: u32,
    upper: u32,
    lower: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

const FBIOGET_VSCREENINFO: u64 = 0x4600;

impl Framebuffer {
    pub fn open(path: &str) -> Result<Framebuffer, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("open {path}: {e}"))?;
        let mut var = FbVarScreenInfo::default();
        let r = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                FBIOGET_VSCREENINFO,
                &mut var as *mut FbVarScreenInfo,
            )
        };
        if r != 0 {
            return Err(format!(
                "FBIOGET_VSCREENINFO failed on {path} (is there a framebuffer?)"
            ));
        }
        if var.bits_per_pixel != 32 && var.bits_per_pixel != 16 {
            return Err(format!(
                "unsupported framebuffer bpp {} (need 32 or 16)",
                var.bits_per_pixel
            ));
        }
        let scan_stride = (var.xres_virtual * (var.bits_per_pixel / 8)) as usize;
        let map_len = scan_stride.saturating_mul(var.yres_virtual as usize);
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err("mmap framebuffer failed".into());
        }
        let buf: &'static mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, map_len) };
        Ok(Framebuffer {
            _file: file,
            xres: var.xres,
            yres: var.yres,
            bpp: var.bits_per_pixel,
            buf,
            _map_len: map_len,
            px: vec![0u8; (var.xres * var.yres * 4) as usize],
            shadow_stride: (var.xres * 4) as usize, // shadow buffer is tightly packed
            scan_stride,
        })
    }

    /// Copy the shadow buffer onto the scanout, converting bpp if needed.
    /// Linux framebuffer byte order is commonly B,G,R,X for 32-bit and
    /// little-endian RGB565 for 16-bit; respect the device stride rather than
    /// assuming virtual width equals visible width.
    pub fn present(&mut self) {
        let width = self.xres as usize;
        let height = self.yres as usize;
        if self.bpp == 32 {
            for row in 0..height {
                let src_row = row * self.shadow_stride;
                let dst_row = row * self.scan_stride;
                for col in 0..width {
                    let src = src_row + col * 4;
                    let dst = dst_row + col * 4;
                    self.buf[dst] = self.px[src + 2];
                    self.buf[dst + 1] = self.px[src + 1];
                    self.buf[dst + 2] = self.px[src];
                    self.buf[dst + 3] = 0;
                }
            }
        } else {
            for row in 0..height {
                let src_row = row * self.shadow_stride;
                let dst_row = row * self.scan_stride;
                for col in 0..width {
                    let src = src_row + col * 4;
                    let r = self.px[src];
                    let g = self.px[src + 1];
                    let b = self.px[src + 2];
                    let value =
                        (((r as u16) >> 3) << 11) | (((g as u16) >> 2) << 5) | ((b as u16) >> 3);
                    let dst = dst_row + col * 2;
                    self.buf[dst] = (value & 0xff) as u8;
                    self.buf[dst + 1] = (value >> 8) as u8;
                }
            }
        }
    }

    #[inline]
    pub fn set_px(&mut self, x: i32, y: i32, c: Color) {
        if x < 0 || y < 0 || x >= self.xres as i32 || y >= self.yres as i32 {
            return;
        }
        let i = (y as usize * self.shadow_stride) + (x as usize * 4);
        let dst = Color(self.px[i], self.px[i + 1], self.px[i + 2], 255);
        let out = c.blend_over(dst);
        self.px[i] = out.0;
        self.px[i + 1] = out.1;
        self.px[i + 2] = out.2;
        self.px[i + 3] = 255;
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: Color) {
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w).min(self.xres as i32);
        let y1 = (y + h).min(self.yres as i32);
        if c.3 == 255 {
            for yy in y0..y1 {
                let row = yy as usize * self.shadow_stride;
                for xx in x0..x1 {
                    let i = row + xx as usize * 4;
                    self.px[i] = c.0;
                    self.px[i + 1] = c.1;
                    self.px[i + 2] = c.2;
                    self.px[i + 3] = 255;
                }
            }
        } else {
            for yy in y0..y1 {
                for xx in x0..x1 {
                    self.set_px(xx, yy, c);
                }
            }
        }
    }

    pub fn rect_outline(&mut self, x: i32, y: i32, w: i32, h: i32, c: Color) {
        self.fill_rect(x, y, w, 1, c);
        self.fill_rect(x, y + h - 1, w, 1, c);
        self.fill_rect(x, y, 1, h, c);
        self.fill_rect(x + w - 1, y, 1, h, c);
    }

    /// Filled rounded rectangle (integer corner radius, cheap and clean).
    pub fn round_rect(&mut self, x: i32, y: i32, w: i32, h: i32, r: i32, c: Color) {
        let r = r.clamp(0, (w / 2).min(h / 2));
        self.fill_rect(x + r, y, w - 2 * r, h, c);
        self.fill_rect(x, y + r, w, h - 2 * r, c);
        self.fill_circle_quarter(x + r, y + r, r, c);
        self.fill_circle_quarter(x + w - r - 1, y + r, r, c);
        self.fill_circle_quarter(x + r, y + h - r - 1, r, c);
        self.fill_circle_quarter(x + w - r - 1, y + h - r - 1, r, c);
    }

    fn fill_circle_quarter(&mut self, cx: i32, cy: i32, r: i32, c: Color) {
        if r <= 0 {
            return;
        }
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r * r {
                    self.set_px(cx + dx, cy + dy, c);
                }
            }
        }
    }

    pub fn circle(&mut self, cx: i32, cy: i32, r: i32, c: Color) {
        if r <= 0 {
            return;
        }
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r * r {
                    self.set_px(cx + dx, cy + dy, c);
                }
            }
        }
    }

    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, c: Color) {
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let (mut x, mut y, mut err) = (x0, y0, dx + dy);
        loop {
            self.set_px(x, y, c);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// Blit an anti-aliased coverage bitmap (from the font cache).
    pub fn blit_aa(&mut self, x: i32, y: i32, bmp: &[u8], w: usize, h: usize, c: Color) {
        for row in 0..h {
            for col in 0..w {
                let cov = bmp[row * w + col];
                if cov > 0 {
                    let a = (cov as u16 * c.3 as u16 / 255) as u8;
                    self.set_px(x + col as i32, y + row as i32, Color(c.0, c.1, c.2, a));
                }
            }
        }
    }

    pub fn clear(&mut self, c: Color) {
        let [r, g, b, _] = [c.0, c.1, c.2, c.3];
        for i in 0..(self.xres as usize * self.yres as usize) {
            let o = i * 4;
            self.px[o] = r;
            self.px[o + 1] = g;
            self.px[o + 2] = b;
            self.px[o + 3] = 255;
        }
    }
}

pub fn fb_error_hang(msg: &str) -> ! {
    // Last-resort error path: write to console + serial and sleep forever so
    // the failure is visible in QEMU logs rather than silently exiting.
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/console")
        .and_then(|mut f| writeln!(f, "[agentos-shell] FATAL: {msg}"));
    eprintln!("[agentos-shell] FATAL: {msg}");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
