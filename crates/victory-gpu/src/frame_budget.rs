//! The frame budget (33.3 ms a frame at 30 a second).
//!
//! A report of each frame past [`REPORT`], at most one line a second: what
//! bound it and why ([`cause`]), and where its time went (the GPU, our work
//! around submissions ([`Phases`]), pipeline waits, and the rest). With
//! commands timed (`VICTORY_GPU_TIMING` and `VICTORY_D3D12_DRED`, diagnostics
//! builds), also the GPU time per kind of command and per way programs reach
//! memory ([`Traits`]), and the slowest commands.
// Timed commands are fed only in diagnostics builds.
#![cfg_attr(not(feature = "diagnostics"), allow(dead_code))]

use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Frames longer than this are reported.
pub(crate) const REPORT: Duration = Duration::from_millis(50);
/// Slowest commands kept per frame for its report.
const SLOWEST: usize = 3;

/// Whether commands are timed: 0 not read yet, 1 off, 2 on.
static ON: AtomicU8 = AtomicU8::new(0);

/// Whether commands are timed: noted and timed by the host lists
/// (`VICTORY_GPU_TIMING` and `VICTORY_D3D12_DRED`, diagnostics builds).
pub(crate) fn timing() -> bool {
    match ON.load(Ordering::Relaxed) {
        0 => {
            let on = crate::host::timing() && crate::host::dred_on();
            let _ = ON.compare_exchange(0, 1 + u8::from(on), Ordering::Relaxed, Ordering::Relaxed);
            ON.load(Ordering::Relaxed) == 2
        }
        v => v == 2,
    }
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
        for a in [&self.submissions, &self.unreported] {
            a.store(0, Ordering::Relaxed);
        }
    }
}

static FRAME: Report = Report::new();

/// The frame's timed commands, while commands are timed.
static GPU: Mutex<Option<Breakdown>> = Mutex::new(None);

/// A list's timed commands ran: each one and its GPU time in ms.
pub(crate) fn ran(timed: Vec<(Command, f64)>) {
    if timed.is_empty() || !timing() {
        return;
    }
    let mut gpu = GPU.lock().unwrap_or_else(|e| e.into_inner());
    let gpu = gpu.get_or_insert_with(Breakdown::default);
    for (command, ms) in timed {
        gpu.add(&command, ms);
    }
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
        return "GPU work (commands are timed with VICTORY_GPU_TIMING=1 and VICTORY_D3D12_DRED=1 \
                in diagnostics builds)"
            .to_owned();
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
        GPU.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default()
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
        other_slow_frames = slow.others,
        "slow frame"
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D12::{ID3D12PipelineState, ID3D12RootSignature};

    /// The tests share the timing switch: one at a time.
    pub(crate) static ONE: Mutex<()> = Mutex::new(());

    /// Clears the breakdown and the frame (tests share the process).
    pub(crate) fn reset() {
        *GPU.lock().unwrap_or_else(|e| e.into_inner()) = None;
        FRAME.clear();
    }

    /// Timing on or off until dropped (then as before); hold [`ONE`].
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

    /// With timing off no breakdown is kept.
    #[test]
    fn timing_off_times_nothing() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let _off = switch(false);
        reset();
        for _ in 0..3 {
            submitted(Phases {
                gpu: Duration::from_millis(1),
                ..Phases::default()
            });
            waited(Duration::from_millis(1));
            ran(vec![(Command::compute("[compute off]"), 1e6)]);
            presented();
        }
        assert!(GPU.lock().unwrap().is_none(), "a breakdown kept");
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

    /// Commands noted for the report are timed by the host lists'
    /// timestamps: after the list ran, a dispatch of a long loop took far
    /// longer than one of a short loop, each under its pipeline.
    #[cfg(feature = "diagnostics")]
    #[test]
    fn timed_dispatches_measure_their_gpu_time() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let _off = switch(false);
        /// Noting and timing on for this thread until dropped.
        struct Here;
        impl Drop for Here {
            fn drop(&mut self) {
                crate::host::TIMED_HERE.with(|t| t.set(false));
            }
        }
        crate::host::TIMED_HERE.with(|t| t.set(true));
        let _here = Here;
        let (root, pso) = looping(host);
        let groups = 1024u32;
        let (out, _) = host.shared_buffer(u64::from(groups) * 64 * 4).unwrap();
        let list = host.new_list().unwrap();
        for (loops, pipeline) in [(1u32, "[compute short]"), (1 << 16, "[compute long]")] {
            // SAFETY: an open list, live objects of ours.
            unsafe {
                list.list.SetComputeRootSignature(&root);
                list.list.SetPipelineState(&pso);
                list.list
                    .SetComputeRootUnorderedAccessView(0, out.GetGPUVirtualAddress());
                list.list.SetComputeRoot32BitConstant(1, loops, 0);
            }
            list.note_timed(|| format!("dispatch {pipeline}"), || Some(Command::compute(pipeline)));
            // SAFETY: as above.
            unsafe { list.list.Dispatch(groups, 1, 1) };
        }
        list.close();
        host.execute(&[&list]);
        // In timestamp ticks: only their ratio is compared.
        let times: std::collections::HashMap<String, f64> =
            list.timed(1).into_iter().map(|(c, ms)| (c.pipeline, ms)).collect();
        let (short, long) = (times["[compute short]"], times["[compute long]"]);
        assert!(long > 10.0 * short.max(1.0), "short {short}, long {long}");
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
        assert!(why.contains("VICTORY_GPU_TIMING=1"), "{why}");
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
