//! Raw evdev input: keyboards, mice (PS/2 rel), tablets (ABS), merged into
//! one event stream for the shell. Read directly with libc — no input stack.

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;

pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const REL_X: u16 = 0x00;
pub const REL_Y: u16 = 0x01;
pub const REL_WHEEL: u16 = 0x08;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Key {
    Char(char),
    Ctrl(u8),
    Enter,
    Backspace,
    Tab,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    Fn(u8),
}

#[derive(Debug, Clone, Copy)]
pub struct InputEvent {
    pub tv_sec: i64,
    pub tv_usec: i64,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

const INPUT_EVENT_SIZE: usize = 24; // timeval(16) + type(2) + code(2) + value(4)

pub struct InputDevice {
    _file: File,
    pub fd: i32,
    pub is_pointer: bool,
    pub is_keyboard: bool,
    abs_max: (i32, i32),
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct AbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

fn eviocgabs(abs: u32) -> u64 {
    // _IOR('E', 0x40 + abs, struct input_absinfo)
    0x8018_4000u64 + 0x40 + abs as u64
}

pub struct Input {
    pub devices: Vec<InputDevice>,
    pub xres: i32,
    pub yres: i32,
    pub pointers_abs: bool,
    shift: bool,
    ctrl: bool,
}

pub enum Ev {
    Move(i32, i32),
    Button { left: bool, pressed: bool },
    Wheel(i32),
    Key(Key, bool),
}

impl Input {
    pub fn open(xres: i32, yres: i32) -> Input {
        let mut devices = Vec::new();
        let mut abs_pointer = false;
        if let Ok(rd) = std::fs::read_dir("/dev/input") {
            let mut paths: Vec<_> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().contains("event"))
                .collect();
            paths.sort();
            for path in paths {
                if let Ok(f) = OpenOptions::new().read(true).open(&path) {
                    let fd = f.as_raw_fd();
                    // grab capabilities via EVIOCGBIT-lite: just try reading; classify
                    // by presence of ABS axes / REL / keys using bitmasks is heavier
                    // than needed — classify by name first, fall back generously.
                    let name = read_dev_name(fd);
                    let is_pointer = name.contains("Tablet")
                        || name.contains("Mouse")
                        || name.contains("touchpad")
                        || name.contains("Touchpad")
                        || name.contains("Pointing");
                    let is_keyboard =
                        name.contains("Keyboard") || name.contains("keyboard") || name.is_empty();
                    let mut abs_max = (0i32, 0i32);
                    if is_pointer {
                        let mut ax = AbsInfo::default();
                        let mut ay = AbsInfo::default();
                        unsafe {
                            if libc::ioctl(fd, eviocgabs(0) as libc::c_ulong, &mut ax) == 0 {
                                abs_max.0 = ax.maximum.max(1);
                            }
                            if libc::ioctl(fd, eviocgabs(1) as libc::c_ulong, &mut ay) == 0 {
                                abs_max.1 = ay.maximum.max(1);
                            }
                        }
                        if abs_max.0 > 1 && abs_max.1 > 1 {
                            abs_pointer = true;
                        }
                    }
                    // non-blocking
                    unsafe {
                        let flags = libc::fcntl(fd, libc::F_GETFL);
                        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                    }
                    devices.push(InputDevice {
                        _file: f,
                        fd,
                        is_pointer,
                        is_keyboard,
                        abs_max,
                    });
                }
            }
        }
        Input {
            devices,
            xres,
            yres,
            pointers_abs: abs_pointer,
            shift: false,
            ctrl: false,
        }
    }

    pub fn poll_fds(&self) -> Vec<libc::pollfd> {
        self.devices
            .iter()
            .map(|d| libc::pollfd {
                fd: d.fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect()
    }

    pub fn poll(&mut self, pfd: &mut [libc::pollfd], x: &mut i32, y: &mut i32, out: &mut Vec<Ev>) {
        for (i, d) in self.devices.iter().enumerate() {
            if pfd[i].revents & libc::POLLIN == 0 {
                continue;
            }
            loop {
                let mut buf = [0u8; INPUT_EVENT_SIZE * 8];
                let n =
                    unsafe { libc::read(d.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n <= 0 {
                    break;
                }
                let count = (n as usize) / INPUT_EVENT_SIZE;
                for k in 0..count {
                    let ev = parse_event(&buf[k * INPUT_EVENT_SIZE..(k + 1) * INPUT_EVENT_SIZE]);
                    match (ev.type_, ev.code) {
                        (EV_REL, REL_X) => {
                            *x = (*x + ev.value).clamp(0, self.xres - 1);
                            out.push(Ev::Move(*x, *y));
                        }
                        (EV_REL, REL_Y) => {
                            *y = (*y + ev.value).clamp(0, self.yres - 1);
                            out.push(Ev::Move(*x, *y));
                        }
                        (EV_REL, REL_WHEEL) => out.push(Ev::Wheel(ev.value)),
                        (EV_ABS, ABS_X) => {
                            *x = (ev.value * (self.xres - 1)) / d.abs_max.0.max(1);
                            out.push(Ev::Move(*x, *y));
                        }
                        (EV_ABS, ABS_Y) => {
                            *y = (ev.value * (self.yres - 1)) / d.abs_max.1.max(1);
                            out.push(Ev::Move(*x, *y));
                        }
                        (EV_KEY, BTN_LEFT) => out.push(Ev::Button {
                            left: true,
                            pressed: ev.value == 1,
                        }),
                        (EV_KEY, BTN_RIGHT) => out.push(Ev::Button {
                            left: false,
                            pressed: ev.value == 1,
                        }),
                        (EV_KEY, code) => {
                            let pressed = ev.value == 1;
                            let released = ev.value == 0;
                            match code {
                                42 | 54 => {
                                    self.shift = pressed;
                                }
                                29 | 97 => {
                                    self.ctrl = pressed;
                                }
                                110 | 119 => { // insert / sysrq etc: ignore
                                }
                                _ => {
                                    if (pressed || ev.value == 2) && d.is_keyboard {
                                        let key = if self.ctrl {
                                            keycode_to_key(code, false).and_then(|k| match k {
                                                Key::Char(c) => {
                                                    Some(Key::Ctrl(c.to_ascii_lowercase() as u8))
                                                }
                                                other => Some(other),
                                            })
                                        } else {
                                            keycode_to_key(code, self.shift)
                                        };
                                        if let Some(k) = key {
                                            out.push(Ev::Key(k, true));
                                        }
                                    }
                                    let _ = released;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

fn read_dev_name(fd: i32) -> String {
    let mut buf = [0u8; 256];
    // EVIOCGNAME(len) = _IOR('E', 0x06, char[len]) => 0x8000_0500 | len... precisely:
    // _IOR(EVDEV, 0x06, buf) : size=256 dir=read type 0x45 nr 0x06 -> (2<<30)|(256<<16)|(0x45<<8)|0x06
    let cmd: libc::c_ulong = (2u64 << 30) | ((buf.len() as u64) << 16) | (0x45 << 8) | 0x06;
    let r = unsafe { libc::ioctl(fd, cmd as libc::c_ulong, buf.as_mut_ptr()) };
    if r > 0 {
        let end = buf[..r as usize]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(r as usize);
        String::from_utf8_lossy(&buf[..end]).to_string()
    } else {
        String::new()
    }
}

fn parse_event(b: &[u8]) -> InputEvent {
    InputEvent {
        tv_sec: i64::from_le_bytes(b[0..8].try_into().unwrap()),
        tv_usec: i64::from_le_bytes(b[8..16].try_into().unwrap()),
        type_: u16::from_le_bytes(b[16..18].try_into().unwrap()),
        code: u16::from_le_bytes(b[18..20].try_into().unwrap()),
        value: i32::from_le_bytes(b[20..24].try_into().unwrap()),
    }
}

// Minimal US keymap (enough for goals/hostname input + terminal).
pub fn keycode_to_key(code: u16, shift: bool) -> Option<Key> {
    use Key::*;
    Some(match code {
        1 => Esc,
        14 => Backspace,
        15 => Tab,
        28 | 96 => Enter,
        102 => Home,
        107 => End,
        103 => Up,
        108 => Down,
        105 => Left,
        106 => Right,
        111 => Delete,
        59 => Fn(1),
        60 => Fn(2),
        61 => Fn(3),
        62 => Fn(4),
        63 => Fn(5),
        64 => Fn(6),
        2..=11 => {
            let digits = ['1', '2', '3', '4', '5', '6', '7', '8', '9', '0'];
            let shifted = ['!', '@', '#', '$', '%', '^', '&', '*', '(', ')'];
            let i = (code - 2) as usize;
            Char(if shift { shifted[i] } else { digits[i] })
        }
        12 => Char(if shift { '_' } else { '-' }),
        13 => Char(if shift { '+' } else { '=' }),
        16..=25 => {
            let lower = ['q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p'];
            let upper = ['Q', 'W', 'E', 'R', 'T', 'Y', 'U', 'I', 'O', 'P'];
            let i = (code - 16) as usize;
            Char(if shift { upper[i] } else { lower[i] })
        }
        26 => Char(if shift { '{' } else { '[' }),
        27 => Char(if shift { '}' } else { ']' }),
        30..=38 => {
            let lower = ['a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l'];
            let upper = ['A', 'S', 'D', 'F', 'G', 'H', 'J', 'K', 'L'];
            let i = (code - 30) as usize;
            Char(if shift { upper[i] } else { lower[i] })
        }
        39 => Char(if shift { ':' } else { ';' }),
        40 => Char(if shift { '"' } else { '\'' }),
        41 => Char(if shift { '~' } else { '`' }),
        42 | 54 => return None, // shift itself handled by caller
        43 => Char(if shift { '|' } else { '\\' }),
        44..=50 => {
            let lower = ['z', 'x', 'c', 'v', 'b', 'n', 'm'];
            let upper = ['Z', 'X', 'C', 'V', 'B', 'N', 'M'];
            let i = (code - 44) as usize;
            Char(if shift { upper[i] } else { lower[i] })
        }
        51 => Char(if shift { '<' } else { ',' }),
        52 => Char(if shift { '>' } else { '.' }),
        53 => Char(if shift { '?' } else { '/' }),
        57 => Char(' '),
        _ => return None,
    })
}
