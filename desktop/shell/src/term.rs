//! Embedded terminal for the AgentOS shell: a real PTY running /bin/sh,
//! with a compact VT/ANSI emulator (subset sufficient for busybox ash,
//! coreutils output, and interactive use).

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Grid + parser
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: u8, // 16-color index
    pub bg: u8,
    pub bold: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            fg: 7,
            bg: 0,
            bold: false,
        }
    }
}

const PAL16: [(u8, u8, u8); 16] = [
    (0x12, 0x16, 0x1b),
    (0xd7, 0x3a, 0x3a),
    (0x4e, 0x9a, 0x06),
    (0xc4, 0xa0, 0x00),
    (0x34, 0x6a, 0xd0),
    (0x75, 0x50, 0x7b),
    (0x0e, 0x9c, 0xb0),
    (0xbf, 0xc7, 0xd5),
    (0x4c, 0x54, 0x5f),
    (0xe0, 0x3b, 0x46),
    (0x63, 0xd4, 0x66),
    (0xed, 0xd4, 0x00),
    (0x55, 0x8b, 0xff),
    (0xb6, 0x74, 0xd0),
    (0x2d, 0xd4, 0xbf),
    (0xf2, 0xf6, 0xfa),
];

pub fn fg_rgb(idx: u8) -> (u8, u8, u8) {
    PAL16[(idx & 15) as usize]
}
pub fn bg_rgb(idx: u8) -> (u8, u8, u8) {
    PAL16[(idx & 15) as usize]
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ground,
    Esc,
    Csi,
    Swallow,
}

pub struct TermCore {
    grid: Vec<Cell>,
    cols: usize,
    rows: usize,
    cur_col: usize,
    cur_row: usize,
    cur_fg: u8,
    cur_bg: u8,
    bold: bool,
    scroll_top: usize,
    scroll_bot: usize,
    mode: Mode,
    csi: Vec<u8>,
    utf8: Vec<u8>,
}

impl TermCore {
    fn new(cols: usize, rows: usize) -> TermCore {
        TermCore {
            grid: vec![Cell::default(); cols * rows],
            cols,
            rows,
            cur_col: 0,
            cur_row: 0,
            cur_fg: 7,
            cur_bg: 0,
            bold: false,
            scroll_top: 0,
            scroll_bot: rows.saturating_sub(1),
            mode: Mode::Ground,
            csi: Vec::new(),
            utf8: Vec::new(),
        }
    }

    fn cell(&mut self, ch: char) {
        if self.cur_col >= self.cols {
            self.cur_col = 0;
            self.lf();
        }
        let idx = self.cur_row * self.cols + self.cur_col;
        self.grid[idx] = Cell {
            ch,
            fg: self.cur_fg,
            bg: self.cur_bg,
            bold: self.bold,
        };
        self.cur_col += 1;
    }

    fn lf(&mut self) {
        if self.cur_row == self.scroll_bot {
            let (cols, top, bot) = (self.cols, self.scroll_top, self.scroll_bot);
            for r in top..bot {
                for c in 0..cols {
                    self.grid[r * cols + c] = self.grid[(r + 1) * cols + c];
                }
            }
            for c in 0..cols {
                self.grid[bot * cols + c] = Cell::default();
            }
        } else if self.cur_row + 1 < self.rows {
            self.cur_row += 1;
        }
    }

    fn cr(&mut self) {
        self.cur_col = 0;
    }

    fn erase_to_eol(&mut self) {
        for c in self.cur_col..self.cols {
            self.grid[self.cur_row * self.cols + c] = Cell::default();
        }
    }

    fn clear_screen(&mut self) {
        for i in 0..self.grid.len() {
            self.grid[i] = Cell::default();
        }
        self.cur_col = 0;
        self.cur_row = 0;
    }

    fn csi(&mut self, params: &[i64], private: bool, final_byte: u8) {
        let p0 = params.first().copied().unwrap_or(0);
        match (private, final_byte) {
            (_, b'A') => self.cur_row = self.cur_row.saturating_sub(p0.max(1) as usize),
            (_, b'B') => self.cur_row = (self.cur_row + p0.max(1) as usize).min(self.rows - 1),
            (_, b'C') => self.cur_col = (self.cur_col + p0.max(1) as usize).min(self.cols - 1),
            (_, b'D') => self.cur_col = self.cur_col.saturating_sub(p0.max(1) as usize),
            (_, b'H') | (_, b'f') => {
                let row = params.first().copied().unwrap_or(1).max(1) as usize - 1;
                let col = params.get(1).copied().unwrap_or(1).max(1) as usize - 1;
                self.cur_row = row.min(self.rows - 1);
                self.cur_col = col.min(self.cols - 1);
            }
            (_, b'J') => {
                if p0 == 2 || p0 == 3 {
                    self.clear_screen();
                } else if p0 == 0 {
                    self.erase_to_eol();
                    for r in (self.cur_row + 1)..self.rows {
                        for c in 0..self.cols {
                            self.grid[r * self.cols + c] = Cell::default();
                        }
                    }
                }
            }
            (_, b'K') => {
                if p0 == 0 {
                    self.erase_to_eol();
                } else if p0 == 1 {
                    for c in 0..=self.cur_col.min(self.cols - 1) {
                        self.grid[self.cur_row * self.cols + c] = Cell::default();
                    }
                } else {
                    for c in 0..self.cols {
                        self.grid[self.cur_row * self.cols + c] = Cell::default();
                    }
                }
            }
            (_, b'm') => {
                if params.is_empty() || params == &[0] {
                    self.cur_fg = 7;
                    self.cur_bg = 0;
                    self.bold = false;
                    return;
                }
                let mut i = 0;
                while i < params.len() {
                    let p = params[i];
                    match p {
                        0 => {
                            self.cur_fg = 7;
                            self.cur_bg = 0;
                            self.bold = false;
                        }
                        1 => self.bold = true,
                        22 => self.bold = false,
                        30..=37 => self.cur_fg = (p - 30) as u8,
                        38 => i += 2, // 5;n indexed color
                        40..=47 => self.cur_bg = (p - 40) as u8,
                        48 => i += 2,
                        90..=97 => self.cur_fg = (p - 90 + 8) as u8,
                        100..=107 => self.cur_bg = (p - 100 + 8) as u8,
                        _ => {}
                    }
                    i += 1;
                }
            }
            (_, b'r') => {
                self.scroll_top = (p0.max(1) as usize - 1).min(self.rows - 1);
                let bot = params.get(1).copied().unwrap_or(self.rows as i64).max(1) as usize;
                self.scroll_bot = (bot - 1).min(self.rows - 1);
            }
            _ => {}
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match self.mode {
                Mode::Ground => self.ground(b),
                Mode::Esc => self.esc(b),
                Mode::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        let buf = std::mem::take(&mut self.csi);
                        let private = matches!(buf.first(), Some(b'?') | Some(b'<') | Some(b'='));
                        let body: &[u8] = if private && !buf.is_empty() {
                            &buf[1..]
                        } else {
                            &buf[..]
                        };
                        let mut params: Vec<i64> = Vec::new();
                        for seg in body.split(|&c| c == b';' || c == b':') {
                            let s: String = seg.iter().map(|&c| c as char).collect();
                            params.push(s.parse().unwrap_or(0));
                        }
                        self.mode = Mode::Ground;
                        self.csi(&params, private, b);
                    } else {
                        if self.csi.len() < 32 {
                            self.csi.push(b);
                        }
                    }
                }
                Mode::Swallow => {
                    // OSC/charset payload: end on BEL or ST
                    if b == 0x07 {
                        self.mode = Mode::Ground;
                    } else if b == 0x1b {
                        self.mode = Mode::Esc;
                    }
                }
            }
        }
    }

    fn ground(&mut self, b: u8) {
        if !self.utf8.is_empty() {
            // collecting a UTF-8 sequence
            if b & 0xc0 == 0x80 {
                self.utf8.push(b);
                let need = match self.utf8[0] {
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    _ => 4,
                };
                if self.utf8.len() >= need {
                    let s = String::from_utf8_lossy(&self.utf8).to_string();
                    if let Some(ch) = s.chars().next() {
                        self.cell(ch);
                    }
                    self.utf8.clear();
                }
                return;
            }
            self.utf8.clear(); // invalid, drop
        }
        match b {
            0x1b => self.mode = Mode::Esc,
            0x0a | 0x0b | 0x0c => self.lf(),
            0x0d => self.cr(),
            b'\t' => {
                for _ in 0..(8 - (self.cur_col % 8)) {
                    self.cell(' ');
                }
            }
            0x08 => {
                if self.cur_col > 0 {
                    self.cur_col -= 1;
                }
            }
            0x07 | 0x00 | 0x0e | 0x0f => {}
            0x80..=0xff => {
                self.utf8.push(b);
                if b & 0xc0 == 0xc0 {
                    return; // leader collected; wait for continuations
                }
                self.utf8.clear();
                self.cell(char::REPLACEMENT_CHARACTER);
            }
            _ => self.cell(b as char),
        }
    }

    fn esc(&mut self, b: u8) {
        match b {
            b'[' => {
                self.mode = Mode::Csi;
                self.csi.clear();
            }
            b']' | b'(' | b')' => self.mode = Mode::Swallow,
            _ => self.mode = Mode::Ground,
        }
    }
}

// ---------------------------------------------------------------------------
// Term handle (shareable between UI thread and reader thread)
// ---------------------------------------------------------------------------

pub struct Term {
    core: Arc<Mutex<TermCore>>,
    master_fd: RawFd,
    pub alive: Arc<AtomicBool>,
    pub changed: Arc<AtomicBool>,
}

#[repr(C)]
struct WinSize {
    row: u16,
    col: u16,
    xpixel: u16,
    ypixel: u16,
}

pub fn spawn(cols: usize, rows: usize, cmd: Option<(&str, &[&str])>) -> Result<Arc<Term>, String> {
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        if master < 0 {
            return Err(format!("posix_openpt: {}", std::io::Error::last_os_error()));
        }
        if libc::grantpt(master) != 0 || libc::unlockpt(master) != 0 {
            return Err("grantpt/unlockpt failed".into());
        }
        let mut name_buf = [0u8; 128];
        if libc::ptsname_r(
            master,
            name_buf.as_mut_ptr() as *mut libc::c_char,
            name_buf.len(),
        ) != 0
        {
            return Err("ptsname_r failed".into());
        }
        let end = name_buf
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(name_buf.len());
        let slave_name = String::from_utf8_lossy(&name_buf[..end]).to_string();

        let ws = WinSize {
            row: rows as u16,
            col: cols as u16,
            xpixel: 0,
            ypixel: 0,
        };
        libc::ioctl(master, libc::TIOCSWINSZ, &ws);

        let pid = libc::fork();
        if pid < 0 {
            return Err("fork failed".into());
        }
        if pid == 0 {
            libc::setsid();
            let c_slave = std::ffi::CString::new(slave_name).unwrap();
            let slave = libc::open(c_slave.as_ptr(), libc::O_RDWR);
            if slave < 0 {
                libc::_exit(127);
            }
            libc::ioctl(slave, libc::TIOCSCTTY, 0u64);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            if slave > 2 {
                libc::close(slave);
            }
            libc::close(master);
            let (prog, args): (&str, &[&str]) = cmd.unwrap_or(("/bin/busybox", &["sh", "-l"]));
            let c_prog = std::ffi::CString::new(prog).unwrap();
            let c_args: Vec<std::ffi::CString> = std::iter::once(prog.to_string())
                .chain(args.iter().map(|s| s.to_string()))
                .map(std::ffi::CString::new)
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let mut argv: Vec<*const libc::c_char> = c_args.iter().map(|a| a.as_ptr()).collect();
            argv.push(std::ptr::null());
            let env = [
                std::ffi::CString::new("TERM=linux").unwrap(),
                std::ffi::CString::new("PATH=/bin:/sbin:/usr/bin:/usr/sbin").unwrap(),
                std::ffi::CString::new("PS1=agentos # ").unwrap(),
            ];
            let mut envp: Vec<*const libc::c_char> = env.iter().map(|e| e.as_ptr()).collect();
            envp.push(std::ptr::null());
            libc::execve(c_prog.as_ptr(), argv.as_ptr(), envp.as_ptr());
            libc::_exit(127);
        }

        let core = Arc::new(Mutex::new(TermCore::new(cols, rows)));
        let alive = Arc::new(AtomicBool::new(true));
        let changed = Arc::new(AtomicBool::new(true));
        let term = Arc::new(Term {
            core: core.clone(),
            master_fd: master,
            alive: alive.clone(),
            changed: changed.clone(),
        });

        std::thread::spawn(move || {
            let mut f = File::from_raw_fd(master);
            let mut buf = [0u8; 4096];
            loop {
                match f.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        core.lock().unwrap().feed(&buf[..n]);
                        changed.store(true, Ordering::Release);
                    }
                }
            }
            alive.store(false, Ordering::Release);
        });
        Ok(term)
    }
}

pub struct Snapshot {
    pub grid: Vec<Cell>,
    pub cols: usize,
    pub rows: usize,
    pub cur_col: usize,
    pub cur_row: usize,
}

impl Term {
    pub fn snapshot(&self) -> Snapshot {
        let c = self.core.lock().unwrap();
        Snapshot {
            grid: c.grid.clone(),
            cols: c.cols,
            rows: c.rows,
            cur_col: c.cur_col,
            cur_row: c.cur_row,
        }
    }

    pub fn write_input(&self, bytes: &[u8]) {
        let mut f = unsafe { File::from_raw_fd(libc::dup(self.master_fd)) };
        let _ = f.write_all(bytes);
        let _ = f.sync_all();
    }

    pub fn resize(&self, cols: usize, rows: usize) {
        if cols == 0 || rows == 0 {
            return;
        }
        {
            let mut c = self.core.lock().unwrap();
            if c.cols == cols && c.rows == rows {
                return;
            }
            let mut grid = vec![Cell::default(); cols * rows];
            for r in 0..c.rows.min(rows) {
                for cc in 0..c.cols.min(cols) {
                    grid[r * cols + cc] = c.grid[r * c.cols + cc];
                }
            }
            c.grid = grid;
            c.cols = cols;
            c.rows = rows;
            c.scroll_top = 0;
            c.scroll_bot = rows - 1;
            c.cur_row = c.cur_row.min(rows - 1);
            c.cur_col = c.cur_col.min(cols - 1);
        }
        unsafe {
            let ws = WinSize {
                row: rows as u16,
                col: cols as u16,
                xpixel: 0,
                ypixel: 0,
            };
            libc::ioctl(self.master_fd, libc::TIOCSWINSZ, &ws);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: emulator correctness without a PTY
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn core(cols: usize, rows: usize) -> TermCore {
        TermCore::new(cols, rows)
    }

    #[test]
    fn plain_text_wraps_and_tracks_cursor() {
        let mut c = core(10, 4);
        c.feed(b"hello");
        assert_eq!(c.cur_col, 5);
        c.feed(b" world!!!"); // exactly fills 10 then wraps? 10 wide: "hello worl" -> wrap at col 10
        assert_eq!(c.grid[0 * 10 + 9].ch, 'l');
        assert!(c.cur_row >= 1 || c.cur_col < 10);
    }

    #[test]
    fn newline_carriage_return_and_backspace() {
        let mut c = core(20, 4);
        c.feed(b"abc\r\nxyz");
        assert_eq!(c.grid[0].ch, 'a');
        assert_eq!(c.grid[1 * 20].ch, 'x');
        c.feed(b"\x08\x08");
        assert_eq!(c.cur_col, 1);
        c.feed(b"Q");
        assert_eq!(c.grid[1 * 20 + 1].ch, 'Q');
    }

    #[test]
    fn ansi_colors_and_clear() {
        let mut c = core(20, 4);
        c.feed(b"\x1b[31mred\x1b[0m plain");
        assert_eq!(c.grid[0].fg, 1);
        assert_eq!(c.grid[4].fg, 7);
        c.feed(b"\x1b[2J");
        assert!(c.grid.iter().all(|cell| cell.ch == ' '));
    }

    #[test]
    fn cursor_moves_and_erase_to_eol() {
        let mut c = core(20, 4);
        c.feed(b"0123456789\x1b[4D\x1b[K");
        assert_eq!(c.grid[6].ch, ' ');
        assert_eq!(c.grid[5].ch, '5');
        assert_eq!(c.cur_col, 6);
    }

    #[test]
    fn scroll_at_bottom() {
        let mut c = core(5, 3);
        c.feed(b"aaa\r\nbbb\r\nccc\r\nddd");
        // "ddd" pushed everything up by one
        assert_eq!(c.grid[0].ch, 'b');
        assert_eq!(c.grid[5].ch, 'c');
        assert_eq!(c.grid[10].ch, 'd');
    }

    #[test]
    fn utf8_multibyte() {
        let mut c = core(10, 3);
        c.feed("héllo".as_bytes());
        assert_eq!(c.grid[0].ch, 'h');
        assert_eq!(c.grid[1].ch, 'é');
        assert_eq!(c.grid[2].ch, 'l');
    }

    #[test]
    fn osc_sequences_are_swallowed() {
        let mut c = core(10, 3);
        c.feed(b"\x1b]0;title\x07ok");
        assert_eq!(c.grid[0].ch, 'o');
        assert_eq!(c.grid[1].ch, 'k');
    }
}
