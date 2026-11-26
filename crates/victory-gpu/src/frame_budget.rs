//! The frame budget (33.3 ms a frame at 30 a second).
//!
//! - A report of each frame past [`REPORT`], at most one line a second:
//!   what bound it and why ([`cause`]), and where its time went (the GPU,
//!   our work around submissions ([`Phases`]), pipeline waits, and the
//!   rest). With commands timed (`VICTORY_FRAME_TIMING=1`), also the GPU
//!   time per kind of command and per way programs reach memory
//!   ([`Traits`]), and the slowest commands.
//! - The slow-pipeline guard, off unless `VICTORY_SLOW_PIPELINES=1`: a
//!   pipeline with a command over the limit (`VICTORY_SLOW_PIPELINE_MS`,
//!   100 ms) in [`STRIKES`] frames in a row, past its first [`WARMUP`]
//!   frames, is dropped from draws and dispatches recorded from then on
//!   (`State::bind`) for [`COOLDOWN`], then tried again; each later ban
//!   doubles, up to [`COOLDOWN_MAX`]. It drops title work.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Frames longer than this are reported.
pub(crate) const REPORT: Duration = Duration::from_millis(50);
/// Over-limit frames in a row before a pipeline is banned.
pub(crate) const STRIKES: u32 = 3;
/// Frames after a pipeline is first timed that are no strikes for it.
pub(crate) const WARMUP: u64 = 10;
/// How long a first ban lasts before the pipeline is tried again.
pub(crate) const COOLDOWN: Duration = Duration::from_secs(60);
/// The longest ban.
pub(crate) const COOLDOWN_MAX: Duration = Duration::from_secs(16 * 60);
/// The limit for one command without `VICTORY_SLOW_PIPELINE_MS`.
const LIMIT_MS: f64 = 100.0;
/// Slowest commands kept per frame for its report.
const SLOWEST: usize = 3;

/// The guard's settings, read once: on, and its limit for one command in
/// ms.
fn settings() -> (bool, f64) {
    static SETTINGS: OnceLock<(bool, f64)> = OnceLock::new();
    *SETTINGS.get_or_init(|| {
        let on = std::env::var_os("VICTORY_SLOW_PIPELINES").is_some_and(|v| v == "1");
        let limit = std::env::var("VICTORY_SLOW_PIPELINE_MS")
            .ok()
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|&ms| ms > 0.0)
            .unwrap_or(LIMIT_MS);
        if on {
            tracing::info!(limit_ms = limit, strikes = STRIKES, "slow-pipeline guard");
        }
        (on, limit)
    })
}

/// Whether the guard is on: 0 not read yet, 1 off, 2 on.
static ON: AtomicU8 = AtomicU8::new(0);

/// Whether the guard is on (`VICTORY_SLOW_PIPELINES=1`).
fn guarding() -> bool {
    match ON.load(Ordering::Relaxed) {
        0 => {
            let on = settings().0;
            let _ = ON.compare_exchange(0, 1 + u8::from(on), Ordering::Relaxed, Ordering::Relaxed);
            ON.load(Ordering::Relaxed) == 2
        }
        v => v == 2,
    }
}

/// Whether commands are timed: with the guard, or `VICTORY_FRAME_TIMING=1`.
pub(crate) fn timing() -> bool {
    static ASKED: OnceLock<bool> = OnceLock::new();
    guarding()
        || *ASKED.get_or_init(|| std::env::var_os("VICTORY_FRAME_TIMING").is_some_and(|v| v == "1"))
}

/// The guard's limit for one command, in ms.
pub(crate) fn limit() -> f64 {
    settings().1
}

/// What a timed command was, in terms of any title: what kind of command,
/// how big, and how its programs reach memory.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Command {
    /// Its pipeline's shaders.
    pub(crate) pipeline: String,
    pub(crate) kind: Kind,
    /// The groups of a direct dispatch or mesh draw (an indirect one's are
    /// in GPU memory).
    pub(crate) groups: Option<[u32; 3]>,
    /// [`Traits`] bits of its programs.
    pub(crate) traits: u8,
}

/// Kinds of timed commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Dispatch = 0,
    Mesh = 1,
    IndirectDispatch = 2,
    IndirectMesh = 3,
}

const KINDS: [&str; 4] = ["compute", "mesh", "indirect compute", "indirect mesh"];

/// How a command's programs reach memory: bits of [`Command::traits`],
/// each a known cost of translated programs on the host.
pub(crate) struct Traits;

impl Traits {
    /// It reads or writes title memory at addresses it computes, through
    /// the page table; its writes mark pages in a bitmap with atomics.
    pub(crate) const MEMORY: u8 = 1;
    /// It marks the pages it writes through buffer views, with atomics.
    pub(crate) const VIEW_MARKS: u8 = 2;
    /// It picks views from the heap at indices it computes.
    pub(crate) const HEAP: u8 = 4;
    /// It reads views' title addresses from their descriptors.
    pub(crate) const VIEW_ADDRESSES: u8 = 8;
    const NAMES: [&str; 4] = [
        "title memory by address",
        "write marks through views",
        "views picked from the heap",
        "view addresses read",
    ];
}

/// `traits` named.
fn trait_names(traits: u8) -> Vec<&'static str> {
    (0..Traits::NAMES.len())
        .filter(|k| traits >> k & 1 != 0)
        .map(|k| Traits::NAMES[k])
        .collect()
}

impl Command {
    /// A direct dispatch of `pipeline` with no traits (tests).
    #[cfg(test)]
    pub(crate) fn compute(pipeline: &str) -> Self {
        Command {
            pipeline: pipeline.to_owned(),
            kind: Kind::Dispatch,
            groups: None,
            traits: 0,
        }
    }

    /// The command for a report: `212 ms compute 1024x1x1 groups [title
    /// memory by address] [compute 0123..]`.
    fn describe(&self, ms: f64) -> String {
        let mut out = format!("{ms:.0} ms {}", KINDS[self.kind as usize]);
        if let Some([x, y, z]) = self.groups {
            out += &format!(" {x}x{y}x{z} groups");
        }
        let traits = trait_names(self.traits);
        if !traits.is_empty() {
            out += &format!(" [{}]", traits.join(", "));
        }
        out + " " + &self.pipeline
    }
}

/// Where a submission's time went: ours before it (`sync`: descriptor
/// heaps and evictions; `uploads`: textures; `refresh`: host copies of
/// title memory brought up to date), the GPU's, and ours after it
/// (`copy_back`: what shaders wrote, back into title memory).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Phases {
    pub(crate) sync: Duration,
    pub(crate) uploads: Duration,
    pub(crate) refresh: Duration,
    pub(crate) gpu: Duration,
    pub(crate) copy_back: Duration,
}

/// What a frame's timed commands took on the GPU: per [`Kind`] (ms and
/// count), per [`Traits`] bit (ms), and the slowest commands.
#[derive(Clone, Debug, Default, PartialEq)]
struct Breakdown {
    kinds: [(f64, u32); 4],
    traits: [f64; 4],
    slowest: Vec<(f64, Command)>,
}

impl Breakdown {
    fn add(&mut self, command: &Command, ms: f64) {
        let kind = &mut self.kinds[command.kind as usize];
        kind.0 += ms;
        kind.1 += 1;
        for (k, t) in self.traits.iter_mut().enumerate() {
            if command.traits >> k & 1 != 0 {
                *t += ms;
            }
        }
        keep_slowest(&mut self.slowest, ms, command);
    }

    /// The timed commands' GPU time, summed (commands without a barrier
    /// between them may overlap, so it can pass the wall time).
    fn timed_ms(&self) -> f64 {
        self.kinds.iter().map(|k| k.0).sum()
    }
}

/// What submissions and waits added to the frame so far, and the report's
/// clock: atomics, so the report takes no lock.
struct Report {
    /// [`Phases`], in µs.
    sync_us: AtomicU64,
    uploads_us: AtomicU64,
    refresh_us: AtomicU64,
    gpu_us: AtomicU64,
    copy_back_us: AtomicU64,
    submissions: AtomicU32,
    /// Waits for pipelines being made, in µs.
    waited_us: AtomicU64,
    /// Draws and dispatches the guard dropped.
    skipped: AtomicU32,
    /// The end of the last frame, and of the last report: µs from
    /// [`epoch`], plus one (0: none yet).
    last: AtomicU64,
    reported: AtomicU64,
    /// Slow frames not reported since the last report.
    unreported: AtomicU32,
}

/// A slow frame to report.
#[derive(Debug, PartialEq)]
struct Slow {
    took: Duration,
    phases: Phases,
    submissions: u32,
    waited: Duration,
    skipped: u32,
    /// Slow frames not reported since the last report.
    others: u32,
}

/// The report's clock starts at the first frame.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn us(d: Duration) -> u64 {
    d.as_micros().try_into().unwrap_or(u64::MAX)
}

impl Report {
    const fn new() -> Self {
        Self {
            sync_us: AtomicU64::new(0),
            uploads_us: AtomicU64::new(0),
            refresh_us: AtomicU64::new(0),
            gpu_us: AtomicU64::new(0),
            copy_back_us: AtomicU64::new(0),
            submissions: AtomicU32::new(0),
            waited_us: AtomicU64::new(0),
            skipped: AtomicU32::new(0),
            last: AtomicU64::new(0),
            reported: AtomicU64::new(0),
            unreported: AtomicU32::new(0),
        }
    }

    fn submitted(&self, p: Phases) {
        for (a, d) in [
            (&self.sync_us, p.sync),
            (&self.uploads_us, p.uploads),
            (&self.refresh_us, p.refresh),
            (&self.gpu_us, p.gpu),
            (&self.copy_back_us, p.copy_back),
        ] {
            a.fetch_add(us(d), Ordering::Relaxed);
        }
        self.submissions.fetch_add(1, Ordering::Relaxed);
    }

    /// A frame ended at `now`; a new one begins. The frame if it was slow
    /// and none was reported in the last second (else it is counted). One
    /// thread presents; another racing it only blurs a report.
    fn presented(&self, now: Instant) -> Option<Slow> {
        let at = us(now.saturating_duration_since(epoch())) + 1;
        let last = self.last.swap(at, Ordering::Relaxed);
        let micros = |a: &AtomicU64| Duration::from_micros(a.swap(0, Ordering::Relaxed));
        let phases = Phases {
            sync: micros(&self.sync_us),
            uploads: micros(&self.uploads_us),
            refresh: micros(&self.refresh_us),
            gpu: micros(&self.gpu_us),
            copy_back: micros(&self.copy_back_us),
        };
        let waited = micros(&self.waited_us);
        let submissions = self.submissions.swap(0, Ordering::Relaxed);
        let skipped = self.skipped.swap(0, Ordering::Relaxed);
        if last == 0 {
            return None;
        }
        let took = Duration::from_micros(at.saturating_sub(last));
        if took <= REPORT {
            return None;
        }
        let reported = self.reported.load(Ordering::Relaxed);
        if reported != 0 && at.saturating_sub(reported) < 1_000_000 {
            self.unreported.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        self.reported.store(at, Ordering::Relaxed);
        let others = self.unreported.swap(0, Ordering::Relaxed);
        Some(Slow {
            took,
            phases,
            submissions,
            waited,
            skipped,
            others,
        })
    }

    #[cfg(test)]
    fn clear(&self) {
        for a in [
            &self.sync_us,
            &self.uploads_us,
            &self.refresh_us,
            &self.gpu_us,
            &self.copy_back_us,
            &self.waited_us,
            &self.last,
            &self.reported,
        ] {
            a.store(0, Ordering::Relaxed);
        }
        for a in [&self.submissions, &self.skipped, &self.unreported] {
            a.store(0, Ordering::Relaxed);
        }
    }
}

static FRAME: Report = Report::new();

/// The slow-pipeline guard's state; only used while it is on.
#[derive(Default)]
struct Guard {
    /// Frames presented while the guard is on: the warm-up's clock.
    frames: u64,
    /// The frame each pipeline was first timed in.
    seen: HashMap<String, u64>,
    /// Over-limit frames in a row per pipeline (none: no strikes).
    strikes: HashMap<String, u32>,
    /// Pipelines whose draws and dispatches are dropped when recorded.
    banned: HashMap<String, Ban>,
    /// Pipelines tried again after a ban, with that ban's length: banned
    /// again for twice as long if still slow, kept (and forgotten) after a
    /// frame within the limit.
    retried: HashMap<String, Duration>,
    /// The pipelines timed this frame (not banned ones), each with its
    /// slowest command's ms.
    timed: HashMap<String, f64>,
    /// The frame's timed commands, for its report.
    gpu: Breakdown,
}

/// A ban: until when, and how long it is.
#[derive(Debug, PartialEq)]
struct Ban {
    until: Instant,
    cooldown: Duration,
}

impl Guard {
    /// A timed command of `pipeline` took `ms` on the GPU. Strikes are
    /// counted when the frame ends.
    fn ran(&mut self, pipeline: &str, ms: f64) {
        if self.banned.contains_key(pipeline) {
            return;
        }
        match self.timed.get_mut(pipeline) {
            Some(slowest) => *slowest = slowest.max(ms),
            None => {
                self.timed.insert(pipeline.to_owned(), ms);
            }
        }
    }

    /// A frame ended at `now`, with commands over `limit` ms or not: one
    /// strike for each pipeline with a command over it (not in its
    /// warm-up), none left for those whose timed commands all kept to it.
    /// A pipeline at [`STRIKES`] is banned; bans that ended are retried.
    /// What the frame's timed commands took.
    fn presented(&mut self, now: Instant, limit: f64, strike: bool) -> Breakdown {
        let frame = self.frames;
        self.frames += 1;
        let mut timed = std::mem::take(&mut self.timed);
        if !strike {
            timed.clear();
        }
        for (pipeline, ms) in timed.drain() {
            let first = match self.seen.get(&pipeline) {
                Some(&first) => first,
                None => {
                    self.seen.insert(pipeline.clone(), frame);
                    frame
                }
            };
            if frame - first < WARMUP {
                continue;
            }
            if ms <= limit {
                self.strikes.remove(&pipeline);
                self.retried.remove(&pipeline);
                continue;
            }
            let strikes = self.strikes.entry(pipeline.clone()).or_default();
            *strikes += 1;
            if *strikes < STRIKES {
                continue;
            }
            self.strikes.remove(&pipeline);
            let cooldown = match self.retried.remove(&pipeline) {
                Some(last) => {
                    let cooldown = (2 * last).min(COOLDOWN_MAX);
                    tracing::warn!(
                        pipeline,
                        ms = format_args!("{ms:.0}"),
                        limit_ms = limit,
                        retry_s = cooldown.as_secs(),
                        "a pipeline tried again is still too slow on this host: dropped again, \
                         for longer"
                    );
                    cooldown
                }
                None => {
                    tracing::warn!(
                        pipeline,
                        ms = format_args!("{ms:.0}"),
                        limit_ms = limit,
                        frames = STRIKES,
                        retry_s = COOLDOWN.as_secs(),
                        "a pipeline too slow on this host is dropped from draws and dispatches \
                         recorded from now on, until it is tried again; lists recorded before \
                         still run it: what it computes is missing"
                    );
                    COOLDOWN
                }
            };
            let until = now + cooldown;
            self.banned.insert(pipeline, Ban { until, cooldown });
        }
        // The next frame keeps the map's room.
        self.timed = timed;
        let retried = &mut self.retried;
        self.banned.retain(|pipeline, ban| {
            if ban.until > now {
                return true;
            }
            tracing::info!(
                pipeline,
                banned_s = ban.cooldown.as_secs(),
                "a pipeline dropped as too slow is tried again"
            );
            retried.insert(pipeline.clone(), ban.cooldown);
            false
        });
        std::mem::take(&mut self.gpu)
    }
}

static GUARD: Mutex<Option<Guard>> = Mutex::new(None);
/// Whether any pipeline is banned (the check on every bind is then free).
static ANY_BANNED: AtomicBool = AtomicBool::new(false);
/// The banned pipelines, for [`skips`]: recording threads read them under
/// a shared lock, never the guard's.
static BANNED: std::sync::RwLock<Vec<String>> = std::sync::RwLock::new(Vec::new());

#[cfg(test)]
thread_local! {
    /// Times this thread took the guard's lock.
    static LOCKED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn guard<R>(f: impl FnOnce(&mut Guard) -> R) -> R {
    #[cfg(test)]
    LOCKED.with(|n| n.set(n.get() + 1));
    let mut held = GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let g = held.get_or_insert_with(Guard::default);
    let r = f(g);
    let changed = {
        let banned = BANNED.read().unwrap_or_else(|e| e.into_inner());
        banned.len() != g.banned.len() || banned.iter().any(|p| !g.banned.contains_key(p))
    };
    if changed {
        *BANNED.write().unwrap_or_else(|e| e.into_inner()) = g.banned.keys().cloned().collect();
    }
    ANY_BANNED.store(!g.banned.is_empty(), Ordering::Relaxed);
    r
}

/// Whether the guard dropped `pipeline` (and counts the draw or dispatch
/// not run). Asked while recording: commands already in a closed list
/// run whenever it is submitted.
pub(crate) fn skips(pipeline: &str) -> bool {
    if !ANY_BANNED.load(Ordering::Relaxed) {
        return false;
    }
    let banned = BANNED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|p| p == pipeline);
    if banned {
        FRAME.skipped.fetch_add(1, Ordering::Relaxed);
    }
    banned
}

/// A list's timed commands ran: each one and its GPU time in ms. Strikes
/// are counted when the frame ends ([`presented`]).
pub(crate) fn ran(timed: Vec<(Command, f64)>) {
    if timed.is_empty() || !timing() {
        return;
    }
    guard(|g| {
        for (command, ms) in timed {
            g.gpu.add(&command, ms);
            g.ran(&command.pipeline, ms);
        }
    });
}

/// Keeps the [`SLOWEST`] slowest commands of a frame, slowest first.
fn keep_slowest<T: Clone>(slowest: &mut Vec<(f64, T)>, ms: f64, command: &T) {
    if slowest.len() < SLOWEST || slowest.last().is_some_and(|l| l.0 < ms) {
        slowest.push((ms, command.clone()));
        slowest.sort_by(|a, b| b.0.total_cmp(&a.0));
        slowest.truncate(SLOWEST);
    }
}

/// A submission and where its time went.
pub(crate) fn submitted(phases: Phases) {
    FRAME.submitted(phases);
}

/// A wait for a pipeline being made.
pub(crate) fn waited(time: Duration) {
    FRAME.waited_us.fetch_add(us(time), Ordering::Relaxed);
}

/// The parts of a slow frame, largest first. "outside submissions" is the
/// frame less all we measured: the title's own work on its threads, and
/// the translation of its command-list calls as it makes them.
fn parts(slow: &Slow) -> [(&'static str, Duration); 7] {
    let p = slow.phases;
    let measured = p.sync + p.uploads + p.refresh + p.gpu + p.copy_back + slow.waited;
    let mut parts = [
        ("GPU", p.gpu),
        ("outside submissions", slow.took.saturating_sub(measured)),
        ("title memory refresh", p.refresh),
        ("copy-back", p.copy_back),
        ("texture uploads", p.uploads),
        ("descriptor heap sync", p.sync),
        ("pipeline waits", slow.waited),
    ];
    parts.sort_by_key(|p| std::cmp::Reverse(p.1));
    parts
}

/// What a slow frame was bound by, and why, in terms of any title: its
/// largest part and what that part is made of (for the GPU: the kind of
/// commands, and how their programs reach memory). `timed`: whether the
/// frame's commands were timed.
fn cause(slow: &Slow, gpu: &Breakdown, timed: bool) -> (&'static str, String) {
    let bound = parts(slow)[0].0;
    let why = match bound {
        "GPU" => gpu_cause(gpu, slow.phases.gpu, timed),
        "outside submissions" => "the title's own CPU work, and the translation of its \
                                  command-list calls as it makes them (time not in a \
                                  submission or a pipeline wait)"
            .to_owned(),
        "title memory refresh" => "bringing host copies of title memory up to date before \
                                   submissions (what the title wrote since)"
            .to_owned(),
        "copy-back" => "copying what shaders wrote back into title memory after submissions"
            .to_owned(),
        "texture uploads" => "uploading textures whose title memory changed".to_owned(),
        "descriptor heap sync" => {
            "bringing host descriptor heaps up to date with the title's, and evictions".to_owned()
        }
        _ => "waiting for pipelines being compiled".to_owned(),
    };
    (bound, why)
}

/// What the GPU's part of a frame was spent on.
fn gpu_cause(b: &Breakdown, gpu: Duration, timed: bool) -> String {
    if !timed {
        return "GPU work (commands are timed with VICTORY_FRAME_TIMING=1)".to_owned();
    }
    let total = b.timed_ms();
    let wall = gpu.as_secs_f64() * 1e3;
    if total <= 0.0 || wall - total > total {
        return format!(
            "GPU work that is not timed (draws without mesh programs, copies, clears): \
             {:.0} of {wall:.0} ms",
            (wall - total).max(0.0)
        );
    }
    let pct = |ms: f64| 100.0 * ms / total;
    let (k, &(ms, n)) = b
        .kinds
        .iter()
        .enumerate()
        .max_by(|a, c| a.1.0.total_cmp(&c.1.0))
        .expect("four kinds");
    let mut why = format!(
        "{:.0}% of the timed GPU time is {} work ({n} commands)",
        pct(ms),
        KINDS[k]
    );
    for (t, &ms) in b.traits.iter().enumerate() {
        if pct(ms) >= 50.0 {
            why += &format!("; {:.0}% is in programs using {}", pct(ms), Traits::NAMES[t]);
        }
    }
    why
}

/// A frame was presented: reports it if it was slow; a new one begins.
pub(crate) fn presented() {
    let now = Instant::now();
    let timed = timing();
    let gpu = if timed {
        guard(|g| g.presented(now, limit(), guarding()))
    } else {
        Breakdown::default()
    };
    let Some(slow) = FRAME.presented(now) else {
        return;
    };
    let (bound, why) = cause(&slow, &gpu, timed);
    let ms = |d: Duration| format_args!("{:.0}", d.as_secs_f64() * 1e3).to_string();
    let p = slow.phases;
    let parts = parts(&slow);
    let part = |name: &str| {
        let time = parts.iter().find(|(n, _)| *n == name);
        ms(time.map_or(Duration::ZERO, |p| p.1))
    };
    let gpu_by_kind: Vec<String> = (0..KINDS.len())
        .filter(|&k| gpu.kinds[k].1 > 0)
        .map(|k| format!("{} {:.0} ms in {}", KINDS[k], gpu.kinds[k].0, gpu.kinds[k].1))
        .collect();
    let gpu_by_trait: Vec<String> = (0..Traits::NAMES.len())
        .filter(|&t| gpu.traits[t] > 0.0)
        .map(|t| format!("{} {:.0} ms", Traits::NAMES[t], gpu.traits[t]))
        .collect();
    let slowest: Vec<String> = gpu.slowest.iter().map(|(t, c)| c.describe(*t)).collect();
    tracing::warn!(
        frame_ms = ms(slow.took),
        bound,
        why,
        gpu_ms = ms(p.gpu),
        outside_ms = part("outside submissions"),
        ours_ms = ms(p.sync + p.uploads + p.refresh + p.copy_back),
        refresh_ms = ms(p.refresh),
        copy_back_ms = ms(p.copy_back),
        uploads_ms = ms(p.uploads),
        heap_sync_ms = ms(p.sync),
        pipeline_waits_ms = ms(slow.waited),
        submissions = slow.submissions,
        ?gpu_by_kind,
        ?gpu_by_trait,
        ?slowest,
        skipped = slow.skipped,
        other_slow_frames = slow.others,
        "slow frame"
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D12::{ID3D12PipelineState, ID3D12RootSignature};
    use windows::core::Interface;

    /// The tests share the guard: one at a time (`crate::draw`'s too).
    pub(crate) static ONE: Mutex<()> = Mutex::new(());

    /// Clears the guard and the frame (tests share the process).
    pub(crate) fn reset() {
        *GUARD.lock().unwrap_or_else(|e| e.into_inner()) = None;
        BANNED.write().unwrap_or_else(|e| e.into_inner()).clear();
        ANY_BANNED.store(false, Ordering::Relaxed);
        FRAME.clear();
    }

    /// The guard on or off until dropped (then as before); hold [`ONE`].
    pub(crate) struct Switched(u8);

    pub(crate) fn switch(on: bool) -> Switched {
        timing();
        Switched(ON.swap(1 + u8::from(on), Ordering::Relaxed))
    }

    impl Drop for Switched {
        fn drop(&mut self) {
            ON.store(self.0, Ordering::Relaxed);
        }
    }

    /// Bans `pipeline` through the guard's calls: its warm-up, then
    /// [`STRIKES`] frames over the limit.
    pub(crate) fn ban(pipeline: &str) {
        for _ in 0..WARMUP + u64::from(STRIKES) {
            ran(vec![(Command::compute(pipeline), limit() + 1.0)]);
            presented();
        }
    }

    const LIMIT: f64 = 100.0;
    const SLOW: f64 = LIMIT + 1.0;
    const QUICK: f64 = LIMIT / 2.0;

    /// A frame of `g` ending at `now` in which `pipeline` ran commands of
    /// `ms` each.
    fn frame_at(g: &mut Guard, now: Instant, pipeline: &str, ms: &[f64]) {
        for &ms in ms {
            g.ran(pipeline, ms);
        }
        g.presented(now, LIMIT, true);
    }

    /// A guard past `pipeline`'s warm-up, its frames quick.
    fn warm(pipeline: &str, now: Instant) -> Guard {
        let mut g = Guard::default();
        for _ in 0..WARMUP {
            frame_at(&mut g, now, pipeline, &[QUICK]);
        }
        g
    }

    /// Without the guard, timed commands strike nothing: a pipeline over
    /// the limit in every frame is never banned.
    #[test]
    fn without_the_guard_nothing_is_banned() {
        let mut g = Guard::default();
        for _ in 0..WARMUP + 10 * u64::from(STRIKES) {
            g.ran("[compute slow]", SLOW);
            g.presented(Instant::now(), LIMIT, false);
        }
        assert!(g.banned.is_empty() && g.strikes.is_empty());
    }

    /// The first [`WARMUP`] frames after a pipeline is first timed are no
    /// strikes, however slow; the frames after are.
    #[test]
    fn no_strikes_during_the_warm_up() {
        let now = Instant::now();
        let mut g = Guard::default();
        frame_at(&mut g, now, "[compute other]", &[QUICK]);
        for _ in 0..WARMUP {
            frame_at(&mut g, now, "[compute first]", &[SLOW, SLOW]);
        }
        assert!(g.strikes.is_empty() && g.banned.is_empty(), "warming up");
        for _ in 0..STRIKES - 1 {
            frame_at(&mut g, now, "[compute first]", &[SLOW]);
        }
        assert!(!g.banned.contains_key("[compute first]"));
        frame_at(&mut g, now, "[compute first]", &[SLOW]);
        assert!(g.banned.contains_key("[compute first]"), "after the warm-up");
    }

    /// Commands over the limit in one frame are one strike, however many:
    /// a pipeline is not banned in the frame its commands first ran slow.
    #[test]
    fn slow_commands_in_one_frame_are_one_strike() {
        let now = Instant::now();
        let mut g = warm("[compute slow]", now);
        frame_at(&mut g, now, "[compute slow]", &[SLOW; STRIKES as usize]);
        assert!(!g.banned.contains_key("[compute slow]"), "one frame, one strike");
        assert_eq!(g.strikes["[compute slow]"], 1);
    }

    /// A frame in which all of a pipeline's commands kept to the limit
    /// clears its strikes (a one-off hitch does not add up), and frames in
    /// which it did not run leave them.
    #[test]
    fn a_frame_within_the_limit_clears_the_strikes() {
        let now = Instant::now();
        let mut g = warm("[compute hitch]", now);
        frame_at(&mut g, now, "[compute hitch]", &[SLOW]);
        frame_at(&mut g, now, "[compute hitch]", &[QUICK, QUICK]);
        for _ in 0..STRIKES - 1 {
            frame_at(&mut g, now, "[compute hitch]", &[SLOW]);
            frame_at(&mut g, now, "[compute other]", &[QUICK]);
        }
        assert!(!g.banned.contains_key("[compute hitch]"), "slow, quick, slow, slow");
    }

    /// [`STRIKES`] over-limit frames in a row ban a pipeline at the end of
    /// the last, not before, and only it, for [`COOLDOWN`]; a frame counts
    /// as over the limit if any of its commands was.
    #[test]
    fn slow_frames_in_a_row_ban_a_pipeline() {
        let now = Instant::now();
        let mut g = warm("[compute slow]", now);
        for _ in 0..STRIKES - 1 {
            frame_at(&mut g, now, "[compute slow]", &[QUICK, SLOW, QUICK]);
            frame_at(&mut g, now, "[compute quick]", &[QUICK]);
        }
        g.ran("[compute slow]", SLOW);
        assert!(!g.banned.contains_key("[compute slow]"), "not before the frame ends");
        g.presented(now, LIMIT, true);
        let until = now + COOLDOWN;
        let ban = Ban {
            until,
            cooldown: COOLDOWN,
        };
        assert_eq!(g.banned.get("[compute slow]"), Some(&ban));
        assert!(!g.banned.contains_key("[compute quick]"));
    }

    /// A banned pipeline is tried again after its cooldown; still slow
    /// [`STRIKES`] frames in a row, it is banned again for twice as long,
    /// up to [`COOLDOWN_MAX`].
    #[test]
    fn a_ban_ends_and_a_pipeline_still_slow_is_banned_longer() {
        let p = "[compute slow]";
        let mut now = Instant::now();
        let mut g = warm(p, now);
        let mut cooldown = COOLDOWN;
        for _ in 0..8 {
            for _ in 0..STRIKES {
                frame_at(&mut g, now, p, &[SLOW]);
            }
            assert_eq!(g.banned[p].cooldown, cooldown);
            // Lists recorded before the ban still run it: no strikes.
            frame_at(&mut g, now + cooldown / 2, p, &[SLOW]);
            frame_at(&mut g, now + cooldown - Duration::from_millis(1), p, &[SLOW]);
            assert!(g.banned.contains_key(p), "within the cooldown");
            assert!(g.strikes.is_empty());
            now += cooldown;
            g.presented(now, LIMIT, true);
            assert!(!g.banned.contains_key(p), "tried again after {cooldown:?}");
            assert_eq!(g.retried[p], cooldown);
            cooldown = (2 * cooldown).min(COOLDOWN_MAX);
        }
        assert_eq!(cooldown, COOLDOWN_MAX);
    }

    /// A pipeline tried again that keeps to the limit stays allowed, and a
    /// later ban of it starts again at [`COOLDOWN`].
    #[test]
    fn a_pipeline_quick_when_tried_again_stays_allowed() {
        let p = "[compute better]";
        let mut now = Instant::now();
        let mut g = warm(p, now);
        for _ in 0..STRIKES {
            frame_at(&mut g, now, p, &[SLOW]);
        }
        now += COOLDOWN;
        g.presented(now, LIMIT, true);
        assert!(!g.banned.contains_key(p));
        frame_at(&mut g, now, p, &[SLOW]);
        frame_at(&mut g, now, p, &[QUICK]);
        assert!(g.retried.is_empty() && g.strikes.is_empty(), "kept");
        for _ in 0..100 {
            now += Duration::from_secs(1);
            frame_at(&mut g, now, p, &[QUICK]);
        }
        assert!(!g.banned.contains_key(p));
        for _ in 0..STRIKES {
            frame_at(&mut g, now, p, &[SLOW]);
        }
        assert_eq!(g.banned[p].cooldown, COOLDOWN, "a new ban");
    }

    /// The guard through its calls: a ban is seen by `skips`, which takes
    /// no lock of the guard's (recording threads ask it at every bind).
    #[test]
    fn a_banned_pipeline_is_skipped() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let _on = switch(true);
        reset();
        ban("[compute global]");
        let locked = LOCKED.with(|n| n.get());
        assert!(skips("[compute global]"));
        assert!(!skips("[compute other]"));
        assert_eq!(LOCKED.with(|n| n.get()), locked, "binds took the guard's lock");
        reset();
    }

    /// With the guard off nothing is timed and its lock is never taken:
    /// not on submissions, waits, presents or binds, and a list asked to
    /// time a command makes no query heap.
    #[test]
    fn the_guard_off_times_nothing_and_takes_no_lock() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let _off = switch(false);
        reset();
        let locked = LOCKED.with(|n| n.get());
        for _ in 0..WARMUP + u64::from(STRIKES) {
            submitted(Phases {
                gpu: Duration::from_millis(1),
                ..Phases::default()
            });
            waited(Duration::from_millis(1));
            ran(vec![(Command::compute("[compute off]"), 1e6)]);
            assert!(!skips("[compute off]"));
            presented();
        }
        assert_eq!(LOCKED.with(|n| n.get()), locked, "the guard's lock taken");
        assert!(GUARD.lock().unwrap().is_none(), "the guard's state made");
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let list = host.new_list().unwrap();
        assert_eq!(list.time_start(), None);
        list.close();
        host.execute(&[&list]);
        assert!(list.timestamp_heap().is_none(), "a query heap made");
        assert_eq!(LOCKED.with(|n| n.get()), locked, "the guard's lock taken");
    }

    /// With the guard on, a list makes its query heap at its first timed
    /// command, not before (a list with none never does), and keeps it
    /// across resets.
    #[test]
    fn lists_make_their_query_heap_once_when_they_time_a_command() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let _on = switch(true);
        let list = host.new_list().unwrap();
        list.close();
        host.execute(&[&list]);
        assert!(list.timestamp_heap().is_none(), "no timed command, no heap");
        let mut heaps = Vec::new();
        for _ in 0..3 {
            list.reset();
            let k = list.time_start().expect("timed");
            list.time_end(k, Command::compute("[compute heap]"));
            list.close();
            host.execute(&[&list]);
            heaps.push(list.timestamp_heap().expect("a heap").as_raw());
        }
        assert!(heaps.iter().all(|&h| h == heaps[0]), "the same heap after resets");
        reset();
    }

    /// A frame keeps its slowest commands, slowest first.
    #[test]
    fn a_frame_keeps_its_slowest_commands() {
        let mut slowest = Vec::new();
        for (ms, p) in [(5.0, "a"), (1.0, "b"), (9.0, "c"), (3.0, "d"), (0.5, "e")] {
            keep_slowest(&mut slowest, ms, &p);
        }
        let order: Vec<&str> = slowest.iter().map(|(_, p)| *p).collect();
        assert_eq!(order, ["c", "a", "d"]);
    }

    /// A compute pipeline that loops `n` (a root constant) times a thread
    /// and writes a UAV (root parameter 0), 64 threads a group.
    fn looping(host: &crate::host::Host) -> (ID3D12RootSignature, ID3D12PipelineState) {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::core::s;
        const SOURCE: &str = r#"
#define RS "UAV(u0), RootConstants(num32BitConstants=1, b0)"
RWByteAddressBuffer o : register(u0);
cbuffer c : register(b0) { uint n; }
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = id.x;
    for (uint i = 0; i < n; i++) {
        x = x * 1664525 + 1013904223;
    }
    o.Store(id.x * 4, x);
}
"#;
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: the source and names outlive the call.
        unsafe {
            D3DCompile(
                SOURCE.as_ptr().cast(),
                SOURCE.len(),
                s!("budget"),
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )
            .unwrap();
        }
        let code = code.unwrap();
        // SAFETY: the blob's own bytes, a shader with its root signature.
        unsafe {
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, bytes).unwrap();
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            };
            let pso: ID3D12PipelineState = host.device.CreateComputePipelineState(&desc).unwrap();
            (root, pso)
        }
    }

    /// Timed commands measure the GPU's work: after the list ran, a
    /// dispatch of a long loop took far longer than one of a short loop,
    /// each under its pipeline.
    #[test]
    fn timed_dispatches_measure_their_gpu_time() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let _on = switch(true);
        let (root, pso) = looping(host);
        let groups = 1024u32;
        let (out, _) = host.shared_buffer(u64::from(groups) * 64 * 4).unwrap();
        let list = host.new_list().unwrap();
        for (loops, pipeline) in [(1u32, "[compute short]"), (1 << 16, "[compute long]")] {
            let k = list.time_start().expect("timed");
            // SAFETY: an open list, live objects of ours.
            unsafe {
                list.list.SetComputeRootSignature(&root);
                list.list.SetPipelineState(&pso);
                list.list
                    .SetComputeRootUnorderedAccessView(0, out.GetGPUVirtualAddress());
                list.list.SetComputeRoot32BitConstant(1, loops, 0);
                list.list.Dispatch(groups, 1, 1);
            }
            list.time_end(k, Command::compute(pipeline));
        }
        list.close();
        host.execute(&[&list]);
        let times: HashMap<String, f64> = list
            .timed_ms(host.timestamp_frequency())
            .into_iter()
            .map(|(c, ms)| (c.pipeline, ms))
            .collect();
        let (short, long) = (times["[compute short]"], times["[compute long]"]);
        assert!(
            long > 10.0 * short.max(0.001),
            "short {short} ms, long {long} ms"
        );
        reset();
    }

    /// A frame's report starts over at each present; the first present
    /// only starts the clock; a slow frame is reported, and the slow ones
    /// within a second of it only counted.
    #[test]
    fn frames_start_over_at_each_present() {
        let r = Report::new();
        let t = epoch() + Duration::from_secs(1);
        let ms = Duration::from_millis;
        assert_eq!(r.presented(t), None);
        let phases = Phases {
            sync: ms(1),
            uploads: ms(2),
            refresh: ms(5),
            gpu: ms(7),
            copy_back: ms(4),
        };
        r.submitted(phases);
        r.waited_us.fetch_add(3000, Ordering::Relaxed);
        let slow = Slow {
            took: ms(60),
            phases,
            submissions: 1,
            waited: ms(3),
            skipped: 0,
            others: 0,
        };
        assert_eq!(r.presented(t + ms(60)), Some(slow));
        assert_eq!(r.presented(t + ms(80)), None, "quick");
        assert_eq!(r.presented(t + ms(200)), None, "slow, within the second");
        let slow = r.presented(t + ms(1100)).expect("slow");
        assert_eq!((slow.submissions, slow.others), (0, 1));
    }

    /// A slow frame of `took` ms whose parts are `phases` and `waited`.
    fn slow(took: u64, phases: Phases, waited: u64) -> Slow {
        Slow {
            took: Duration::from_millis(took),
            phases,
            submissions: 1,
            waited: Duration::from_millis(waited),
            skipped: 0,
            others: 0,
        }
    }

    /// A frame bound by the GPU names the kind of command that took most
    /// of its timed time, and the ways of reaching memory most of it used;
    /// a timed command is described by its kind, size and traits.
    #[test]
    fn a_gpu_bound_frame_says_what_its_commands_were() {
        let ms = Duration::from_millis;
        let frame = slow(
            1000,
            Phases {
                gpu: ms(800),
                refresh: ms(50),
                ..Phases::default()
            },
            0,
        );
        let mut b = Breakdown::default();
        let heavy = Command {
            pipeline: "[compute a]".into(),
            kind: Kind::Dispatch,
            groups: Some([1024, 1, 1]),
            traits: Traits::MEMORY | Traits::HEAP,
        };
        b.add(&heavy, 600.0);
        let mesh = Command {
            pipeline: "[mesh b]".into(),
            kind: Kind::Mesh,
            groups: Some([90, 1, 1]),
            traits: 0,
        };
        b.add(&mesh, 150.0);
        let (bound, why) = cause(&frame, &b, true);
        assert_eq!(bound, "GPU");
        assert!(why.starts_with("80% of the timed GPU time is compute work (1 commands)"), "{why}");
        assert!(why.contains("80% is in programs using title memory by address"), "{why}");
        assert!(why.contains("views picked from the heap"), "{why}");
        assert_eq!(
            b.slowest[0].1.describe(600.0),
            "600 ms compute 1024x1x1 groups [title memory by address, views picked from the \
             heap] [compute a]"
        );
        // Mostly untimed GPU time is said to be.
        let (_, why) = cause(&frame, &Breakdown::default(), true);
        assert!(why.starts_with("GPU work that is not timed"), "{why}");
        let (_, why) = cause(&frame, &b, false);
        assert!(why.contains("VICTORY_FRAME_TIMING=1"), "{why}");
    }

    /// A frame whose time is mostly not in submissions or waits is bound
    /// outside them; one mostly refreshing copies of title memory says so.
    #[test]
    fn a_frame_names_the_part_that_bound_it() {
        let ms = Duration::from_millis;
        let none = Breakdown::default();
        let outside = slow(
            900,
            Phases {
                gpu: ms(200),
                refresh: ms(100),
                ..Phases::default()
            },
            50,
        );
        let (bound, why) = cause(&outside, &none, true);
        assert_eq!(bound, "outside submissions");
        assert!(why.contains("command-list calls"), "{why}");
        assert_eq!(parts(&outside)[0].1, ms(550));
        let refresh = slow(
            400,
            Phases {
                gpu: ms(100),
                refresh: ms(250),
                copy_back: ms(40),
                ..Phases::default()
            },
            0,
        );
        assert_eq!(cause(&refresh, &none, true).0, "title memory refresh");
        let waits = slow(
            400,
            Phases {
                gpu: ms(10),
                ..Phases::default()
            },
            380,
        );
        assert_eq!(cause(&waits, &none, true).0, "pipeline waits");
    }
}
