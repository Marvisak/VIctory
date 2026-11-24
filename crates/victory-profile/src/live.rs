//! `VICTORY_PROFILE_LIVE=1`: a short line every second (`=frame`: after
//! every presented frame), on the console and in a file flushed to disk at
//! each line, so the last seconds before a device removal (TDR) or a
//! frozen machine are on disk. Between lines, warnings as soon as a
//! command or submission is slower than `VICTORY_PROFILE_LIVE_SLOW_MS`, or
//! the GPU has not moved past a marker for `VICTORY_PROFILE_LIVE_STALL_MS`
//! (a hang on its way).
//!
//! A line: frames, CPU and GPU time per frame, the slowest submission and
//! command of the second, the last GPU marker finished (or the command the
//! GPU is in), dropped and zero-group draws and dispatches per frame, video
//! and shared (upload) memory against their budgets. Markers need a
//! diagnostics build with `VICTORY_D3D12_DRED=1`, command times
//! `VICTORY_GPU_TIMING=1` as well; without them the line says so.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::gpu::Slowest;
use crate::{bytes_text, lock};

static LIVE: AtomicBool = AtomicBool::new(false);
static SETTINGS: OnceLock<LiveSettings> = OnceLock::new();

/// Whether live lines are being written; one relaxed load.
#[inline]
pub fn on() -> bool {
    LIVE.load(Relaxed)
}

/// What `VICTORY_PROFILE_LIVE*` ask for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LiveSettings {
    /// A line per presented frame instead of per second.
    pub(crate) every_frame: bool,
    /// `VICTORY_PROFILE_LIVE=<file>`: where the lines go (else `live.log`
    /// in the profile folder, or in the temporary folder).
    pub(crate) file: Option<PathBuf>,
    /// A command or submission slower than this is warned of (default 100).
    pub(crate) slow_ms: f64,
    /// The GPU on one marker this long is warned of (default 1000; the
    /// host resets the device at about 2 s).
    pub(crate) stall_ms: f64,
}

impl LiveSettings {
    /// `None` unless `live` asks: `1`, `on`, `true`, `frame`, or a file.
    pub(crate) fn parse(
        live: Option<&str>,
        slow_ms: Option<&str>,
        stall_ms: Option<&str>,
    ) -> Option<LiveSettings> {
        let live = live?.trim();
        let positive = |v: Option<&str>, d: f64| {
            v.and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(d)
        };
        let (every_frame, file) = match live.to_ascii_lowercase().as_str() {
            "" | "0" | "off" | "false" => return None,
            "1" | "on" | "true" => (false, None),
            "frame" => (true, None),
            _ => (false, Some(PathBuf::from(live))),
        };
        Some(LiveSettings {
            every_frame,
            file,
            slow_ms: positive(slow_ms, 100.0),
            stall_ms: positive(stall_ms, 1000.0),
        })
    }
}

fn settings() -> &'static LiveSettings {
    static DEFAULT: OnceLock<LiveSettings> = OnceLock::new();
    SETTINGS.get().unwrap_or_else(|| {
        DEFAULT.get_or_init(|| LiveSettings::parse(Some("1"), None, None).expect("defaults"))
    })
}

/// Where the GPU is, as the host knows it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Progress {
    /// The submission being waited for, if one is.
    pub busy: Option<Busy>,
    /// The text of the last marker the GPU finished (diagnostics builds
    /// with `VICTORY_D3D12_DRED`), as a [`crate::gpu::command_key`].
    pub last_done: Option<String>,
}

/// A submission the GPU has not finished.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Busy {
    pub submission: u64,
    /// Since it was handed to the GPU.
    pub for_ms: f64,
    /// The list the GPU is in (of `lists`) and the commands of it that
    /// finished (of `commands`), when its markers say.
    pub list: Option<usize>,
    pub lists: usize,
    pub done: u32,
    pub commands: u32,
    /// The first command not finished.
    pub running: Option<String>,
    /// Some of the pipelines its lists bind.
    pub pipelines: String,
}

static PROBE: OnceLock<fn() -> Progress> = OnceLock::new();

/// The host's way to say where the GPU is (once).
pub fn progress(f: fn() -> Progress) {
    let _ = PROBE.set(f);
}

fn probe() -> Progress {
    PROBE.get().map(|f| f()).unwrap_or_default()
}

/// The worst of the current second.
#[derive(Default)]
struct Window {
    command: Option<(String, f64, f64)>,
    submission: Option<Slowest>,
}

static WINDOW: Mutex<Window> = Mutex::new(Window {
    command: None,
    submission: None,
});

/// A timed command (from [`crate::gpu::command`]).
pub(crate) fn command(key: &str, ms: f64, list_ms: f64) {
    if !on() {
        return;
    }
    {
        let mut w = lock(&WINDOW);
        if w.command.as_ref().is_none_or(|c| ms > c.1) {
            w.command = Some((key.to_owned(), ms, list_ms));
        }
    }
    if ms >= settings().slow_ms {
        warn(&format!(
            "SLOW COMMAND {ms:.1} ms (its list {list_ms:.1} ms): {key}"
        ));
    }
}

/// Whether a submission of `ms` is worth its pipelines' names.
pub(crate) fn wants_submission(ms: f64) -> bool {
    on() && (ms >= settings().slow_ms
        || lock(&WINDOW).submission.as_ref().is_none_or(|s| ms > s.ms))
}

pub(crate) fn submission(s: Slowest) {
    if s.ms >= settings().slow_ms {
        warn(&format!(
            "SLOW SUBMISSION {:.1} ms, {} lists; pipelines {}",
            s.ms, s.lists, s.pipelines
        ));
    }
    let mut w = lock(&WINDOW);
    if w.submission.as_ref().is_none_or(|old| s.ms > old.ms) {
        w.submission = Some(s);
    }
}

static FILE: Mutex<Option<File>> = Mutex::new(None);
/// Warnings written this second, and those left out past the limit.
static WARNINGS: AtomicU32 = AtomicU32::new(0);
static SUPPRESSED: AtomicU32 = AtomicU32::new(0);
const WARNINGS_PER_SECOND: u32 = 10;

/// Writes `line` to the console and the file, and makes the file durable.
fn write(line: &str) {
    let at = crate::START
        .get()
        .map_or(0.0, |s| s.elapsed().as_secs_f64());
    let line = format!("[{at:8.1} s] {line}");
    eprintln!("victory live {line}");
    if let Some(f) = lock(&FILE).as_mut() {
        let _ = writeln!(f, "{line}");
        let _ = f.sync_data();
    }
}

fn warn(text: &str) {
    if WARNINGS.fetch_add(1, Relaxed) < WARNINGS_PER_SECOND {
        write(&format!("WARNING {text}"));
    } else {
        SUPPRESSED.fetch_add(1, Relaxed);
    }
}

/// The host GPU failed (a device removal, say): the reason's first part
/// (what follows holds addresses) and the last marker done, at once.
pub fn failure(reason: &str) {
    if !on() {
        return;
    }
    let first = reason.split("; ").next().unwrap_or(reason);
    let last = probe().last_done;
    write(&format!(
        "FAILURE {} | last marker done: {}",
        short(first, 300),
        last.as_deref().unwrap_or("n/a")
    ));
}

/// What a line shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LineInput {
    /// Frames presented since the last line, the last one's number.
    pub(crate) frames: usize,
    pub(crate) last_frame: u64,
    /// Since the last frame, when none came.
    pub(crate) no_frame_s: f64,
    /// Per frame: CPU time (interval less GPU wait), GPU wait, dropped and
    /// zero-group work.
    pub(crate) cpu_ms: f64,
    pub(crate) gpu_ms: f64,
    pub(crate) dropped: f64,
    pub(crate) zero_groups: f64,
    pub(crate) submission: Option<Slowest>,
    pub(crate) command: Option<(String, f64, f64)>,
    pub(crate) progress: Progress,
    /// `vram.local` and its budget, `vram.shared` and its budget.
    pub(crate) vram: Option<(u64, u64)>,
    pub(crate) shared: Option<(u64, u64)>,
    pub(crate) markers: bool,
    pub(crate) suppressed: u32,
}

fn short(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        text.to_owned()
    } else {
        let mut s: String = text.chars().take(chars - 1).collect();
        s.push('~');
        s
    }
}

/// A live line: ` | `-separated fields, the same ones every time.
pub(crate) fn line(i: &LineInput) -> String {
    let mut f = Vec::new();
    if i.frames > 0 {
        f.push(format!("frames {} (last {})", i.frames, i.last_frame));
        f.push(format!(
            "cpu {:.1} ms gpu {:.1} ms /frame",
            i.cpu_ms, i.gpu_ms
        ));
    } else {
        f.push(format!("no frame for {:.1} s", i.no_frame_s));
    }
    if let Some(b) = &i.progress.busy {
        let mut busy = format!("GPU BUSY {:.0} ms in s{}", b.for_ms, b.submission);
        if let Some(list) = b.list {
            busy.push_str(&format!(
                " list {}/{} at {}/{}",
                list + 1,
                b.lists,
                b.done,
                b.commands
            ));
        }
        f.push(busy);
    }
    f.push(match &i.submission {
        Some(s) => format!(
            "slowest submission {:.1} ms ({} lists) {}",
            s.ms,
            s.lists,
            short(&s.pipelines, 80)
        ),
        None => "no submission".to_owned(),
    });
    f.push(match &i.command {
        Some((key, ms, list_ms)) => {
            format!(
                "slowest command {ms:.1} ms (list {list_ms:.1}) {}",
                short(key, 100)
            )
        }
        None if i.markers => "no timed command (VICTORY_GPU_TIMING=1 times them)".to_owned(),
        None => "commands untimed".to_owned(),
    });
    let running = i.progress.busy.as_ref().and_then(|b| b.running.as_deref());
    f.push(match (running, &i.progress.last_done) {
        (Some(r), _) => format!("GPU in: {}", short(r, 100)),
        (None, Some(d)) => format!("last marker done: {}", short(d, 100)),
        (None, None) if i.markers => "no marker yet".to_owned(),
        (None, None) => "markers off (diagnostics build, VICTORY_D3D12_DRED=1)".to_owned(),
    });
    f.push(format!(
        "dropped {:.1} zero-group {:.1} /frame",
        i.dropped, i.zero_groups
    ));
    let pair = |name: &str, v: Option<(u64, u64)>| match v {
        Some((used, budget)) => format!(
            "{name} {} / {}",
            bytes_text(used as f64),
            bytes_text(budget as f64)
        ),
        None => format!("{name} n/a"),
    };
    f.push(format!(
        "{}, {}",
        pair("vram", i.vram),
        pair("shared", i.shared)
    ));
    if i.suppressed > 0 {
        f.push(format!("{} warnings left out", i.suppressed));
    }
    f.join(" | ")
}

/// A stall warning: the GPU on the same marker for `stalled_ms`.
pub(crate) fn stall_text(b: &Busy, stalled_ms: f64, last_done: Option<&str>) -> String {
    let mut text = format!(
        "STALL: submission s{} busy {:.0} ms, no marker passed for {:.0} ms",
        b.submission, b.for_ms, stalled_ms
    );
    if let Some(list) = b.list {
        text.push_str(&format!(
            "; list {}/{}, {} of {} commands done",
            list + 1,
            b.lists,
            b.done,
            b.commands
        ));
    }
    if let Some(d) = last_done {
        text.push_str(&format!("; last done: {}", short(d, 160)));
    }
    if let Some(r) = &b.running {
        text.push_str(&format!("; stuck in: {}", short(r, 160)));
    }
    if !b.pipelines.is_empty() {
        text.push_str(&format!("; pipelines {}", short(&b.pipelines, 200)));
    }
    text
}

/// A submission, the list the GPU is in and the commands of it finished.
type Seen = (u64, Option<usize>, u32);

/// When to warn of a stall next: at the threshold, then each doubling.
#[derive(Debug, Default)]
pub(crate) struct StallWatch {
    /// Where the GPU was last seen, and since when.
    at: Option<(Seen, Instant)>,
    next_ms: f64,
}

impl StallWatch {
    /// The stall to warn of now, in milliseconds, if any.
    pub(crate) fn check(
        &mut self,
        busy: Option<&Busy>,
        now: Instant,
        stall_ms: f64,
    ) -> Option<f64> {
        let Some(b) = busy else {
            self.at = None;
            return None;
        };
        let key = (b.submission, b.list, b.done);
        let since = match self.at {
            Some((k, since)) if k == key => since,
            _ => {
                // The marker moved: stalled since it did, or since the
                // submission started if it is new.
                let new_submission = self.at.is_none_or(|(k, _)| k.0 != b.submission);
                let since = if new_submission {
                    now.checked_sub(Duration::from_secs_f64(b.for_ms / 1e3))
                        .unwrap_or(now)
                } else {
                    now
                };
                self.at = Some((key, since));
                self.next_ms = stall_ms;
                since
            }
        };
        let stalled = now.saturating_duration_since(since).as_secs_f64() * 1e3;
        (stalled >= self.next_ms).then(|| {
            self.next_ms *= 2.0;
            stalled
        })
    }
}

/// Starts live lines if the variables ask (from [`crate::init`]).
pub(crate) fn start(s: LiveSettings, dir: Option<&Path>) {
    let path = match (&s.file, dir) {
        (Some(file), _) => file.clone(),
        (None, Some(dir)) => dir.join("live.log"),
        (None, None) => std::env::temp_dir().join("victory-live.log"),
    };
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let opened = parent
        .map_or(Ok(()), |p| {
            victory_core::ensure_outside_repo(p).map_err(|e| e.to_string())
        })
        .and_then(|()| {
            if let Some(p) = parent {
                std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            File::create(&path).map_err(|e| e.to_string())
        });
    match opened {
        Ok(f) => *lock(&FILE) = Some(f),
        Err(e) => {
            tracing::warn!(target: "victory_profile", path = %path.display(), error = %e, "live log file not written; console only");
        }
    }
    let _ = SETTINGS.set(s);
    LIVE.store(true, Relaxed);
    let s = settings();
    write(&format!(
        "live profile on: {}; warnings over {:.0} ms, stalls over {:.0} ms; file {}",
        if s.every_frame {
            "a line per frame"
        } else {
            "a line per second"
        },
        s.slow_ms,
        s.stall_ms,
        path.display()
    ));
    let _ = std::thread::Builder::new()
        .name("victory-live".into())
        .spawn(run);
}

/// The live thread: stalls checked ten times a second, lines each second
/// (or each new frame).
fn run() {
    let s = settings();
    let mut watch = StallWatch::default();
    let mut last_line = Instant::now();
    let mut last_frame_time = 0.0;
    let mut last_number = None;
    let mut seen_last = Instant::now();
    // Video memory and shared memory, each used and budget.
    let mut memory: [Option<(u64, u64)>; 2] = [None, None];
    let mut memory_at: Option<Instant> = None;
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let now = Instant::now();
        let progress = probe();
        if let Some(stalled) = watch.check(progress.busy.as_ref(), now, s.stall_ms)
            && let Some(b) = &progress.busy
        {
            write(&format!(
                "WARNING {}",
                stall_text(b, stalled, progress.last_done.as_deref())
            ));
        }
        let rows: Vec<crate::FrameRow> = crate::frames().as_ref().map_or_else(Vec::new, |f| {
            f.recent
                .iter()
                .filter(|r| r.time_s > last_frame_time)
                .copied()
                .collect()
        });
        let due = if s.every_frame {
            !rows.is_empty() || last_line.elapsed() >= Duration::from_secs(1)
        } else {
            last_line.elapsed() >= Duration::from_secs(1)
        };
        if !due {
            continue;
        }
        if let Some(last) = rows.last() {
            last_frame_time = last.time_s;
            last_number = Some(last.number);
            seen_last = now;
        }
        if memory_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(1)) {
            let samples = crate::samples();
            let get = |n: &str| samples.iter().find(|(k, _)| *k == n).map(|&(_, v)| v);
            let both = |a, b| get(a).zip(get(b));
            memory = [
                both("vram.local", "vram.local_budget"),
                both("vram.shared", "vram.shared_budget"),
            ];
            memory_at = Some(now);
        }
        let window = std::mem::take(&mut *lock(&WINDOW));
        let n = rows.len().max(1) as f64;
        let mean = |f: fn(&crate::FrameRow) -> f64| rows.iter().map(f).sum::<f64>() / n;
        let input = LineInput {
            frames: rows.len(),
            last_frame: last_number.unwrap_or(0),
            no_frame_s: seen_last.elapsed().as_secs_f64(),
            cpu_ms: mean(|r| (r.interval_ms - r.gpu_wait_ms).max(0.0)),
            gpu_ms: mean(|r| r.gpu_wait_ms),
            dropped: mean(|r| r.dropped as f64),
            zero_groups: mean(|r| r.zero_groups as f64),
            submission: window.submission,
            command: window.command,
            markers: progress.last_done.is_some()
                || progress.busy.as_ref().is_some_and(|b| b.list.is_some()),
            progress,
            vram: memory[0],
            shared: memory[1],
            suppressed: SUPPRESSED.swap(0, Relaxed),
        };
        WARNINGS.store(0, Relaxed);
        write(&line(&input));
        last_line = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_variables_ask_for_live_lines() {
        assert_eq!(LiveSettings::parse(None, None, None), None);
        assert_eq!(LiveSettings::parse(Some("0"), None, None), None);
        let s = LiveSettings::parse(Some("1"), None, None).unwrap();
        assert!(!s.every_frame && s.file.is_none());
        assert_eq!((s.slow_ms, s.stall_ms), (100.0, 1000.0));
        let s = LiveSettings::parse(Some("frame"), Some("250"), Some("-3")).unwrap();
        assert!(s.every_frame);
        assert_eq!((s.slow_ms, s.stall_ms), (250.0, 1000.0));
        let s = LiveSettings::parse(Some(r"D:\logs\live.txt"), None, Some("500")).unwrap();
        assert_eq!(s.file, Some(PathBuf::from(r"D:\logs\live.txt")));
        assert_eq!(s.stall_ms, 500.0);
    }

    fn input() -> LineInput {
        LineInput {
            frames: 3,
            last_frame: 1234,
            cpu_ms: 41.3,
            gpu_ms: 290.0,
            dropped: 12.0,
            zero_groups: 3.0,
            submission: Some(Slowest {
                ms: 812.4,
                lists: 11,
                pipelines: "[compute 8bbcbda6008eaa37]".into(),
            }),
            command: Some(("mesh 3 [mesh 51ff, pixel 7fe7]".into(), 387.8, 412.0)),
            progress: Progress {
                busy: None,
                last_done: Some("dispatch 4x1x1 [compute d3e4]".into()),
            },
            vram: Some((6_120_000_000, 7_500_000_000)),
            shared: Some((250_000_000, 16_000_000_000)),
            markers: true,
            ..Default::default()
        }
    }

    #[test]
    fn a_line_has_every_field() {
        assert_eq!(
            line(&input()),
            "frames 3 (last 1234) | cpu 41.3 ms gpu 290.0 ms /frame | slowest submission \
             812.4 ms (11 lists) [compute 8bbcbda6008eaa37] | slowest command 387.8 ms (list \
             412.0) mesh 3 [mesh 51ff, pixel 7fe7] | last marker done: dispatch 4x1x1 [compute \
             d3e4] | dropped 12.0 zero-group 3.0 /frame | vram 6.12 GB / 7.50 GB, shared \
             250.0 MB / 16.00 GB"
        );
    }

    #[test]
    fn a_line_without_frames_or_markers_says_so() {
        let mut i = LineInput {
            no_frame_s: 4.3,
            ..Default::default()
        };
        i.progress.busy = Some(Busy {
            submission: 77,
            for_ms: 1500.0,
            lists: 11,
            ..Default::default()
        });
        let l = line(&i);
        assert!(
            l.starts_with("no frame for 4.3 s | GPU BUSY 1500 ms in s77 | no submission"),
            "{l}"
        );
        assert!(l.contains("commands untimed | markers off"), "{l}");
        assert!(l.ends_with("vram n/a, shared n/a"), "{l}");
    }

    #[test]
    fn the_command_in_flight_comes_before_the_last_done() {
        let mut i = input();
        i.progress.busy = Some(Busy {
            submission: 9,
            for_ms: 2400.0,
            list: Some(2),
            lists: 11,
            done: 45,
            commands: 108,
            running: Some("mesh 1 [mesh ba8d]".into()),
            pipelines: String::new(),
        });
        let l = line(&i);
        assert!(
            l.contains("GPU BUSY 2400 ms in s9 list 3/11 at 45/108"),
            "{l}"
        );
        assert!(l.contains("GPU in: mesh 1 [mesh ba8d]"), "{l}");
        assert!(!l.contains("last marker done"), "{l}");
    }

    #[test]
    fn stalls_are_warned_at_the_threshold_then_each_doubling() {
        let mut w = StallWatch::default();
        let t0 = Instant::now();
        let busy = |done, for_ms| Busy {
            submission: 5,
            for_ms,
            list: Some(0),
            lists: 1,
            done,
            commands: 10,
            ..Default::default()
        };
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        // Busy for 0.2 s at its start: no warning.
        assert_eq!(w.check(Some(&busy(3, 200.0)), at(200), 1000.0), None);
        // On the same marker past 1 s since the submission began: warned.
        let s = w.check(Some(&busy(3, 1100.0)), at(1100), 1000.0).unwrap();
        assert!((s - 1100.0).abs() < 1.0, "{s}");
        assert_eq!(w.check(Some(&busy(3, 1500.0)), at(1500), 1000.0), None);
        assert!(w.check(Some(&busy(3, 2100.0)), at(2100), 1000.0).is_some());
        // The marker moves: a new count from there.
        assert_eq!(w.check(Some(&busy(4, 2200.0)), at(2200), 1000.0), None);
        assert_eq!(w.check(Some(&busy(4, 3000.0)), at(3000), 1000.0), None);
        assert!(w.check(Some(&busy(4, 3300.0)), at(3300), 1000.0).is_some());
        // Idle: forgotten.
        assert_eq!(w.check(None, at(3400), 1000.0), None);
    }

    #[test]
    fn a_stall_names_where_the_gpu_is() {
        let b = Busy {
            submission: 12,
            for_ms: 1800.0,
            list: Some(3),
            lists: 11,
            done: 45,
            commands: 108,
            running: Some("mesh 3 [mesh 51ff, pixel 7fe7]".into()),
            pipelines: "[compute a], [mesh 51ff, pixel 7fe7]".into(),
        };
        assert_eq!(
            stall_text(&b, 1200.0, Some("dispatch 1x1x1 [compute a]")),
            "STALL: submission s12 busy 1800 ms, no marker passed for 1200 ms; list 4/11, 45 \
             of 108 commands done; last done: dispatch 1x1x1 [compute a]; stuck in: mesh 3 \
             [mesh 51ff, pixel 7fe7]; pipelines [compute a], [mesh 51ff, pixel 7fe7]"
        );
    }
}
