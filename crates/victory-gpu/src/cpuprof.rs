//! CPU time of the per-draw path and of submission preparation, by stage
//! (`VICTORY_CPU_PROFILE=1`): cheap timers summed per stage and logged
//! every 5 s as milliseconds and calls per frame. Measurement only; nothing
//! else changes. Off, each timer costs one relaxed atomic load; on, about
//! 0.1 us (two clock reads), so the finest stages read a little high.
//!
//! Times are inclusive: a stage's time holds the stages timed inside it
//! (`bind` holds `tables`, which holds `table entries`), as the indentation of
//! the log shows. Recording stages run on the title's threads (they may
//! overlap each other); submission stages on the submitting thread, inside
//! the frame line's "preparing".

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering::Relaxed};
use std::time::Instant;

/// A timed stage. The order is the log's; `DEPTH` gives its indentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum Stage {
    // Recording (title threads), per draw or dispatch.
    Draw,
    Dispatch,
    Indirect,
    PipelineLookup,
    Bind,
    BindHeaps,
    BindPipeline,
    Tables,
    TableEntries,
    EntryMirror,
    MirrorMadeLock,
    ViewsLock,
    MirrorMake,
    EntryPlaced,
    EntryCopyLookup,
    BuffersLock,
    EntryDefer,
    EntryTexture,
    FreshTables,
    UserData,
    RootViewSlots,
    RootParamViews,
    MemoryTable,
    VertexIndex,
    TargetStates,
    SetTargets,
    // Submission (the submitting thread): the frame line's "preparing".
    Prepare,
    SyncHeaps,
    BindlessUploads,
    RunDeferred,
    StepRefresh,
    StepCopy,
    StepWork,
    StepGpuWrite,
    StepTexture,
    StepHeap,
    StepMemory,
    BatchFlush,
    Prelude,
    VramSync,
}

const STAGES: usize = Stage::VramSync as usize + 1;

const NAMES: [(&str, u8); STAGES] = [
    ("draw (prepare)", 0),
    ("dispatch", 0),
    ("indirect", 0),
    ("pipeline lookup", 1),
    ("bind", 1),
    ("heaps", 2),
    ("root signature + pipeline", 2),
    ("tables", 2),
    ("table entries (mirror + readable)", 3),
    ("entry mirror", 4),
    ("made-slots lock wait", 5),
    ("view records lock wait", 5),
    ("slot descriptors made", 5),
    ("entry placed texture + alias", 4),
    ("entry copy lookup", 4),
    ("buffer copies lock wait", 5),
    ("entry deferred refresh + gpu write", 4),
    ("entry texture upload + state", 4),
    ("fresh tables", 3),
    ("user data", 2),
    ("root-view slots", 3),
    ("root parameter views", 2),
    ("title memory table", 2),
    ("vertex + index buffers", 1),
    ("target states", 1),
    ("set targets", 0),
    ("submission preparing", 0),
    ("sync heaps", 1),
    ("bindless uploads", 1),
    ("run deferred", 1),
    ("refresh steps", 2),
    ("copy/fill steps", 2),
    ("work steps", 2),
    ("GPU write steps", 2),
    ("texture steps", 2),
    ("heap steps", 2),
    ("title memory steps", 2),
    ("batch flush", 2),
    ("preludes", 1),
    ("vram sync", 1),
];

/// Counted events: lookups and locks taken on the per-draw path.
#[derive(Clone, Copy, Debug)]
#[repr(usize)]
pub(crate) enum Count {
    ShadowFind,
    DeferredLock,
    Kept,
    Mirror,
    Readable,
    Barrier,
    Steps,
    CopyUpdates,
    SlotRemade,
    TableReused,
}

const COUNTS: usize = Count::TableReused as usize + 1;
const COUNT_NAMES: [&str; COUNTS] = [
    "buffer copy lookups",
    "deferred-step locks",
    "objects kept",
    "table mirrors",
    "views made readable",
    "state checks",
    "deferred steps run",
    "copy updates (range checks)",
    "slots remade",
    "tables reused",
];

static NS: [AtomicU64; STAGES] = [const { AtomicU64::new(0) }; STAGES];
static CALLS: [AtomicU64; STAGES] = [const { AtomicU64::new(0) }; STAGES];
static COUNTED: [AtomicU64; COUNTS] = [const { AtomicU64::new(0) }; COUNTS];

/// 0 not read yet, 1 off, 2 on.
static ON: AtomicU8 = AtomicU8::new(0);

/// Whether `VICTORY_CPU_PROFILE` is on.
#[inline]
pub(crate) fn on() -> bool {
    match ON.load(Relaxed) {
        2 => true,
        1 => false,
        _ => {
            let on = crate::switch::on("VICTORY_CPU_PROFILE");
            ON.store(if on { 2 } else { 1 }, Relaxed);
            on
        }
    }
}

/// Turns the profile on or off (tests and benchmarks).
#[cfg(test)]
pub(crate) fn force(on: bool) {
    ON.store(if on { 2 } else { 1 }, Relaxed);
}

/// A running timer; its time goes to its stage when dropped.
pub(crate) struct Timer(Stage, Instant);

impl Drop for Timer {
    fn drop(&mut self) {
        let ns = self.1.elapsed().as_nanos() as u64;
        NS[self.0 as usize].fetch_add(ns, Relaxed);
        CALLS[self.0 as usize].fetch_add(1, Relaxed);
    }
}

/// Times `stage` until the result is dropped (nothing when off).
#[inline]
pub(crate) fn time(stage: Stage) -> Option<Timer> {
    on().then(|| Timer(stage, Instant::now()))
}

/// Counts one `what`.
#[inline]
pub(crate) fn count(what: Count) {
    if on() {
        COUNTED[what as usize].fetch_add(1, Relaxed);
    }
}

/// The sums since the last call, zeroed: (ns, calls) per stage and counts.
pub(crate) fn take() -> ([(u64, u64); STAGES], [u64; COUNTS]) {
    (
        std::array::from_fn(|i| (NS[i].swap(0, Relaxed), CALLS[i].swap(0, Relaxed))),
        std::array::from_fn(|i| COUNTED[i].swap(0, Relaxed)),
    )
}

/// The profile over `frames` frames as text, one stage a line.
pub(crate) fn report(frames: f64, draws: f64) -> String {
    use std::fmt::Write;
    let (stages, counts) = take();
    let frames = frames.max(1.0);
    let mut text = String::new();
    let _ = write!(text, "cpu profile per frame ({draws:.0} draws/frame):");
    for (i, &(ns, calls)) in stages.iter().enumerate() {
        if calls == 0 {
            continue;
        }
        let (name, depth) = NAMES[i];
        let _ = write!(
            text,
            "\n  {:indent$}{name}: {:.2} ms, {:.0} calls, {:.2} us/call",
            "",
            ns as f64 / 1e6 / frames,
            calls as f64 / frames,
            ns as f64 / 1e3 / calls as f64,
            indent = 2 * usize::from(depth)
        );
    }
    let counted: Vec<String> = COUNT_NAMES
        .iter()
        .zip(counts)
        .filter(|(_, n)| *n > 0)
        .map(|(name, n)| format!("{name} {:.0}", n as f64 / frames))
        .collect();
    if !counted.is_empty() {
        let _ = write!(text, "\n  per frame: {}", counted.join(", "));
    }
    text
}

/// Frames presented and when the profile was last logged.
static FRAMES: AtomicU64 = AtomicU64::new(0);
static DRAWN: AtomicU64 = AtomicU64::new(0);
static LOGGED: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// A frame was presented: logs the profile every 5 s.
pub(crate) fn presented() {
    if !on() {
        return;
    }
    let frames = FRAMES.fetch_add(1, Relaxed) + 1;
    let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    let since = *logged.get_or_insert_with(Instant::now);
    if since.elapsed() < std::time::Duration::from_secs(5) {
        return;
    }
    *logged = Some(Instant::now());
    FRAMES.store(0, Relaxed);
    let drawn = crate::draw::drawn() + crate::draw::dispatched();
    let draws = (drawn - DRAWN.swap(drawn, Relaxed)) as f64 / frames as f64;
    tracing::info!("{}", report(frames as f64, draws));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cover_every_stage() {
        assert_eq!(NAMES.len(), Stage::VramSync as usize + 1);
        assert_eq!(COUNT_NAMES.len(), Count::TableReused as usize + 1);
    }

    #[test]
    fn timers_sum_per_stage_and_reset() {
        force(true);
        {
            let _t = time(Stage::SetTargets);
        }
        count(Count::Kept);
        let text = report(1.0, 0.0);
        assert!(text.contains("set targets"), "{text}");
        assert!(text.contains("objects kept"), "{text}");
        let (stages, counts) = take();
        assert_eq!(stages[Stage::SetTargets as usize].1, 0, "taken by the report");
        assert_eq!(counts[Count::Kept as usize], 0);
    }
}
