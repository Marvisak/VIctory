//! The live probe (`VICTORY_PROBE`, builds with diagnostics only; off when
//! unset or `0`).
//!
//! - `VICTORY_PROBE=1`: every title pipeline is followed for the whole run
//!   ([`coverage`]: how its draws and dispatches ran, or why they did not).
//!   F9, with the window in front, takes a [`snapshot`]: the targets drawn
//!   into between that present and the next are read back, measured (black,
//!   NaN, infinite, magenta, luma) and saved as small previews.
//! - `VICTORY_PROBE=auto:<seconds>`: the same, and one snapshot is also
//!   taken by itself that many seconds after the first present.
//! - `VICTORY_PROBE_DIR=<folder>`: where the reports go (default: `probe` in
//!   the cache folder). Never inside the source tree (docs/asset-policy.md).
//! - `VICTORY_PROBE_XY=<x>,<y>`: that pixel's value in every target read,
//!   scaled from the size of the largest one.
//! - `VICTORY_PROBE_MAX_RT=<n>`: targets a snapshot reads (default 12, at
//!   most 32).
//!
//! `probe-coverage.txt` and `.json` are written every 15 s and at the
//! title's end, each through a temporary file renamed over the last.
//!
//! Switched off, each hook is one relaxed load. Switched on, a draw takes
//! one of 16 short locks.

pub(crate) mod coverage;
mod pixels;
mod snapshot;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

pub(crate) use snapshot::note_target;

/// What `VICTORY_PROBE` asks.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Off,
    On,
    /// On, with one snapshot by itself this many seconds after the first
    /// present.
    Auto(f64),
}

impl Mode {
    fn parse(value: Option<&str>) -> Mode {
        let Some(value) = value.map(str::trim) else {
            return Mode::Off;
        };
        if value.is_empty() || value == "0" {
            return Mode::Off;
        }
        match value
            .strip_prefix("auto:")
            .and_then(|s| s.parse::<f64>().ok())
        {
            Some(seconds) if seconds.is_finite() && seconds >= 0.0 => Mode::Auto(seconds),
            _ => Mode::On,
        }
    }
}

/// `x,y` as `VICTORY_PROBE_XY` gives a pixel.
fn parse_xy(value: &str) -> Option<(u32, u32)> {
    let (x, y) = value.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// `VICTORY_PROBE_MAX_RT`: default 12, between 1 and 32.
fn parse_max_rt(value: Option<&str>) -> usize {
    value
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(12)
        .clamp(1, 32)
}

struct Config {
    mode: Mode,
    xy: Option<(u32, u32)>,
    max_rt: usize,
}

fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let var = |name: &str| std::env::var(name).ok();
        Config {
            mode: Mode::parse(var("VICTORY_PROBE").as_deref()),
            xy: var("VICTORY_PROBE_XY").as_deref().and_then(parse_xy),
            max_rt: parse_max_rt(var("VICTORY_PROBE_MAX_RT").as_deref()),
        }
    })
}

/// The reports' folder, made on first use; `None` (said once) if it cannot be
/// used.
fn folder() -> Option<&'static Path> {
    static FOLDER: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOLDER
        .get_or_init(|| {
            let made = match std::env::var_os("VICTORY_PROBE_DIR").filter(|d| !d.is_empty()) {
                Some(dir) => {
                    let dir = PathBuf::from(dir);
                    victory_core::ensure_outside_repo(&dir)
                        .map_err(anyhow::Error::from)
                        .and_then(|()| Ok(std::fs::create_dir_all(&dir)?))
                        .map(|()| dir)
                }
                None => victory_core::Config::load().and_then(|c| c.cache_subdir("probe")),
            };
            match made {
                Ok(dir) => {
                    tracing::info!(dir = %dir.display(), "probe: reports go here");
                    Some(dir)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "probe: no folder for its reports; none written");
                    None
                }
            }
        })
        .as_deref()
}

/// 0: not read yet; 1: off; 2: on.
static STATE: AtomicU8 = AtomicU8::new(0);

/// Whether the probe is on: one relaxed load once read.
#[inline]
pub(crate) fn on() -> bool {
    match STATE.load(Relaxed) {
        1 => false,
        2 => true,
        _ => first_look(),
    }
}

#[cold]
fn first_look() -> bool {
    let on = config().mode != Mode::Off;
    STATE.store(if on { 2 } else { 1 }, Relaxed);
    if on {
        tracing::info!(mode = ?config().mode, "probe on (VICTORY_PROBE): F9 takes a snapshot");
    }
    on
}

/// One draw or dispatch call of the title's, noted when it goes out of scope:
/// as run if [`Call::ran`] was called, else with how its pipeline stands.
pub(crate) struct Call {
    pipeline: usize,
    ran: bool,
}

impl Call {
    pub(crate) fn new(pipeline: usize) -> Call {
        Call {
            pipeline,
            ran: false,
        }
    }

    /// It reached the host, drawing into `targets` and `depth` (title
    /// resources; none for compute).
    pub(crate) fn ran(&mut self, targets: &[usize], depth: usize) {
        self.ran = true;
        if on() {
            coverage::note(self.pipeline, None);
            snapshot::drawn(self.pipeline, true, targets, depth);
        }
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        if !self.ran && on() {
            let met = crate::pipeline::probe_status(self.pipeline as crate::object::This);
            coverage::note(self.pipeline, Some(met));
            snapshot::drawn(self.pipeline, false, &[], 0);
        }
    }
}

/// At every present: a snapshot asked at the last one ends here; F9 or the
/// clock may ask the next.
pub(crate) fn presented() {
    if !on() {
        return;
    }
    struct Seen {
        first: Instant,
        f9: bool,
        auto_done: bool,
        flushed: Instant,
    }
    static SEEN: Mutex<Option<Seen>> = Mutex::new(None);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = seen.get_or_insert_with(|| Seen {
        first: Instant::now(),
        f9: false,
        auto_done: false,
        flushed: Instant::now(),
    });
    let f9 = f9_down();
    let mut asked = f9 && !seen.f9;
    seen.f9 = f9;
    if let Mode::Auto(at) = config().mode
        && !seen.auto_done
        && seen.first.elapsed().as_secs_f64() >= at
    {
        seen.auto_done = true;
        asked = true;
    }
    if snapshot::recording() {
        snapshot::finish();
    } else if asked {
        snapshot::begin();
    }
    if seen.flushed.elapsed().as_secs() >= 15 && !snapshot::recording() {
        seen.flushed = Instant::now();
        static WRITING: AtomicBool = AtomicBool::new(false);
        if !WRITING.swap(true, Relaxed) {
            let spawned = std::thread::Builder::new()
                .name("victory-probe".into())
                .spawn(|| {
                    write_coverage();
                    WRITING.store(false, Relaxed);
                });
            if spawned.is_err() {
                WRITING.store(false, Relaxed);
            }
        }
    }
}

/// At the title's end, before the process is terminated: the coverage, if
/// the probe was on.
pub fn flush() {
    if STATE.load(Relaxed) == 2 {
        write_coverage();
    }
}

fn write_coverage() {
    // The title's end may come while the periodic write runs.
    static ONE: Mutex<()> = Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(dir) = folder() else { return };
    let report = coverage::report();
    write_whole(&dir.join("probe-coverage.txt"), report.text().as_bytes());
    write_whole(&dir.join("probe-coverage.json"), report.json().as_bytes());
}

/// `body` to `path` through `<path>.tmp` renamed over it: a reader never sees
/// half a report.
fn write_whole(path: &Path, body: &[u8]) {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if let Err(e) = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, path)) {
        tracing::warn!(error = %e, path = %path.display(), "probe: report not written");
    }
}

/// F9 held, with a window of ours in front.
fn f9_down() -> bool {
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_F9};
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let mut process = 0;
    // SAFETY: plain queries; a null window leaves `process` 0.
    unsafe {
        GetAsyncKeyState(i32::from(VK_F9.0)) < 0 && {
            GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut process));
            process == GetCurrentProcessId()
        }
    }
}

/// `text` as a JSON string, quotes included.
fn json_str(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unless_asked() {
        for off in [None, Some(""), Some("0"), Some(" 0 ")] {
            assert_eq!(Mode::parse(off), Mode::Off, "{off:?}");
        }
        assert_eq!(Mode::parse(Some("1")), Mode::On);
        assert_eq!(Mode::parse(Some("auto:75")), Mode::Auto(75.0));
        assert_eq!(Mode::parse(Some("auto:-1")), Mode::On);
        assert_eq!(Mode::parse(Some("auto:soon")), Mode::On);
    }

    #[test]
    fn the_pixel_and_the_limit_read() {
        assert_eq!(parse_xy("12, 34"), Some((12, 34)));
        assert_eq!(parse_xy("12"), None);
        assert_eq!(parse_xy("-1,2"), None);
        assert_eq!(parse_max_rt(None), 12);
        assert_eq!(parse_max_rt(Some("0")), 1);
        assert_eq!(parse_max_rt(Some("99")), 32);
    }

    #[test]
    fn reports_are_written_whole_and_text_is_escaped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.txt");
        write_whole(&path, b"one");
        write_whole(&path, b"two");
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert!(!dir.path().join("r.txt.tmp").exists());
        assert_eq!(json_str("a\"b\\c\u{1}"), "\"a\\\"b\\\\c\\u0001\"");
    }
}
