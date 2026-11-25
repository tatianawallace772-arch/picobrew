//! Tiny terminal UI: colours, messages, and a lock-free-ish aggregate progress bar.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static QUIET: AtomicBool = AtomicBool::new(false);
static JSON: AtomicBool = AtomicBool::new(false);

pub fn init(quiet: bool, json: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
    JSON.store(json, Ordering::Relaxed);
}

pub fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

pub fn is_json() -> bool {
    JSON.load(Ordering::Relaxed)
}

pub fn color_enabled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn paint(code: &str, s: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String {
    paint("1", s)
}
pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn red(s: &str) -> String {
    paint("31", s)
}
pub fn cyan(s: &str) -> String {
    paint("36", s)
}

/// `==> message` — the main progress step line (like brew).
pub fn step(msg: &str) {
    if is_quiet() {
        return;
    }
    eprintln!("{} {}", cyan("==>"), bold(msg));
}

pub fn note(msg: &str) {
    if is_quiet() {
        return;
    }
    println!("{msg}");
}

pub fn ok(msg: &str) {
    println!("{} {}", green("✓"), msg);
}

pub fn warn(msg: &str) {
    eprintln!("{} {}", yellow("warning:"), msg);
}

pub fn error(msg: &str) {
    eprintln!("{} {}", red("error:"), msg);
}

/// Human readable byte counts: 512 B, 1.4 KiB, 121 MiB…
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else if v >= 100.0 {
        format!("{v:.0} {}", UNITS[u])
    } else if v >= 10.0 {
        format!("{v:.1} {}", UNITS[u])
    } else {
        format!("{v:.2} {}", UNITS[u])
    }
}

pub fn human_dur(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1.0 {
        format!("{:.0}µs", ms * 1000.0)
    } else if ms < 1000.0 {
        format!("{ms:.0}ms")
    } else {
        format!("{:.2}s", ms / 1000.0)
    }
}

/// Aggregate progress across a batch of parallel downloads.
///
/// A single line is redrawn (tty only): `[2/5] 4.1 MiB · 12.4 MiB/s ⠹ jq`.
pub struct Progress {
    state: Mutex<PState>,
    tty: bool,
}

struct PState {
    done: usize,
    total: usize,
    bytes: u64,
    current: String,
    last_render: Instant,
    started: Instant,
}

impl Progress {
    pub fn new(total: usize) -> Self {
        Self {
            state: Mutex::new(PState {
                done: 0,
                total,
                bytes: 0,
                current: String::new(),
                last_render: Instant::now(),
                started: Instant::now(),
            }),
            tty: std::io::stderr().is_terminal() && !is_quiet(),
        }
    }

    pub fn file_start(&self, label: &str) {
        let mut s = self.state.lock().unwrap();
        s.current = label.to_string();
        self.render_locked(&mut s, false);
    }

    pub fn add_bytes(&self, n: u64) {
        let mut s = self.state.lock().unwrap();
        s.bytes += n;
        self.render_locked(&mut s, false);
    }

    pub fn file_done(&self) {
        let mut s = self.state.lock().unwrap();
        s.done += 1;
        self.render_locked(&mut s, true);
    }

    fn render_locked(&self, s: &mut PState, force: bool) {
        if !self.tty {
            return;
        }
        let now = Instant::now();
        if !force && now.duration_since(s.last_render) < Duration::from_millis(70) {
            return;
        }
        s.last_render = now;
        let elapsed = now.duration_since(s.started).as_secs_f64().max(0.001);
        let rate = s.bytes as f64 / elapsed;
        let spin = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
            [s.done % 10];
        let line = format!(
            "\r\x1b[2K{} [{}/{}] {} · {}/s {} {}",
            cyan("⟳"),
            s.done.min(s.total),
            s.total,
            human_bytes(s.bytes),
            human_bytes(rate as u64),
            spin,
            s.current
        );
        let mut err = std::io::stderr();
        let _ = write!(err, "{line}");
        let _ = err.flush();
    }

    pub fn finish(&self) {
        if self.tty {
            let mut err = std::io::stderr();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
        }
    }
}
