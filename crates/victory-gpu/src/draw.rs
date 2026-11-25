//! Draws on the host GPU (decision 0007, step 3). A list keeps the state
//! the title sets for its draws (pipeline, descriptor heaps, root
//! arguments, vertex and index buffers, targets); each draw then brings the
//! host list up to date: the host pipeline and our root signature, the
//! user data the translated shaders read, the host copies of the buffers
//! and descriptors they read, and the states of the textures involved.
//! State the host keeps across draws (viewports, scissors, blend factor,
//! stencil reference, topology, targets) is set on the host list as the
//! title sets it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use victory_shader::resource::RootIndexedView;
use victory_shader::{ResourceType, RootType, UsedResource};

use crate::descriptor::View;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::Interface;

use crate::host::HostList;
use crate::object::This;
use crate::pipeline::host::HostPipeline;

/// A root argument as the title set it.
#[derive(Debug, Clone, Default)]
enum Root {
    #[default]
    None,
    /// A descriptor table's first descriptor (an address in a heap).
    Table(u64),
    Constants(Vec<u32>),
    /// A root view's GPU address.
    View(u64),
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VertexView {
    address: u64,
    size: u32,
    stride: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct IndexView {
    address: u64,
    size: u32,
    format: u32,
}

/// Host states of a depth target, and of a texture shaders read.
const DEPTH_WRITE: u32 = 0x10;
const DEPTH_READ: u32 = 0x20;
const SHADER_RESOURCE: u32 = 0x40 | 0x80;

thread_local! {
    /// While a draw is prepared (`State::bind`, then its targets' states):
    /// the depth target the draw only tests (0: none, or one it writes),
    /// and whether a view its shaders read is of that same texture.
    static TESTED_DEPTH: std::cell::Cell<(usize, bool)> =
        const { std::cell::Cell::new((0, false)) };
    /// While a stage's tables are walked (`State::tables`): whether the
    /// buffer ranges its unordered access views name are given back after
    /// it: its shader can store to buffers (`Translated::stores`, or every
    /// range with `VICTORY_WRITE_SKIP=0`), and it is no quarantined stage
    /// of the structure builder.
    static STORES: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// What is given back into title memory after a shader runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GivesBack {
    /// The buffer ranges of its unordered access views and root views.
    views: bool,
    /// The copies of title memory by address (`memory.rs`).
    memory: bool,
}

/// What is given back after a shader that `stores` and stores
/// `by_address` (`Translated::stores`, `Translated::stores_by_address`)
/// runs: what it can store to (or everything, without `skip`:
/// `VICTORY_WRITE_SKIP=0`), and nothing after a `quarantined` stage of the
/// structure builder.
fn gives_back(stores: bool, by_address: bool, skip: bool, quarantined: bool) -> GivesBack {
    GivesBack {
        views: (stores || !skip) && !quarantined,
        memory: (by_address || !skip) && !quarantined,
    }
}

/// Whether a buffer range a shader names through an unordered access view
/// is given to shaders as written (snapshot before the submission, compare
/// and write back after) only if that shader's code can store to buffers.
/// The title binds buffers its shaders only read through such views, and
/// every range declared costs a snapshot and a compare of its bytes: 3.97
/// GB a frame in 31 submissions, 0.49 s of snapshots and 0.28 s of
/// comparing (run 28). On by default; `VICTORY_WRITE_SKIP=0` declares
/// every such range, as before.
fn write_skip() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WRITE_SKIP").is_none_or(|v| v != "0"))
}

/// A run of indirect records clamped in one pass ([`windows`]): the host
/// copy they came from (its number), their kind and stride, the offset of
/// the first in that copy, how many, and the list's scratch buffer that
/// holds them clamped, record `i` at `i * step`.
#[derive(Clone)]
struct Window {
    copy: u64,
    kind: u32,
    stride: u32,
    /// How far apart the records are in the copy, and in the scratch
    /// buffer: what the title walks them by ([`windows`]).
    step: u32,
    base: u64,
    records: u32,
    scratch: ID3D12Resource,
}

/// The most records clamped in one pass ([`windows`]).
const WINDOW_MOST: u64 = 64;

/// Whether indirect draws that read record after record of one argument
/// buffer are clamped a window at a time (`VICTORY_INDIRECT_WINDOW=1`; off
/// by default).
///
/// The title issues its mesh draws one record per call, each from the next
/// slot of the same argument buffer: 3,460 a frame in gameplay, and before
/// each our clamp ran as a compute dispatch of its own between two waits,
/// so the card went from drawing to compute and back for every record. In
/// the timing run of the lit scene (run 79) a shadow draw of next to
/// nothing cost 14-16 microseconds, 4,126 records a frame.
///
/// With the switch, when a draw asks for a record a short step after the
/// one drawn before, the clamp takes that record and the ones that step
/// apart after it (16, then twice as many each time, [`WINDOW_MOST`] at
/// most, never past the buffer) into
/// the list's scratch buffer in one pass, and the draws that follow read
/// their record there with no pass of ours and no barrier (the scratch
/// buffer stays readable as arguments until the next clamp), until one of
/// these: a compute
/// dispatch, a direct draw, an indirect call with a count buffer or more
/// than one record, a draw whose shaders can store to buffers, a record
/// outside the window, another host list.
///
/// The step is the one the title is seen to take, not the stride its
/// command signature names: it draws one record a call, for which the
/// stride means nothing, and the title names 12 bytes (the three
/// words of a mesh dispatch) for records it keeps 32 bytes apart (2,173 of
/// a frame's 3,349 mesh records in run 87's listed frame; 283 are 16
/// apart, which ends a window). Walked by the named stride the window
/// never opened: run 87, with the switch, still clamped 5,068 records a
/// frame one at a time.
///
/// What it costs the guard against garbage records: nothing against a
/// hang (every record is bounded as before and charged to the list's
/// budget as before), but the budget is charged when a window is clamped,
/// so also for records of the window's tail that are never drawn. Those
/// hold garbage in this title (slots of all ones: run 93 spent lists'
/// budgets on them and emptied the draws after), so in a window a record
/// that asks for more than can run is left empty and charges nothing
/// (`crate::indirect::bound_ahead`); a tail record that looks real is
/// charged what it asks, a few thousand of 2^27. And arguments the title's CPU side changes are seen as
/// before, at the next submission.
fn windows() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_INDIRECT_WINDOW").is_some_and(|v| v == "1"))
}

/// What one title list has set, and what its host list was last given.
#[derive(Default)]
pub(crate) struct State {
    pipeline: usize,
    /// View heap, sampler heap.
    heaps: [usize; 2],
    graphics: Vec<Root>,
    compute: Vec<Root>,
    vertex: [VertexView; 32],
    index: IndexView,
    /// Bound render target and depth resources, for their host states.
    targets: Vec<usize>,
    depth: usize,
    /// The host state the bound depth view needs ([`depth_state`]).
    depth_state: u32,
    /// The title's primitive topology (the host's may differ).
    topology: u32,
    /// What the title set that goes straight onto the host list, kept for
    /// the host list that continues this one after a cut ([`State::replay`]):
    /// whether a topology was set, the render target and depth handles, the
    /// viewports, the scissors, the blend factor and the stencil reference.
    topology_set: bool,
    target_handles: Vec<u64>,
    depth_handle: u64,
    targets_set: bool,
    viewports: Vec<D3D12_VIEWPORT>,
    scissors: Vec<RECT>,
    blend: Option<[f32; 4]>,
    stencil: Option<u32>,
    depth_bounds: Option<[f32; 2]>,
    /// The indirect records clamped last, if later draws may still read
    /// them ([`windows`]).
    window: Option<Window>,
    /// What the host list has: pipeline, root signature, heaps.
    host_pipeline: usize,
    /// Graphics, compute.
    host_roots: [usize; 2],
    host_heaps: [usize; 2],
    /// Tables made afresh in this list: (table, slot count, contents) → first
    /// dynamic slot.
    fresh_tables: std::collections::HashMap<(u64, u32, u64), u32>,
    /// The dynamic slots the list holds: title heap, first, count; given
    /// back when the list has run, is reset or released.
    dynamic: Vec<(usize, u32, u32)>,
    /// Root SRVs bound as one-view tables in this list: (title heap, title
    /// address; 0 for none) → dynamic slot, and the host copy the slot
    /// shows: later draws of the list bring that one up to date, whatever
    /// copy the address would find by then (`shadow.rs` `tails`).
    root_srv_slots: std::collections::HashMap<(usize, u64), RootSlot>,
    /// Root UAVs bound as one-view tables in this list, likewise.
    root_uav_slots: std::collections::HashMap<(usize, u64), RootSlot>,
    /// The list ran and its dynamic slots were given back.
    tables_gone: bool,
}

impl Drop for State {
    fn drop(&mut self) {
        for &(heap, first, n) in &self.dynamic {
            crate::descriptor::release_dynamic(heap, first, n);
        }
    }
}

impl State {
    /// The list's submission has run (submissions are waited for): the
    /// dynamic slots its tables were made in are given back, not kept
    /// until the title resets the list. A frame of the opening cutscene
    /// asks for 12,500 tables; held until the resets they ran the 131,072
    /// slots out, and up to 4,900 draws a frame went without their tables.
    /// shortcut: a list the title runs a second time without a reset then
    /// reads slots others may have taken (logged once).
    pub(crate) fn tables_done(&mut self) {
        if self.tables_gone && self.dynamic.is_empty() {
            return;
        }
        if self.tables_gone {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!("a list ran again after its descriptor tables were given back"));
        }
        for (heap, first, n) in self.dynamic.drain(..) {
            crate::descriptor::release_dynamic(heap, first, n);
        }
        self.fresh_tables.clear();
        self.root_srv_slots.clear();
        self.root_uav_slots.clear();
        self.tables_gone = true;
    }
}

/// An experiment on the title's scissor rectangles (`VICTORY_SCISSOR_TEST=full`):
/// they are all widened to the whole target. The title's room at
/// the start of the prologue lost its walls in five runs of seven, and in
/// those runs the player's body and the world outside were cut by one
/// rectangle around the window (run 85, frame 2046): one run with this on
/// says whether the walls are drawn and cut away by such a rectangle, or
/// not drawn at all. Off by default: a rectangle the title sets is part of
/// its picture.
fn scissor_test() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_SCISSOR_TEST").as_deref() == Ok("full"))
}

/// An experiment on mesh draws (`VICTORY_MESH_TEST`): 1 for `skip` (none
/// is issued), 2 for `scissor` (each runs its shaders but rasterises one
/// pixel), 0 otherwise. It tells a card hung by a mesh shader from one hung
/// by what the shader's triangles cover.
fn mesh_test() -> u8 {
    static ON: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_MESH_TEST").as_deref() {
        Ok("skip") => 1,
        Ok("scissor") => 2,
        _ => 0,
    })
}

/// `VICTORY_MESH_TEST=keep:<hex digits>[:<hex digits>]`: only the mesh
/// draws whose mesh shader's hash ends in one of the first digits (and
/// whose digit before it is one of the second, if given) are issued: a
/// search for the shaders that stop the card.
fn mesh_kept(pipeline: usize) -> bool {
    static KEEP: std::sync::OnceLock<Option<(String, String)>> = std::sync::OnceLock::new();
    let Some((last, before)) = KEEP.get_or_init(|| {
        let value = std::env::var("VICTORY_MESH_TEST").ok()?;
        let mut parts = value.strip_prefix("keep:")?.split(':');
        Some((parts.next()?.to_owned(), parts.next().unwrap_or("").to_owned()))
    }) else {
        return true;
    };
    let Some(p) = crate::pipeline::host_pipeline(pipeline as This) else {
        return true;
    };
    let Some(at) = p.name.find("mesh ") else {
        return true;
    };
    let hash = &p.name.as_bytes()[at + 5..];
    if hash.len() < 16 {
        return true;
    }
    last.as_bytes().contains(&hash[15]) && (before.is_empty() || before.as_bytes().contains(&hash[14]))
}

/// Draws recorded on the host GPU since the last frame line.
static DRAWN: AtomicU64 = AtomicU64::new(0);

/// Draws run on the host so far.
pub(crate) fn drawn() -> u64 {
    DRAWN.load(Ordering::Relaxed)
}

/// The draws traced since the last present (diagnostics builds, with
/// debug logging for this module): logged when the frame comes out black.
#[cfg(feature = "diagnostics")]
static FRAME: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The draws traced since the last call.
#[cfg(feature = "diagnostics")]
pub(crate) fn take_frame() -> Vec<String> {
    std::mem::take(&mut *FRAME.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Draws, mesh dispatches and dispatches recorded but not run on the host
/// since the last call, for any reason ([`take_dropped_why`]).
pub(crate) fn take_dropped() -> u64 {
    DROPPED.swap(0, Ordering::Relaxed)
}
/// A root SRV's dynamic slot in a list, and the host copy it shows.
type RootSlot = (u32, Option<std::sync::Arc<crate::shadow::Shadow>>);

/// Buffer views of the heap whose slot showed another host buffer than
/// the copy the pass brought up to date for them, since the start
/// (diagnostics; for the memory line). It stays 0 while the pass and the
/// slots agree; with `VICTORY_VIEW_EXTENT=0` it counts the views that were
/// read stale.
#[cfg(feature = "diagnostics")]
pub(crate) static HEAP_VIEWS_ELSEWHERE: AtomicU64 = AtomicU64::new(0);
/// Passes over the view heap's buffer views for shaders that pick them by
/// index ([`State::heap_buffers`]), and the microseconds they took.
pub(crate) static HEAP_BUFFER_PASSES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_BUFFER_MICROS: AtomicU64 = AtomicU64::new(0);
/// Where the heap pass spends its time, in microseconds, and what it did:
/// 0 the buffer slots looked at, 1 the buffer copies refreshed, 2 the
/// texture queue asked (`texture_slots_remapped`, `texture_slots_due`),
/// 3 the mapped bytes of placed textures, 4 the host view and host texture
/// made (`mirror`, `placed_texture`), 5 the upload recorded (`readable`),
/// 6 the rest of a texture's turn; counts: 7 texture slots looked at,
/// 8 textures uploaded, 9 passes ended by the time, 10 by the bytes.
pub(crate) static HEAP_PHASES: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];
/// Textures brought up to date for shaders that pick them by index
/// ([`State::heap_textures`]), the bytes of their memory, and the slots
/// still waiting.
pub(crate) static HEAP_TEXTURES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURE_BYTES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURES_WAITING: AtomicU64 = AtomicU64::new(0);
/// Passes that made no texture because the card had no room
/// (`crate::resource::room`), for the memory line.
pub(crate) static HEAP_TEXTURES_FULL: AtomicU64 = AtomicU64::new(0);

/// Content checks that found a texture unchanged, or queued a fresh upload.
pub(crate) static HEAP_TEXTURES_RECHECKED: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
pub(crate) static HEAP_TEXTURE_RECHECK_US: AtomicU64 = AtomicU64::new(0);
pub(crate) static EARLY_TEXTURE_PASSES: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static EARLY_TEXTURE_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);

/// Pixel shaders that bake decoration textures once while the world loads.
const BAKE_PIXEL_SHADERS: [&str; 4] =
    ["c0a7fe77b1862e10", "0d1575a396dfe513", "b7cfd8dda91ebb6b", "280967ad158cee0b"];

fn early_texture_limits() -> (Duration, Duration) {
    static LIMITS: std::sync::OnceLock<(Duration, Duration)> = std::sync::OnceLock::new();
    *LIMITS.get_or_init(|| {
        let limit = |name| {
            let ms = std::env::var(name).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
            Duration::from_millis(ms)
        };
        (limit("VICTORY_EARLY_TEXTURES_MS"), limit("VICTORY_BAKE_TEXTURES_MS"))
    })
}

enum TexturePass {
    Indexed,
    Early { regular: Duration, bake: Duration },
}

struct TextureUploads {
    frame: Option<u64>,
    spent: u64,
    early: bool,
    bake_spent: u64,
    bake_time: Duration,
}

impl TextureUploads {
    const fn new(frame: Option<u64>) -> Self {
        Self { frame, spent: 0, early: false, bake_spent: 0, bake_time: Duration::ZERO }
    }

    fn budget(&mut self, mode: TexturePass) -> Option<(Duration, u64, bool)> {
        match mode {
            TexturePass::Indexed => Some((Duration::from_millis(250), 192 << 20, false)),
            TexturePass::Early { regular, bake } => {
                let bake_time = bake.saturating_sub(self.bake_time);
                let bake_bytes = (2u64 << 30).saturating_sub(self.bake_spent);
                if !bake_time.is_zero() && bake_bytes > 0 {
                    self.early = true;
                    Some((bake_time, self.spent.saturating_add(bake_bytes), true))
                } else if !self.early && !regular.is_zero() {
                    self.early = true;
                    Some((regular, self.spent.saturating_add(64 << 20), false))
                } else {
                    None
                }
            }
        }
    }
}

/// Uploads of textures already on the card made while it had no room for
/// new ones, since the start.
pub(crate) static HEAP_TEXTURES_REUPLOADED_FULL: AtomicU64 = AtomicU64::new(0);

/// Whether a texture that has its host copy is still brought up to date
/// while the card has no room for new ones (`VICTORY_REUPLOAD_WHEN_FULL=0`:
/// no).
fn reupload_when_full() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_REUPLOAD_WHEN_FULL").is_none_or(|v| v != "0"))
}

/// Whether textures picked by index are looked at again after their upload
/// (`VICTORY_HEAP_RECHECK=0`: no).
fn heap_recheck() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_RECHECK").is_none_or(|v| v != "0"))
}
/// Times a placed texture's slot was passed over because its memory is not
/// mapped.
pub(crate) static HEAP_TEXTURES_AWAY: AtomicU64 = AtomicU64::new(0);
/// Slots the pass found up to date already: their texture is uploaded by
/// the draws that read it through a table (it has a staging buffer).
pub(crate) static HEAP_TEXTURES_TABLED: AtomicU64 = AtomicU64::new(0);
/// The queue of textures picked by index at the last pass
/// (`TextureQueue::census`): slots in it, due, waiting for their memory,
/// uploaded in part, and the frames the oldest due one has waited.
pub(crate) static HEAP_TEXTURES_QUEUE: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
/// Since the start: looks at a slot uploaded in part that found no more of
/// its memory mapped; uploads of textures not all mapped; and of those,
/// the ones whose first page was not there (asked by the first page alone,
/// as before, they counted as not there at all).
pub(crate) static HEAP_TEXTURES_PARTLY: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Since the start: looks that found more of a placed texture's memory
/// mapped than at its last look although no notice of the runtime
/// (`descriptor::host::remapped`) had made its slot due: memory that
/// arrives unannounced, which only the looks find ([`park_frames`]).
pub(crate) static HEAP_TEXTURES_UNANNOUNCED: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// Why draws and dispatches were dropped, since the last frame line (their
/// sum is [`take_dropped`]'s): the words the frame line names them by.
pub(crate) const DROP_REASONS: [&str; 9] = [
    "no host pipeline",
    "a stage of the structure builder, which is off",
    "a pipeline of another kind",
    "binding refused",
    "an index buffer without a host copy",
    "rectangles not expanded",
    "mesh dispatches switched off",
    "skipped (VICTORY_SKIP_PIPELINES)",
    "a host list without mesh dispatches",
];
const NO_PIPELINE: usize = 0;
const BUILDER_OFF: usize = 1;
const OTHER_KIND: usize = 2;
const BIND: usize = 3;
const NO_INDEX_COPY: usize = 4;
const RECTS: usize = 5;
const MESH_OFF: usize = 6;
#[cfg(feature = "diagnostics")]
const SKIPPED: usize = 7;
const NO_MESH_LIST: usize = 8;
static DROPPED_WHY: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];
thread_local! {
    /// Why the last [`State::prepare_for`] on this thread gave up.
    static PREPARE_WHY: std::cell::Cell<usize> = const { std::cell::Cell::new(NO_PIPELINE) };
}

/// A draw or dispatch dropped, for `why` (one of the [`DROP_REASONS`]).
fn drop_as(why: usize) {
    DROPPED.fetch_add(1, Ordering::Relaxed);
    DROPPED_WHY[why].fetch_add(1, Ordering::Relaxed);
}

/// The drops since the last call, by reason ([`DROP_REASONS`]).
pub(crate) fn take_dropped_why() -> [u64; 9] {
    DROPPED_WHY.each_ref().map(|n| n.swap(0, Ordering::Relaxed))
}

/// `VICTORY_TEXTURE_PARK_FRAMES=<n>`: a texture uploaded in part whose look
/// found no more of its memory mapped is looked at again after up to `n`
/// frames, not after at most 256 (or less: about 4 s). The runtime's
/// notices make it due at once when memory is mapped, unmapped or written
/// under it, so the looks are only for memory that arrives without one: 0,
/// the default, keeps them as they are. Each look asks the kernel about
/// every 64 KiB of the texture (4.5 ms a frame on average in a release
/// run, 241 slots looked at, peaks of 13-16 ms). To try only where the
/// frame line's "arrived unannounced" stays at about 0 through a long run
/// with streaming: a texture whose memory arrived so stays partly uploaded
/// for up to `n` frames.
pub(crate) fn park_frames() -> u64 {
    static FRAMES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *FRAMES.get_or_init(|| {
        std::env::var("VICTORY_TEXTURE_PARK_FRAMES")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    })
}

/// Marks the last traced draw as not run on the host.
#[cfg(feature = "diagnostics")]
fn dropped(pipeline: usize) {
    crate::pipeline::note_drop(pipeline as This);
    if let Some(last) = FRAME.lock().unwrap_or_else(|e| e.into_inner()).last_mut() {
        last.push_str(" DROPPED");
    }
}

/// Dispatches run on the host so far.
static DISPATCHED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn dispatched() -> u64 {
    DISPATCHED.load(Ordering::Relaxed)
}

/// The host address of a root view: its buffer's host copy, or for a
/// constant buffer not 256-byte aligned there (the console needs 16) a
/// copy in upload memory.
/// shortcut: a root view has no size; constant buffers are taken as 64 KiB
/// (cut at their buffer's end), other views as all that is left of their
/// buffer. (They were taken as 1 MiB: 1,452 of the 2,021 mesh vertex
/// streams of a cutscene frame reach further, and what lay past the first
/// megabyte was never brought up to date - meshes without bodies, exploded
/// triangles. `VICTORY_ROOT_VIEW_MB` sets a limit again.)
fn root_view(list: &HostList, address: u64, srv: bool) -> Option<u64> {
    static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let limit = *LIMIT.get_or_init(|| {
        std::env::var("VICTORY_ROOT_VIEW_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(u64::MAX, |mb| mb << 20)
    });
    let shadow = crate::shadow::find(address, 1)?;
    let len = shadow
        .left(address)
        .min(if srv { limit } else { 1 << 16 });
    let gpu = shadow.gpu(address);
    if srv || gpu % 256 == 0 {
        shadow.refresh(list, address, len);
        return Some(gpu);
    }
    let (gpu, to) = list.scratch(len)?;
    list.defer_fill(to, address as *const u8, len as usize);
    Some(gpu)
}

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

fn set(roots: &mut Vec<Root>, slot: u32, root: Root) {
    let slot = slot as usize;
    if roots.len() <= slot {
        roots.resize(slot + 1, Root::None);
    }
    roots[slot] = root;
}

impl State {
    pub(crate) fn reset(&mut self, pipeline: This) {
        *self = State::default();
        self.pipeline = pipeline as usize;
    }

    /// The title's topology; console-only ones become host lists (see
    /// [`crate::primitive`]).
    pub(crate) fn set_topology(&mut self, list: &HostList, topology: u32) {
        self.topology = topology;
        self.topology_set = true;
        let host = match topology {
            RECTANGLES => 4,
            TRIANGLE_FAN | LINE_LOOP | QUADS | QUAD_STRIP => {
                once!(topology, "console topology drawn as a triangle list");
                4
            }
            t => t,
        };
        // SAFETY: an open list.
        unsafe {
            list.list
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(host as i32))
        };
    }

    pub(crate) fn set_pipeline(&mut self, pipeline: This) {
        self.pipeline = pipeline as usize;
    }

    /// A root signature that directly indexes heaps must follow this binding.
    fn bind_heaps(&mut self, list: &HostList) {
        if self.host_heaps != self.heaps {
            let heaps: Vec<Option<ID3D12DescriptorHeap>> = self
                .heaps
                .iter()
                .filter(|&&h| h != 0)
                .filter_map(|&h| crate::descriptor::heap(h as This).host())
                .map(|h| Some(h.heap.clone()))
                .collect();
            heaps.iter().flatten().for_each(|h| list.keep(h));
            // SAFETY: an open list and shader-visible heaps.
            unsafe { list.list.SetDescriptorHeaps(&heaps) };
            self.host_heaps = self.heaps;
            self.host_roots = [0; 2];
        }
    }

    /// A host library can replace the bindings tracked by the title.
    pub(crate) fn host_state_lost(&mut self) {
        (self.host_pipeline, self.host_roots, self.host_heaps) = (0, [0; 2], [usize::MAX; 2]);
    }

    pub(crate) fn set_heaps(&mut self, heaps: &[This]) {
        self.heaps = [0; 2];
        for &h in heaps {
            if !h.is_null() {
                let samplers = crate::descriptor::heap(h).samplers();
                self.heaps[usize::from(samplers)] = h as usize;
            }
        }
    }

    fn roots(&mut self, compute: bool) -> &mut Vec<Root> {
        if compute {
            &mut self.compute
        } else {
            &mut self.graphics
        }
    }

    pub(crate) fn set_table(&mut self, compute: bool, slot: u32, handle: u64) {
        set(self.roots(compute), slot, Root::Table(handle));
    }

    pub(crate) fn set_view(&mut self, compute: bool, slot: u32, address: u64) {
        set(self.roots(compute), slot, Root::View(address));
    }

    pub(crate) fn set_constants(&mut self, compute: bool, slot: u32, values: &[u32], offset: u32) {
        let roots = self.roots(compute);
        let s = slot as usize;
        if !matches!(roots.get(s), Some(Root::Constants(_))) {
            set(roots, slot, Root::Constants(Vec::new()));
        }
        if let Root::Constants(c) = &mut roots[s] {
            let end = offset as usize + values.len();
            if c.len() < end {
                c.resize(end, 0);
            }
            c[offset as usize..end].copy_from_slice(values);
        }
    }

    pub(crate) fn set_vertex_buffers(&mut self, start: u32, views: &[VertexView]) {
        for (i, v) in views.iter().enumerate() {
            if let Some(slot) = self.vertex.get_mut(start as usize + i) {
                *slot = *v;
            }
        }
    }

    pub(crate) fn clear_vertex_buffers(&mut self, start: u32, count: u32) {
        for slot in self.vertex.iter_mut().skip(start as usize).take(count as usize) {
            *slot = VertexView::default();
        }
    }

    pub(crate) fn set_index_buffer(&mut self, view: Option<&IndexView>) {
        self.index = view.copied().unwrap_or_default();
    }

    /// Render targets and depth, set on the host list at once.
    pub(crate) fn set_targets(&mut self, list: &HostList, handles: &[u64], depth: u64) {
        let Some(host) = crate::host::host() else {
            return;
        };
        self.target_handles.clear();
        self.target_handles.extend_from_slice(handles);
        self.depth_handle = depth;
        self.targets_set = true;
        self.targets.clear();
        let mut rtvs = Vec::new();
        for &h in handles {
            let Some(record) = crate::descriptor::rtv_record(h) else {
                once!(
                    handle = format_args!("{h:#x}"),
                    "render target without a recorded view"
                );
                continue;
            };
            let Some(texture) = crate::resource::host_resource(record.resource) else {
                // A typed null view (or a target without a host copy) still
                // takes its slot: left out, the next live target would move
                // into it and the pixel shader's outputs reach the wrong
                // targets.
                // shortcut: a view without a description or a format has no
                // null view to stand for it and is left out, as before.
                let null = record
                    .desc()
                    .filter(|d| d.Format.0 != 0 && d.ViewDimension.0 != 0);
                if let Some(desc) = null {
                    rtvs.push(host.rtv(None, Some(&desc)));
                }
                continue;
            };
            self.targets.push(record.resource as usize);
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(record.resource);
            rtvs.push(host.rtv(Some(texture), record.desc().as_ref()));
        }
        self.depth = 0;
        self.depth_state = DEPTH_WRITE;
        let dsv = crate::descriptor::dsv_record(depth).and_then(|record| {
            let texture = crate::resource::host_resource(record.resource)?;
            self.depth = record.resource as usize;
            self.depth_state = depth_state(texture, record.depth_desc().as_ref());
            if crate::resource::depth_and_unordered(record.resource) {
                crate::resource::DEPTH_UNORDERED[1].fetch_add(1, Ordering::Relaxed);
            }
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(record.resource);
            Some(host.dsv(texture, record.depth_desc().as_ref()))
        });
        // SAFETY: an open list; the views were just made.
        unsafe {
            list.list.OMSetRenderTargets(
                rtvs.len() as u32,
                (!rtvs.is_empty()).then_some(rtvs.as_ptr()),
                false,
                dsv.as_ref().map(std::ptr::from_ref),
            )
        };
    }

    /// Viewports, scissors, the blend factor and the stencil reference: set
    /// on the host list at once, and kept for [`State::replay`].
    pub(crate) fn set_viewports(&mut self, list: &HostList, viewports: &[D3D12_VIEWPORT]) {
        self.viewports.clear();
        self.viewports.extend_from_slice(viewports);
        crate::draw::viewports(list, viewports);
    }

    pub(crate) fn set_scissors(&mut self, list: &HostList, rects: &[RECT]) {
        self.scissors.clear();
        self.scissors.extend_from_slice(rects);
        if scissor_test() {
            // The experiment: every rectangle the title sets is widened to
            // the largest target there is, so nothing is cut by one. What
            // the listing shows (`op_text`) stays the title's own.
            let full = RECT {
                left: 0,
                top: 0,
                right: 16384,
                bottom: 16384,
            };
            crate::draw::scissors(list, &vec![full; rects.len()]);
            return;
        }
        crate::draw::scissors(list, rects);
    }

    pub(crate) fn set_blend_factor(&mut self, list: &HostList, factor: Option<&[f32; 4]>) {
        self.blend = factor.copied();
        crate::draw::blend_factor(list, factor);
    }

    pub(crate) fn set_stencil_ref(&mut self, list: &HostList, reference: u32) {
        self.stencil = Some(reference);
        crate::draw::stencil_ref(list, reference);
    }

    pub(crate) fn set_depth_bounds(&mut self, list: &HostList, min: f32, max: f32) {
        self.depth_bounds = Some([min, max]);
        crate::draw::depth_bounds(list, min, max);
    }

    /// `list` is a new host list that continues the title's list (it was
    /// cut at a wait for a memory value, `crate::list::cut`): a new host
    /// list has nothing set, so what the title set so far is set again.
    /// The pipeline, the root signatures and the heaps are set by the next
    /// draw or dispatch (`bind`, which is told here that the host list has
    /// none); root arguments, vertex and index buffers are set by every
    /// draw anyway. The dynamic descriptor slots and the tables made in
    /// them stay: the parts before the cut have not run yet, and they are
    /// given back when the title's list is reset.
    /// shortcut: the render target views are made again from the handles'
    /// descriptors as they are now, not as they were when the title set
    /// the targets (a descriptor rewritten in between names another view).
    pub(crate) fn replay(&mut self, list: &HostList) {
        self.host_pipeline = 0;
        self.host_roots = [0; 2];
        self.host_heaps = [0; 2];
        if self.topology_set {
            self.set_topology(list, self.topology);
        }
        if self.targets_set {
            let handles = std::mem::take(&mut self.target_handles);
            self.set_targets(list, &handles, self.depth_handle);
        }
        if !self.viewports.is_empty() {
            crate::draw::viewports(list, &self.viewports);
        }
        if !self.scissors.is_empty() {
            let rects = std::mem::take(&mut self.scissors);
            self.set_scissors(list, &rects);
        }
        if let Some(factor) = self.blend {
            crate::draw::blend_factor(list, Some(&factor));
        }
        if let Some(reference) = self.stencil {
            crate::draw::stencil_ref(list, reference);
        }
        if let Some([min, max]) = self.depth_bounds {
            crate::draw::depth_bounds(list, min, max);
        }
    }

    /// The user-data value of a root argument, as translated shaders read
    /// it: a table's byte offset in its heap, a constant's value.
    fn user_value(&self, compute: bool, slot: u8, root: RootType, dword: u32) -> u32 {
        match (self.roots_ref(compute).get(usize::from(slot)), root) {
            (Some(Root::Table(handle)), RootType::Table) => match self.heap_of(*handle) {
                Some(heap) => (handle - heap.start) as u32,
                None => {
                    once!(
                        handle = format_args!("{handle:#x}"),
                        "descriptor table outside the bound heaps"
                    );
                    0
                }
            },
            (Some(Root::Constants(c)), RootType::Constants) => {
                c.get(dword as usize).copied().unwrap_or(0)
            }
            // The host's own ([`Self::host_word`]): no root argument.
            (_, RootType::Other(_)) => 0,
            _ => {
                once!(
                    slot,
                    ?root,
                    "a shader reads a root argument the title did not set"
                );
                0
            }
        }
    }

    /// A user-data dword the host computes for a translated shader
    /// (`victory_shader::emit::USER_MEMORY_BASE`, `USER_VIEW_ADDRESS`).
    fn host_word(
        &self,
        compute: bool,
        kind: u32,
        slot: u8,
        dword: u32,
        memory: Option<&crate::memory::Ready>,
    ) -> u32 {
        use victory_shader::emit::{USER_MEMORY_BASE, USER_VIEW_ADDRESS, USER_VIEW_BIAS};
        if kind == USER_MEMORY_BASE {
            return memory.map_or(0, |m| m.base);
        }
        if kind == USER_VIEW_BIAS {
            // The view begins on 16 bytes of the copy the address finds
            // (`root_uav`, `root_srv_table`).
            // shortcut: found again here, not taken from the list's slot.
            return match self.roots_ref(compute).get(usize::from(slot)) {
                Some(&Root::View(address)) => crate::shadow::find(address, 1).map_or(0, |s| {
                    // SAFETY: a live buffer.
                    let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                    ((s.gpu(address) - base) & 15) as u32
                }),
                _ => 0,
            };
        }
        if kind != USER_VIEW_ADDRESS && kind != USER_VIEW_ADDRESS + 1 {
            once!(kind, "a shader reads a host value this build does not know");
            return 0;
        }
        // The title address of the buffer view at that dword of the table.
        let address = match self.roots_ref(compute).get(usize::from(slot)) {
            Some(&Root::Table(table)) => crate::descriptor::view(table + u64::from(dword) * 4)
                .and_then(|view| crate::descriptor::host::buffer_range(&view))
                .map_or(0, |range| range.0),
            _ => 0,
        };
        if address == 0 {
            once!(slot, dword, "a shader takes the address of a buffer view that is not there: it reads 0");
        }
        if kind == USER_VIEW_ADDRESS {
            address as u32
        } else {
            (address >> 32) as u32
        }
    }

    /// For a shader that reads title memory by address
    /// ([`crate::memory`]): its directory, fault notes and view slots,
    /// with the pages of the root views the list has set made ready.
    fn memory_ready(&self, list: &HostList, compute: bool, writes: bool) -> Option<crate::memory::Ready> {
        let heap = match self.heaps[0] {
            0 => None,
            h => crate::descriptor::heap(h as This).host(),
        }?;
        let (mut addresses, mut constants) = (Vec::new(), Vec::new());
        for root in self.roots_ref(compute) {
            match root {
                Root::View(address) => addresses.push(*address),
                Root::Constants(values) => constants.extend_from_slice(values),
                _ => {}
            }
        }
        crate::memory::ready(list, heap, &addresses, &constants, writes)
    }

    /// The bound heap holding `handle`.
    fn heap_of(&self, handle: u64) -> Option<&'static crate::descriptor::Heap> {
        self.heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| h.index(handle).is_some())
    }

    /// Brings the host list up to date for a draw; false if the draw cannot
    /// run on the host.
    fn prepare(&mut self, list: &HostList, indexed: bool) -> bool {
        self.prepare_for(list, indexed, false)
    }

    /// As [`State::prepare`], for draws (`mesh` false) or mesh dispatches.
    fn prepare_for(&mut self, list: &HostList, indexed: bool, mesh: bool) -> bool {
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            PREPARE_WHY.set(SKIPPED);
            return false;
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            PREPARE_WHY.set(NO_PIPELINE);
            return false;
        };
        if p.compute || p.mesh != mesh {
            once!(mesh, "a draw or mesh dispatch with another kind of pipeline");
            PREPARE_WHY.set(OTHER_KIND);
            return false;
        }
        if p.rects && self.topology != RECTANGLES {
            // Its geometry stage would turn each triangle into a rectangle.
            once!("a rectangle pipeline drawn with another topology");
        }
        // A depth texture shaders also write: what they wrote, first, and
        // the depth plane drawn from its twin before a draw tests it.
        if self.depth != 0 && list.twin_use_now(self.depth as This) {
            // The plane's pass set its own heap, root signature, pipeline,
            // target, viewport, scissor and topology.
            self.replay(list);
        }
        if !self.bind(list, p, false) {
            PREPARE_WHY.set(BIND);
            return false;
        }
        let l = &list.list;
        // Vertex buffers the input layout reads.
        let mut views = [D3D12_VERTEX_BUFFER_VIEW::default(); 32];
        let mut count = 0;
        for (slot, (view, &v)) in views.iter_mut().zip(&self.vertex).enumerate() {
            if p.slots & 1 << slot == 0 {
                continue;
            }
            count = slot + 1;
            if v.address == 0 || v.size == 0 {
                continue;
            }
            let Some(shadow) = crate::shadow::find_any(v.address, u64::from(v.size)) else {
                continue;
            };
            shadow.refresh(list, v.address, u64::from(v.size));
            *view = D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: shadow.gpu(v.address),
                SizeInBytes: v.size,
                StrideInBytes: v.stride,
            };
        }
        // SAFETY: an open list; the views name live host buffers.
        unsafe {
            if count > 0 {
                l.IASetVertexBuffers(0, Some(&views[..count]));
            }
        }
        if indexed {
            let v = self.index;
            let Some(shadow) = crate::shadow::find_any(v.address, u64::from(v.size)) else {
                PREPARE_WHY.set(NO_INDEX_COPY);
                return false;
            };
            shadow.refresh(list, v.address, u64::from(v.size));
            let view = D3D12_INDEX_BUFFER_VIEW {
                BufferLocation: shadow.gpu(v.address),
                SizeInBytes: v.size,
                Format: DXGI_FORMAT(v.format as i32),
            };
            // SAFETY: an open list; the view names a live host buffer.
            unsafe { l.IASetIndexBuffer(Some(&view)) };
        }
        const RENDER_TARGET: u32 = 0x4;
        for &t in &self.targets {
            crate::list::ensure_state(list, t as This, RENDER_TARGET);
        }
        if self.depth != 0 {
            // (what shaders wrote of it is in: `twin_use_now` above)
            // A view that writes neither plane keeps the texture readable:
            // the depth read state, together with the shader read states
            // if this draw's shaders read the same texture (the write state
            // put it out of the state those views need, and a move to it
            // and back for every such draw).
            let state = if self.depth_state == DEPTH_READ {
                let (tested, read) = TESTED_DEPTH.get();
                if read && tested == self.depth {
                    DEPTH_READ | SHADER_RESOURCE
                } else {
                    DEPTH_READ
                }
            } else {
                DEPTH_WRITE
            };
            crate::list::ensure_state(list, self.depth as This, state);
        }
        true
    }

    /// A descriptor a shader reads through a table: its host slot made if
    /// it changed, constant data copied, textures made readable.
    /// What draws and dispatches share: heaps, our root signature, the
    /// pipeline, and per stage the user data, root views and the
    /// descriptors its tables hold.
    /// False if the draw or dispatch must not run: a root SRV it reads
    /// could not be given a view.
    pub(crate) fn bind(&mut self, list: &HostList, p: &HostPipeline, compute: bool) -> bool {
        let l = &list.list;
        if !compute {
            self.early_textures(list, p);
        }
        if crate::host::walls_trace() {
            self.walls_census(p, compute);
        }
        self.bind_heaps(list);
        // The depth target a draw only tests, for the views of it that the
        // draw's shaders read (`readable`).
        let tested = if !compute && self.depth_state == DEPTH_READ {
            self.depth
        } else {
            0
        };
        TESTED_DEPTH.set((tested, false));
        let root = p.root.as_raw() as usize;
        let root_changed = self.host_roots[usize::from(compute)] != root;
        if root_changed {
            // SAFETY: an open list and a live root signature.
            unsafe {
                if compute {
                    l.SetComputeRootSignature(&p.root);
                } else {
                    l.SetGraphicsRootSignature(&p.root);
                }
            }
            self.host_roots[usize::from(compute)] = root;
        }
        // By host object: a title pipeline released and another made at its
        // address is another host pipeline (the list keeps the old one).
        let pso = p.pso.as_raw() as usize;
        let pipeline_changed = self.host_pipeline != pso;
        if pipeline_changed {
            // SAFETY: an open list and a live pipeline.
            unsafe { l.SetPipelineState(&p.pso) };
            self.host_pipeline = pso;
        }
        if pipeline_changed || root_changed {
            list.keep_pipeline(p);
        }
        // What a quarantined stage of the structure builder stores stays in
        // the host's copies: none of it is given back, by view or by address.
        let quarantined = crate::pipeline::builder_quarantined(self.pipeline as This);
        for stage in &p.stages {
            let t = stage.translated();
            let back = gives_back(t.stores, t.stores_by_address, write_skip(), quarantined);
            STORES.set(back.views);
            let made = self.tables(list, &t.used, &t.host.root_indexed_views, compute);
            STORES.set(true);
            if made.iter().any(|m| m.1 == u32::MAX) {
                once!("a draw whose descriptor table found no free slots is dropped");
                return false;
            }
            // What the shader stores through root views is copied back into
            // title memory afterwards; the copies of title memory by address
            // only after a shader that stores by address.
            let (writes, writes_memory) = (back.views, back.memory);
            // Title memory by address: what the shader finds it by.
            let memory = if t.host.root_srvs.contains(&victory_shader::emit::SLOT_MEMORY) {
                let Some(ready) = self.memory_ready(list, compute, writes_memory) else {
                    once!("a shader that reads title memory by address without a view heap to hold its views: the draw is dropped");
                    return false;
                };
                Some(ready)
            } else {
                None
            };
            if let Some((param, dwords)) = stage.user_data {
                let mut values = vec![0u32; dwords as usize];
                for u in &t.host.user_data {
                    if let RootType::Other(kind) = u.root {
                        values[u.index as usize] =
                            self.host_word(compute, kind, u.slot, u.dword, memory.as_ref());
                        continue;
                    }
                    let fresh = made
                        .iter()
                        .find(|m| m.0 == u.slot && u.root == RootType::Table);
                    values[u.index as usize] = match fresh {
                        Some(&(_, offset)) => offset,
                        None => self.user_value(compute, u.slot, u.root, u.dword),
                    };
                }
                // SAFETY: an open list; `values` covers the parameter.
                unsafe {
                    if compute {
                        l.SetComputeRoot32BitConstants(param, dwords, values.as_ptr().cast(), 0);
                    } else {
                        l.SetGraphicsRoot32BitConstants(param, dwords, values.as_ptr().cast(), 0);
                    }
                }
            }
            for (views, srv) in [(&stage.root_cbvs, false), (&stage.root_srvs, true)] {
                for &(slot, param) in views {
                    if srv && slot == victory_shader::emit::SLOT_MEMORY {
                        // Ours: the directory of title memory by address.
                        if let Some(m) = &memory {
                            // SAFETY: an open list; the directory lives as
                            // long as the process and the list holds it.
                            unsafe {
                                if compute {
                                    l.SetComputeRootShaderResourceView(param, m.directory);
                                } else {
                                    l.SetGraphicsRootShaderResourceView(param, m.directory);
                                }
                            }
                        }
                        continue;
                    }
                    if srv && slot == victory_shader::emit::SLOT_STRIDES {
                        // Ours, not the title's: the shader picks buffers
                        // from the view heap by an index it computes.
                        match self.heap_buffers(list) {
                            // SAFETY: an open list; the table lives as long
                            // as the heap the list holds.
                            Some(gpu) => unsafe {
                                if compute {
                                    l.SetComputeRootShaderResourceView(param, gpu);
                                } else {
                                    l.SetGraphicsRootShaderResourceView(param, gpu);
                                }
                            },
                            None => once!("a shader picks buffers from a view heap that is not set"),
                        }
                        continue;
                    }
                    let tabled = srv && crate::pipeline::host::bounded_root_srvs();
                    let Some(&Root::View(address)) = self.roots_ref(compute).get(usize::from(slot))
                    else {
                        once!(slot, "a shader reads a root view the title did not set");
                        // A table left unset is undefined: a null view.
                        if tabled && !self.root_srv_table(list, None, param, compute, false) {
                            return false;
                        }
                        continue;
                    };
                    // The shader adds the bytes a raw view begins early by.
                    let biased = srv
                        && t.host.user_data.iter().any(|u| {
                            u.slot == slot
                                && u.root == RootType::Other(victory_shader::emit::USER_VIEW_BIAS)
                        });
                    if tabled {
                        if !self.root_srv_table(list, Some(address), param, compute, biased) {
                            return false;
                        }
                        continue;
                    }
                    let Some(mut gpu) = root_view(list, address, srv) else {
                        continue;
                    };
                    if biased {
                        gpu -= u64::from(self.host_word(
                            compute,
                            victory_shader::emit::USER_VIEW_BIAS,
                            slot,
                            0,
                            None,
                        ));
                    }
                    // SAFETY: an open list; the address is in live memory.
                    unsafe {
                        match (compute, srv) {
                            (false, false) => l.SetGraphicsRootConstantBufferView(param, gpu),
                            (false, true) => l.SetGraphicsRootShaderResourceView(param, gpu),
                            (true, false) => l.SetComputeRootConstantBufferView(param, gpu),
                            (true, true) => l.SetComputeRootShaderResourceView(param, gpu),
                        }
                    }
                }
            }
            for &(slot, param) in &stage.root_uavs {
                if slot == victory_shader::emit::SLOT_FAULTS {
                    // Ours: where shaders note the pages they found empty.
                    if let Some(m) = &memory {
                        // SAFETY: an open list; the buffer lives as long as
                        // the process and the list holds it.
                        unsafe {
                            if compute {
                                l.SetComputeRootUnorderedAccessView(param, m.faults);
                            } else {
                                l.SetGraphicsRootUnorderedAccessView(param, m.faults);
                            }
                        }
                    }
                    continue;
                }
                let address = match self.roots_ref(compute).get(usize::from(slot)) {
                    Some(&Root::View(address)) => Some(address),
                    _ => {
                        once!(slot, "a shader writes through a root view the title did not set");
                        None
                    }
                };
                if !self.root_uav(list, address, param, compute, writes) {
                    return false;
                }
            }
        }
        // Depth textures it writes through unordered access: their colour
        // twins hold what they lack from here on.
        list.twins_written();
        true
    }

    /// Binds a root UAV: a raw unordered access view of the title buffer's
    /// host copy from `address` on, as a one-view table (a null view
    /// without an address, without a host copy shaders can write, or at an
    /// address not on 16 bytes), or as a root descriptor with
    /// `VICTORY_ROOT_SRV_DIRECT=1` (the dispatch is dropped then if there
    /// is nothing to bind). With `writes`, what the shader stored is copied
    /// back into title memory when the list has run. False if the draw
    /// must be dropped.
    /// shortcut: a root view has no size: the view, the refresh and the
    /// copy back cover all that is left of the title's buffer
    /// (`VICTORY_ROOT_UAV_MB` sets a limit for all three).
    fn root_uav(
        &mut self,
        list: &HostList,
        address: Option<u64>,
        param: u32,
        compute: bool,
        writes: bool,
    ) -> bool {
        const UNORDERED: i32 = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0;
        static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let limit = *LIMIT.get_or_init(|| {
            std::env::var("VICTORY_ROOT_UAV_MB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map_or(u64::MAX, |mb| mb << 20)
        });
        let find = |a: u64| {
            let s = crate::shadow::find(a, 1)?;
            // SAFETY: a live buffer.
            let flags = unsafe { s.buffer.GetDesc() }.Flags;
            if flags.0 & UNORDERED == 0 {
                once!("a root UAV of a buffer whose host copy shaders cannot write");
                return None;
            }
            Some(s)
        };
        let ready = |a: u64, s: &std::sync::Arc<crate::shadow::Shadow>| {
            let len = s.left(a).min(limit);
            s.refresh(list, a, len);
            if writes {
                s.gpu_writes(list, a, len);
            }
        };
        if !crate::pipeline::host::bounded_root_srvs() {
            let Some((a, s)) = address.and_then(|a| Some((a, find(a)?))) else {
                once!("a root UAV with nothing to bind: the draw is dropped");
                return false;
            };
            ready(a, &s);
            // From the 16 bytes below: the shader adds the rest.
            // SAFETY: a live buffer.
            let base = unsafe { s.buffer.GetGPUVirtualAddress() };
            let gpu = base + ((s.gpu(a) - base) & !15);
            // SAFETY: an open list; the address is in a live buffer.
            unsafe {
                if compute {
                    list.list.SetComputeRootUnorderedAccessView(param, gpu);
                } else {
                    list.list.SetGraphicsRootUnorderedAccessView(param, gpu);
                }
            }
            return true;
        }
        let heap = match self.heaps[0] {
            0 => None,
            h => Some(crate::descriptor::heap(h as This)),
        };
        let Some((heap, host)) = heap.and_then(|h| Some((h, h.host()?))) else {
            once!("a root UAV without a bound view heap: the draw is dropped");
            return false;
        };
        let key = (std::ptr::from_ref(heap) as usize, address.unwrap_or(0));
        let (index, shadow) = match self.root_uav_slots.get(&key) {
            Some((index, shadow)) => (*index, address.zip(shadow.clone())),
            None => {
                let shadow = address.and_then(|a| find(a).map(|s| (a, s)));
                let Some(index) = host.dynamic(1) else {
                    return false;
                };
                self.dynamic.push((key.0, index, 1));
                if let Some((a, s)) = &shadow {
                    // SAFETY: a live buffer.
                    let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                    let offset = s.gpu(*a) - base;
                    // Raw views start on 16 bytes: from the 16 bytes
                    // below, and the shader adds the rest to its offsets
                    // (`victory_shader::emit::USER_VIEW_BIAS`). (An address
                    // not on 16 bytes got a null view at first: the
                    // builder's counters are at such addresses.)
                    let bias = offset & 15;
                    host.raw_uav_at(index, &s.buffer, offset - bias, s.left(*a).min(limit) + bias);
                }
                if shadow.is_none() {
                    host.null_raw_uav_at(index);
                }
                self.root_uav_slots
                    .insert(key, (index, shadow.as_ref().map(|(_, s)| s.clone())));
                (index, shadow)
            }
        };
        if let Some((a, s)) = &shadow {
            ready(*a, s);
        }
        let handle = host.gpu_handle(index);
        // SAFETY: an open list; the slot is in the bound view heap.
        unsafe {
            if compute {
                list.list.SetComputeRootDescriptorTable(param, handle);
            } else {
                list.list.SetGraphicsRootDescriptorTable(param, handle);
            }
        }
        true
    }

    /// Binds a root SRV as a one-view table
    /// ([`crate::pipeline::host::bounded_root_srvs`]): a raw view of the
    /// title buffer's host copy from `address` to the copy's end, or a null
    /// view (no address, no host copy, or an address not on 16 bytes). One
    /// dynamic slot per address per list, given back with the list's
    /// others. False if no slot could be had or no view heap is bound.
    fn root_srv_table(
        &mut self,
        list: &HostList,
        address: Option<u64>,
        param: u32,
        compute: bool,
        biased: bool,
    ) -> bool {
        let heap = match self.heaps[0] {
            0 => None,
            h => Some(crate::descriptor::heap(h as This)),
        };
        let Some((heap, host)) = heap.and_then(|h| Some((h, h.host()?))) else {
            once!("a root SRV without a bound view heap: the draw is dropped");
            return false;
        };
        // A view that begins early is another view than one that does not
        // (title addresses stay below bit 63).
        let key = (
            std::ptr::from_ref(heap) as usize,
            address.unwrap_or(0) | u64::from(biased) << 63,
        );
        let (index, shadow) = match self.root_srv_slots.get(&key) {
            Some((index, shadow)) => (*index, address.zip(shadow.clone())),
            None => {
                let shadow = address.and_then(|a| crate::shadow::find(a, 1).map(|s| (a, s)));
                let Some(index) = host.dynamic(1) else {
                    return false;
                };
                self.dynamic.push((key.0, index, 1));
                self.root_srv_slots
                    .insert(key, (index, shadow.as_ref().map(|(_, s)| s.clone())));
                match &shadow {
                    Some((a, s)) => {
                        // SAFETY: a live buffer.
                        let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                        let offset = s.gpu(*a) - base;
                        if biased {
                            // From the 16 bytes below; the shader adds the
                            // rest (`victory_shader::emit::USER_VIEW_BIAS`).
                            let bias = offset & 15;
                            host.raw_at(index, &s.buffer, offset - bias, s.left(*a) + bias);
                        } else if offset.is_multiple_of(16) {
                            host.raw_at(index, &s.buffer, offset, s.left(*a));
                        } else {
                            // Raw views start on 16 bytes; one from the
                            // aligned start would shift every read.
                            once!(
                                address = format_args!("{a:#x}"),
                                "a root SRV not on 16 bytes reads zeros"
                            );
                            host.null_raw_at(index);
                        }
                    }
                    None => host.null_raw_at(index),
                }
                (index, shadow)
            }
        };
        // As for root descriptors (`root_view`): all that is left of the
        // buffer is brought up to date. The first 1 MiB only left what lay
        // past it stale (most mesh vertex streams reach further).
        if let Some((a, s)) = &shadow {
            s.refresh(list, *a, s.left(*a));
        }
        let handle = host.gpu_handle(index);
        // SAFETY: an open list; the slot is in the bound view heap.
        unsafe {
            if compute {
                list.list.SetComputeRootDescriptorTable(param, handle);
            } else {
                list.list.SetGraphicsRootDescriptorTable(param, handle);
            }
        }
        true
    }

    fn view_heap(&self) -> Option<&crate::descriptor::Heap> {
        self.heaps.iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())
    }

    fn early_textures(&self, list: &HostList, pipeline: &HostPipeline) {
        let (regular, bake_limit) = early_texture_limits();
        if regular.is_zero() && bake_limit.is_zero() {
            return;
        }
        let regular = if EARLY_TEXTURE_FRAME.load(Ordering::Relaxed) == crate::frame::current() {
            Duration::ZERO
        } else {
            regular
        };
        let bake = if !bake_limit.is_zero()
            && pipeline.stages.iter().any(|stage| BAKE_PIXEL_SHADERS.contains(&stage.shader.hash.as_str()))
        {
            bake_limit
        } else {
            Duration::ZERO
        };
        if regular.is_zero() && bake.is_zero() {
            return;
        }
        if let Some(heap) = self.view_heap() && let Some(host) = heap.host() {
            self.heap_textures(list, heap, host, TexturePass::Early { regular, bake });
        }
    }

    /// For a shader that picks buffers from the view heap by an index it
    /// computes (SM 6.6 dynamic resources): the GPU address of the heap's
    /// table of element strides, with every buffer view of the heap given
    /// its host slot and its memory brought up to date when `list` is
    /// submitted. Tables tell us which views a draw reads; nothing tells
    /// us which of the heap's a computed index names, so all of them are
    /// made ready, once per list.
    /// shortcut: the buffers are read, not written: what such a shader
    /// writes through a picked unordered access view stays in the host
    /// copy. A list refreshes them where it first runs such a shader;
    /// title writes recorded later in the same list are not seen.
    fn heap_buffers(&self, list: &HostList) -> Option<u64> {
        let heap = self.view_heap()?;
        let host = heap.host()?;
        let gpu = host.strides_gpu()?;
        // One key per list and heap, as textures have: no title resource is
        // there (the heap's address with every bit flipped is past user
        // space). A list that binds a second heap runs that heap's pass and
        // keeps its table too.
        const ONCE: usize = usize::MAX;
        if !list.first_use(ONCE ^ std::ptr::from_ref(heap) as usize) {
            return Some(gpu);
        }
        // The list may outlive the title's heap object, which owns the table.
        if let Some(buffer) = host.strides_buffer() {
            list.keep(buffer);
        }
        // The pass itself once per frame: per list it ran up to 51 times a
        // frame over 81,000 views and 4,100 host copies, 18 s of every
        // frame (2026-11-24, run 11). Per heap: the views the pass keeps
        // are the heap's, and a second heap's are not made by the first's
        // pass.
        // shortcut: what the title writes into such buffers between lists
        // of one frame is seen a frame late.
        let frame = crate::frame::current();
        if host.buffers_frame.swap(frame, Ordering::Relaxed) == frame {
            return Some(gpu);
        }
        let started = std::time::Instant::now();
        // Per host heap: the host copy and range each slot's buffer view
        // reads, kept from pass to pass. Only the slots written since the
        // last pass are looked at again (83,000 slots a pass otherwise, 0.1 s
        // at best), with a sixteenth of all of them in turn, because a view
        // of a released resource goes stale without a write, and a slot
        // written before its memory had a host copy is only made then.
        let (slots, mut kept) = host.buffers.begin_pass();
        let ranges = &mut kept.by_slot;
        // Whether the device was there before the pass: a view that removes
        // it is named below.
        static GONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let mut alive = !GONE.load(Ordering::Relaxed) && crate::descriptor::host::device_removed().is_none();
        for (n, &index) in slots.iter().enumerate() {
            ranges.remove(&index);
            let Some(handle) = heap.handle(index) else {
                continue;
            };
            let Some(view) = host.mirror(index, handle, true) else {
                continue;
            };
            // Every 64 views while the device lives (a cheap query).
            if alive
                && n % 64 == 63
                && let Some(why) = crate::descriptor::host::device_removed()
            {
                alive = false;
                GONE.store(true, Ordering::Relaxed);
                let last: Vec<String> = slots[n - 63..=n]
                    .iter()
                    .filter_map(|&i| crate::descriptor::view(heap.handle(i)?).map(|v| format!("{i}: {v:?}")))
                    .map(|s| s.chars().take(200).collect())
                    .collect();
                tracing::error!(why, slot = index, "the host device went while buffer views of the heap were made; one of: {last:#?}");
                crate::host::log_removal_context();
            }
            // The copy the slot shows (`buffer_copy`), not the one the
            // view's range alone would find.
            let Some((shadow, address, len)) = crate::descriptor::host::buffer_copy(&view) else {
                continue;
            };
            #[cfg(feature = "diagnostics")]
            if host.shows(index, &shadow.buffer) == Some(false) {
                HEAP_VIEWS_ELSEWHERE.fetch_add(1, Ordering::Relaxed);
            }
            ranges.insert(index, (shadow, address, address + len));
        }
        let views = ranges.len() as u32;
        HEAP_PHASES[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        let spans_started = std::time::Instant::now();
        // Per host copy, the span its picked views cover.
        let mut spans: std::collections::HashMap<usize, (std::sync::Arc<crate::shadow::Shadow>, u64, u64)> =
            std::collections::HashMap::new();
        for (shadow, from, to) in ranges.values() {
            let key = std::sync::Arc::as_ptr(shadow) as usize;
            let span = spans.entry(key).or_insert_with(|| (shadow.clone(), *from, *to));
            span.1 = span.1.min(*from);
            span.2 = span.2.max(*to);
        }
        drop(kept);
        let bytes: u64 = spans.values().map(|s| s.2 - s.1).sum();
        // Memory that is not write-watched is copied whole by every refresh.
        // It was once refreshed only every 120 frames (unmapped pages read
        // one by one cost 14 GB a frame in run 11, before they were
        // remembered): streamed geometry then drew from stale buffers, and
        // the garbage ended runs (the card stopped answering 14-90 frames
        // into the opening cutscene; with every frame it ran on). The pass
        // costs some 30 ms a frame this way.
        static WHOLE: std::sync::Mutex<Option<std::collections::HashMap<usize, u64>>> = std::sync::Mutex::new(None);
        let mut whole = WHOLE.lock().unwrap_or_else(|e| e.into_inner());
        let whole = whole.get_or_insert_with(Default::default);
        for (shadow, from, to) in spans.values() {
            if shadow.unwatched() {
                // By the copy's number: a copy made where one let go was
                // (`shadow::let_go_idle`) has never been refreshed.
                let key = shadow.id() as usize;
                // `VICTORY_UNWATCHED_FRAMES` sets the interval (1: every frame).
                static EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
                let every = *EVERY.get_or_init(|| {
                    std::env::var("VICTORY_UNWATCHED_FRAMES")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1)
                });
                match whole.get(&key) {
                    Some(&last) if frame < last + every => continue,
                    _ => {
                        whole.insert(key, frame);
                    }
                }
            }
            shadow.refresh(list, *from, to - from);
        }
        HEAP_PHASES[1].fetch_add(spans_started.elapsed().as_micros() as u64, Ordering::Relaxed);
        self.heap_textures(list, heap, host, TexturePass::Indexed);
        HEAP_BUFFER_PASSES.fetch_add(1, Ordering::Relaxed);
        HEAP_BUFFER_MICROS.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        static LOGGED: AtomicU64 = AtomicU64::new(0);
        // The first pass and every growth of the set by half. (After a
        // heap's first pass, `slots` is a pass's share: those written since
        // and a sixteenth of all.)
        let seen = LOGGED.load(Ordering::Relaxed);
        if u64::from(views) > seen + seen / 2 {
            LOGGED.store(u64::from(views), Ordering::Relaxed);
            tracing::info!(
                slots = slots.len(),
                views,
                copies = spans.len(),
                mb = bytes >> 20,
                ms = started.elapsed().as_millis(),
                "buffer views of the heap made ready for shaders that pick them by index"
            );
        }
        Some(gpu)
    }

    /// The queue's side of the card accounting line
    /// (`crate::resource::card_accounting`, which says what the line
    /// holds): the slots that are due, by what their texture has on the
    /// card. Off unless `VICTORY_CARD_ACCOUNTING=1`; then at the first pass
    /// that finds the card without room, and every five seconds after it.
    /// Nothing is made, uploaded or counted as used here. For 18,000 due
    /// slots: 18,000 looks at a title slot and at the texture it views, a
    /// lock each, some milliseconds of the thread that records the frame.
    fn card_accounting(
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
        frame: u64,
        room: bool,
    ) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("VICTORY_CARD_ACCOUNTING").is_some_and(|v| v == "1")) {
            return;
        }
        // When the next line is due (0: none yet, the card has had room).
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let now = crate::host::now_ms().max(1);
        let next = NEXT.load(Ordering::Relaxed);
        let due_now = if next == 0 { !room } else { now >= next };
        if !due_now || NEXT.compare_exchange(next, now + 5000, Ordering::Relaxed, Ordering::Relaxed).is_err() {
            return;
        }
        // Due slots; of them uploaded before; of them viewing nothing the
        // title fills; and the textures (each once) whose host copy is
        // alive, with their bytes on the card.
        let mut due = [0u64; 5];
        // Textures (each once) that never had a host copy, and ones whose
        // host copy was let go: where their memory is, and its bytes.
        let (mut never, mut gone) = (Vec::new(), Vec::new());
        let mut seen = std::collections::BTreeSet::new();
        for (index, uploaded) in host.texture_slots_all_due(frame) {
            due[0] += 1;
            due[1] += u64::from(uploaded);
            let state = match heap.handle(index).and_then(crate::descriptor::view) {
                Some(View::Placed(false, resource, desc)) if resource[0] != 1 => {
                    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                    match crate::resource::placed_known(address, &resource) {
                        Some(texture) => crate::resource::copy_state(texture),
                        // No title texture was made for it: the pass never
                        // got as far as making one.
                        None => Some((0, crate::resource::placed_bytes(&resource).unwrap_or(0), address)),
                    }
                }
                Some(View::Shader(object, _)) if crate::resource::title_fills(object as This) => {
                    crate::resource::copy_state(object as This)
                }
                _ => None,
            };
            let Some((state, bytes, memory)) = state else {
                due[2] += 1;
                continue;
            };
            if !seen.insert(memory) {
                continue;
            }
            match state {
                2 => {
                    due[3] += 1;
                    due[4] += bytes;
                }
                1 => gone.push((memory, bytes)),
                _ => never.push((memory, bytes)),
            }
        }
        crate::resource::card_accounting(room, due, never, gone);
    }

    /// The texture side of [`State::heap_buffers`]: textures the title fills
    /// itself (streamed pages, layout-library copies) whose views sit in the
    /// view heap are created (placed textures have no host texture until
    /// something reads them) and uploaded, as a table read would do, for
    /// shaders that pick them by a computed index. Within a budget of bytes
    /// per frame and in all; the rest waits for later lists and frames.
    /// The most recently written slots first (`TextureQueue::due`); slots
    /// draws read through tables are not in the queue.
    /// A partly mapped texture is uploaded again when more of it is mapped.
    /// Complete uploads also get a limited number of timestamped content
    /// checks for writes that do not update the descriptor.
    /// `VICTORY_HEAP_TEXTURES=0` turns it off.
    fn heap_textures(
        &self,
        list: &HostList,
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
        mode: TexturePass,
    ) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_TEXTURES").is_none_or(|v| v != "0")) {
            return;
        }
        // Both indexed and early passes record uploads and update the same
        // queue. Serialize them even when the early draw does not use buffers.
        static UPLOADS: std::sync::Mutex<TextureUploads> =
            std::sync::Mutex::new(TextureUploads::new(None));
        let mut uploads = UPLOADS.lock().unwrap_or_else(|e| e.into_inner());
        let frame = crate::frame::current();
        if uploads.frame != Some(frame) {
            *uploads = TextureUploads::new(Some(frame));
        }
        let early = matches!(mode, TexturePass::Early { .. });
        let Some((span, bytes_limit, bake)) = uploads.budget(mode) else {
            return;
        };
        let pass_started = std::time::Instant::now();
        let spent_before = uploads.spent;
        if early {
            EARLY_TEXTURE_FRAME.store(frame, Ordering::Relaxed);
            EARLY_TEXTURE_PASSES[usize::from(bake)].fetch_add(1, Ordering::Relaxed);
        }
        // No more of them are made while the card has no room
        // (`resource::room`): by what is alive now, not by what was ever
        // staged (4 GB in all, after which later textures stayed empty for
        // the run). The ones nothing names any more are let go, which
        // makes room again: so the slots are still looked at (the pass
        // returned here, and a slot whose texture had been streamed out
        // kept its host copy for as long as the card was full).
        let room = crate::resource::room();
        if !room {
            HEAP_TEXTURES_FULL.fetch_add(1, Ordering::Relaxed);
            once!("textures picked by index: the card has no room for more; they wait");
        }
        let mut refused = 0;
        // Slots whose texture's memory the runtime mapped, unmapped or
        // wrote since the last pass are due again.
        let remap_started = std::time::Instant::now();
        host.texture_slots_remapped(frame);
        let remap_time = remap_started.elapsed();
        if heap_recheck() {
            // Sample at most one batch per heap and frame. Check time before
            // each sample; unprocessed entries stay due for the next frame.
            const RECHECK_LIMIT: usize = 256;
            const RECHECK_TIME: std::time::Duration = std::time::Duration::from_millis(2);
            let start = std::time::Instant::now();
            let rechecks = host.texture_rechecks_due(frame, start, RECHECK_LIMIT);
            if !rechecks.is_empty() {
                for check in rechecks {
                    if start.elapsed() >= RECHECK_TIME {
                        break;
                    }
                    let changed = crate::texture::fingerprint(check.address, check.bytes) != check.fingerprint;
                    if host.finish_texture_recheck(check, changed, frame, std::time::Instant::now()) {
                        HEAP_TEXTURES_RECHECKED[usize::from(changed)].fetch_add(1, Ordering::Relaxed);
                    }
                }
                HEAP_TEXTURE_RECHECK_US.fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
        }
        let queue_started = std::time::Instant::now();
        let (due, census) = host.texture_slots_due(frame, 4096);
        HEAP_PHASES[2].fetch_add(
            (remap_time + queue_started.elapsed()).as_micros() as u64,
            Ordering::Relaxed,
        );
        let phase = |i: usize, since: std::time::Instant| {
            HEAP_PHASES[i].fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
        };
        HEAP_TEXTURES_WAITING.store(census[0], Ordering::Relaxed);
        for (count, now) in HEAP_TEXTURES_QUEUE.iter().zip(census) {
            count.store(now, Ordering::Relaxed);
        }
        // Where the card's memory is, and what the due slots would need
        // (`VICTORY_CARD_ACCOUNTING=1`; a line when the card first has no
        // room, and every five seconds after it).
        Self::card_accounting(heap, host, frame, room);
        // The frames in about four seconds, between 16 and 256: how long a
        // slot with nothing to upload waits at most for its next look. In
        // frames alone it was 120: four minutes at the opening cutscene's
        // half a frame per second, while the title streams by the clock.
        // (About a second before the runtime told of the memory it maps:
        // the looks are now only for what that does not tell.)
        static PASSED: AtomicU64 = AtomicU64::new(0);
        let now = crate::host::now_ms();
        let took = now.saturating_sub(PASSED.swap(now, Ordering::Relaxed)).max(1);
        let most = (4000 / took).clamp(16, 256);
        // (a texture uploaded in part and found no further along: see
        // `park_frames`)
        let parked = most.max(park_frames());
        // What a table entry of a texture a shader samples says.
        let sampled = UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot: 0,
        };
        // Also within a time per pass: the pass runs on a thread that
        // records the frame, and making and staging 192 MB of textures took
        // 1.6-2.2 s of each of the world's first twenty frames (run 20).
        // The rest waits for the next frames, as it does past the bytes.
        let began = if early { pass_started } else { std::time::Instant::now() };
        for (index, write) in due {
            if uploads.spent >= bytes_limit {
                HEAP_PHASES[10].fetch_add(1, Ordering::Relaxed);
                break;
            }
            if began.elapsed() > span {
                HEAP_PHASES[9].fetch_add(1, Ordering::Relaxed);
                break;
            }
            HEAP_PHASES[7].fetch_add(1, Ordering::Relaxed);
            let turn = std::time::Instant::now();
            // A placed texture whose memory is not mapped (not streamed in
            // yet, or streamed out again: the heap keeps tens of thousands
            // of such views) gets no host texture; looked at again soon (a
            // title writes a texture's view before its pages are mapped,
            // and the pass runs early in a frame). One of which some is
            // mapped (a texture streaming in: its smaller mips lie at the
            // start of its memory and arrive first) is uploaded with what
            // is there, and again when more is. (It was uploaded again
            // every 120 frames, changed or not; and asked by its first
            // page alone, one whose first page is the one missing counted
            // as not there.)
            let mut partly = None;
            let mut upload_bytes = None;
            if let Some(View::Placed(false, resource, desc)) = heap.handle(index).and_then(crate::descriptor::view) {
                let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                upload_bytes = crate::resource::placed_bytes(&resource);
                let len = upload_bytes.unwrap_or(1);
                let mapped = crate::texture::mapped_bytes(address as *const u8, len as usize);
                phase(3, turn);
                // More there than at the last look, with no notice between
                // (a notice resets the looks).
                let (was_mapped, looks) = host.texture_slot_seen(index);
                if mapped > was_mapped && looks > 0 {
                    HEAP_TEXTURES_UNANNOUNCED.fetch_add(1, Ordering::Relaxed);
                }
                if mapped == 0 {
                    HEAP_TEXTURES_AWAY.fetch_add(1, Ordering::Relaxed);
                    // Streamed out: what the slot showed is let go.
                    host.texture_slot_gone(index);
                    host.texture_slot_later(index, write, frame, most);
                    continue;
                }
                if mapped < len {
                    if mapped <= was_mapped {
                        HEAP_TEXTURES_PARTLY[0].fetch_add(1, Ordering::Relaxed);
                        host.texture_slot_later(index, write, frame, parked);
                        continue;
                    }
                    if !crate::texture::committed(address as *const u8, 1) {
                        HEAP_TEXTURES_PARTLY[2].fetch_add(1, Ordering::Relaxed);
                    }
                    partly = Some(mapped);
                }
            }
            // A texture object the host renders to or writes holds the
            // host's own contents: nothing to bring up to date, and this
            // pass is no use of it (its host copy is made when something
            // uses it, and its slots then: `descriptor::make_waiting`).
            if let Some(View::Shader(object, _)) = heap.handle(index).and_then(crate::descriptor::view)
                && crate::resource::buffer_bytes(object as This).is_none()
                && !crate::resource::title_fills(object as This)
            {
                host.texture_slot_done(index, write, None);
                continue;
            }
            // From here on a host texture is made: not without room. The
            // slot stays due.
            // Not so a slot already made from this very view: its host
            // texture exists, and uploading it again takes none of the
            // card's memory. It was refused all the same, so a texture the
            // title filled after its first upload kept its empty host copy
            // from the moment the card reached the line, which is the first
            // world frame in eight launches of nine: the same slots were
            // found changed again at every pass and never uploaded (run
            // 179: 33,000 such findings), and the main character's face
            // had its colour only in the launches that entered the world
            // under the line. `VICTORY_REUPLOAD_WHEN_FULL=0`: refused, as
            // before.
            if !room {
                let has_its_texture = reupload_when_full()
                    && heap.handle(index).is_some_and(|handle| host.texture_slot_made_from(index, handle));
                if !has_its_texture {
                    refused += 1;
                    if refused > 256 {
                        break;
                    }
                    continue;
                }
                let n = HEAP_TEXTURES_REUPLOADED_FULL.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 || n.is_multiple_of(500) {
                    tracing::info!(
                        so_far = n,
                        slot = index,
                        "a texture already on the card is brought up to date although the card has no room for new ones"
                    );
                }
            }
            // Mapping checks and view lookups can use up the rest of this
            // pass. Check again before making and staging a host texture.
            if began.elapsed() > span {
                HEAP_PHASES[9].fetch_add(1, Ordering::Relaxed);
                break;
            }
            // Defer another known-size upload if it exceeds the early
            // budget. Allow the first upload so large textures make progress.
            if early && uploads.spent > spent_before
                && upload_bytes.is_some_and(|bytes| uploads.spent.saturating_add(bytes) > bytes_limit)
            {
                HEAP_PHASES[10].fetch_add(1, Ordering::Relaxed);
                break;
            }
            let made = std::time::Instant::now();
            let view = heap.handle(index).and_then(|handle| host.mirror(index, handle, true));
            let placed = view.as_ref().and_then(crate::descriptor::host::placed_texture);
            phase(4, made);
            let resource = match placed.as_ref().or(view.as_ref()) {
                Some(&View::Shader(resource, _)) => resource as This,
                _ => {
                    host.texture_slot_done(index, write, None);
                    continue;
                }
            };
            let title_fills = crate::resource::host_resource(resource).is_some_and(crate::texture::title_written);
            if !title_fills {
                // a render target, a depth or unordered access texture: the host's own
                host.texture_slot_done(index, write, None);
                continue;
            }
            // A texture that has a staging buffer now is one draws read
            // through a table: they upload it, and check it at every
            // submission that samples it.
            let tabled = crate::texture::is_staged(resource);
            let recorded = std::time::Instant::now();
            if let Some(view) = &view {
                readable(list, sampled.kind, view);
            }
            phase(5, recorded);
            let rest = std::time::Instant::now();
            // A texture view the host refuses removes the device: name it.
            static GONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !GONE.load(Ordering::Relaxed)
                && let Some(why) = crate::descriptor::host::device_removed()
            {
                GONE.store(true, Ordering::Relaxed);
                let text: String = format!("{view:?}").chars().take(300).collect();
                tracing::error!(why, slot = index, "the host device went while textures picked by index were made ready; last: {text}");
                crate::host::log_removal_context();
                break;
            }
            let (bytes, whole) = crate::texture::staged(resource).unwrap_or((0, true));
            // Not all of it there: looked at again next frame, and from
            // then on as a slot with nothing new (a placed texture: until
            // more of it is mapped; a texture object at an address the
            // title mapped: again in 120 frames, as before).
            let again = match partly {
                Some(mapped) => Some((frame + 1, mapped)),
                None => (!whole).then_some((frame + 120, 0)),
            };
            if again.is_some() {
                HEAP_TEXTURES_PARTLY[1].fetch_add(1, Ordering::Relaxed);
            }
            if tabled {
                // Nothing was staged here, so nothing is counted or spent,
                // and the staging buffer stays: taking it away made the
                // next draw that reads the texture through a table stage,
                // detile and upload all of it once more.
                HEAP_TEXTURES_TABLED.fetch_add(1, Ordering::Relaxed);
                host.texture_slot_done(index, write, again);
                continue;
            }
            // Uploaded: the list holds the staging buffer until it has run;
            // a table read, or the next look at a texture not all mapped
            // yet, makes it anew. (Kept for those until now: every partly
            // streamed texture of the heap held a staging buffer of its full
            // size for good.)
            let memory = if again.is_none() && heap_recheck() {
                crate::texture::staged_memory(resource)
            } else {
                None
            };
            crate::texture::unstage(resource);
            uploads.spent = uploads.spent.saturating_add(bytes);
            HEAP_TEXTURE_BYTES.fetch_add(bytes, Ordering::Relaxed);
            HEAP_TEXTURES.fetch_add(1, Ordering::Relaxed);
            HEAP_PHASES[8].fetch_add(1, Ordering::Relaxed);
            match memory {
                Some((at, len)) => {
                    let print = crate::texture::fingerprint(at, len);
                    host.texture_slot_uploaded(index, write, (at, len), print, std::time::Instant::now());
                }
                None => host.texture_slot_done(index, write, again),
            }
            phase(6, rest);
        }
        if bake {
            uploads.bake_spent = uploads.bake_spent.saturating_add(uploads.spent - spent_before);
            uploads.bake_time = uploads.bake_time.saturating_add(began.elapsed());
        }
    }

    /// For the frame listing, after a dropped dispatch's line: its group
    /// counts and the compute root arguments it was recorded with (root
    /// constants as hex dwords, root views with the title buffer they
    /// point into). A program that does not translate has no bindings to
    /// list; these say which memory it would have read and written (the
    /// ray-tracing structure builder passes its buffers this way).
    #[cfg(feature = "diagnostics")]
    fn list_dropped_roots(&self, groups: [u32; 3]) {
        crate::capture::op(0, || {
            use std::fmt::Write;
            let [x, y, z] = groups;
            let mut text = format!("  its roots ({x}x{y}x{z}):");
            for (slot, root) in self.compute.iter().enumerate() {
                match root {
                    Root::None => {}
                    Root::Table(a) => {
                        let _ = write!(text, " {slot}=table {a:#x}");
                    }
                    Root::Constants(v) => {
                        let _ = write!(text, " {slot}=constants {:x?}", &v[..v.len().min(48)]);
                    }
                    Root::View(a) => {
                        let _ = write!(text, " {slot}=view {a:#x}");
                        match crate::shadow::extent(*a) {
                            Some((start, len)) => {
                                let _ = write!(text, " in buffer {start:#x}+{len:#x}");
                            }
                            None => text.push_str(" in no buffer"),
                        }
                    }
                }
            }
            text
        });
    }

    /// Clears the view described by the title's CPU descriptor.
    pub(crate) fn clear_unordered(
        &mut self,
        list: &HostList,
        cpu: u64,
        values: [u32; 4],
        float: bool,
        rects: &[RECT],
    ) {
        let Some(view @ (View::Unordered(..) | View::Placed(true, ..))) =
            crate::descriptor::view(cpu)
        else {
            once!("an unordered access clear without a valid UAV");
            return;
        };
        let heap = (self.heaps[0] != 0).then(|| crate::descriptor::heap(self.heaps[0] as This));
        let (Some(heap), Some(host)) = (heap, crate::host::host()) else {
            once!("an unordered access clear without a bound view heap");
            return;
        };
        let Some(visible) = heap.host() else { return };
        let Some(slot) = visible.dynamic(1) else { return };
        self.bind_heaps(list);
        let cleared = host.with_cpu_view(|cpu| {
            let Some(resource) = visible.copy_unordered_at(slot, &view, cpu) else { return false };
            readable(list, ResourceType::Uav, &view);
            list.keep(&resource);
            let gpu = visible.gpu_handle(slot);
            // SAFETY: an open list and two handles for the same live resource.
            unsafe {
                if float {
                    list.list.ClearUnorderedAccessViewFloat(gpu, cpu, &resource, &values.map(f32::from_bits), rects);
                } else {
                    list.list.ClearUnorderedAccessViewUint(gpu, cpu, &resource, &values, rects);
                }
            }
            true
        });
        if cleared {
            self.dynamic.push((std::ptr::from_ref(heap) as usize, slot, 1));
        } else {
            visible.release(slot, 1);
        }
    }

    /// Runs a dispatch on the host.
    pub(crate) fn dispatch(&mut self, list: &HostList, x: u32, y: u32, z: u32) {
        // It may write the arguments a window holds clamped.
        self.window = None;
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            drop_as(SKIPPED);
            crate::pipeline::note_drop(self.pipeline as This);
            return;
        }
        // The ray-tracing structure builder's stages run all or none.
        if crate::pipeline::is_builder(self.pipeline as This) && !crate::pipeline::builder_runs() {
            drop_as(BUILDER_OFF);
            #[cfg(feature = "diagnostics")]
            {
                crate::pipeline::note_why("a stage of the ray-tracing structure builder, which is off".to_owned());
                self.list_dropped_roots([x, y, z]);
            }
            return;
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            once!(
                pipeline = format_args!("{:#x}", self.pipeline),
                "a dispatch without a host pipeline dropped"
            );
            drop_as(NO_PIPELINE);
            #[cfg(feature = "diagnostics")]
            {
                crate::pipeline::note_drop(self.pipeline as This);
                self.list_dropped_roots([x, y, z]);
            }
            return;
        };
        if !p.compute {
            once!("a dispatch with a graphics pipeline");
            drop_as(OTHER_KIND);
            return;
        }
        if !self.bind(list, p, true) {
            drop_as(BIND);
            // A stage of the structure builder that cannot be bound is a
            // dropped stage: the others stop too.
            self.builder_stage_dropped("one of its dispatches could not be bound");
            return;
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| self.op_text(format!("dispatch {x}x{y}x{z}"), true));
        // SAFETY: an open list with everything set.
        unsafe { list.list.Dispatch(x, y, z) };
        DISPATCHED.fetch_add(1, Ordering::Relaxed);
    }

    /// What a draw or dispatch runs, for the device-removed report
    /// (diagnostics): the pipeline, then per stage the user data it gets
    /// (with the view at that heap index, if one is), the host addresses
    /// of its root views and the views its tables name.
    #[cfg(feature = "diagnostics")]
    fn op_text(&self, kind: String, compute: bool) -> String {
        use std::fmt::Write;
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return kind;
        };
        let mut text = format!("{kind} {}", p.name);
        // What it draws into (a dispatch writes through its views below).
        if !compute {
            let described = |t: usize| {
                crate::resource::host_made(t as This).map(|r| {
                    // SAFETY: a live resource.
                    let d = unsafe { r.GetDesc() };
                    format!("{t:#x} {}x{} f{}", d.Width, d.Height, d.Format.0)
                })
            };
            let targets: Vec<String> = self.targets.iter().filter_map(|&t| described(t)).collect();
            let _ = write!(text, " targets {targets:?} depth {:?} state [{}]", described(self.depth), p.state);
            // The title's scissor and viewport at this draw (the first of
            // each): the title draws what it sees through a
            // portal (a window, from inside a room) under the portal's
            // rectangle, and the listing did not say which draws those are.
            if let Some(r) = self.scissors.first() {
                let _ = write!(text, " scissor [{} {} {} {}]", r.left, r.top, r.right, r.bottom);
            }
            if let Some(v) = self.viewports.first() {
                let _ = write!(
                    text,
                    " viewport [{} {} {} {} depth {}..{}]",
                    v.TopLeftX, v.TopLeftY, v.Width, v.Height, v.MinDepth, v.MaxDepth
                );
            }
        }
        let views = (self.heaps[0] != 0)
            .then(|| crate::descriptor::heap(self.heaps[0] as This).host())
            .flatten();
        for stage in &p.stages {
            let t = stage.translated();
            let _ = write!(text, " | {}:", stage.shader.hash);
            for u in &t.host.user_data {
                let v = match u.root {
                    RootType::Other(kind) => self.host_word(compute, kind, u.slot, u.dword, None),
                    _ => self.user_value(compute, u.slot, u.root, u.dword),
                };
                let _ = write!(text, " u{}={v:#x}", u.index);
                let index = if u.root == RootType::Table { v / 32 } else { v };
                if let Some(slot) = views.and_then(|h| h.describe_slot(index)) {
                    let _ = write!(text, " [heap {index}: {slot}]");
                }
            }
            for &(slot, _) in stage.root_cbvs.iter().chain(&stage.root_srvs).chain(&stage.root_uavs) {
                if let Some(Root::View(address)) = self.roots_ref(compute).get(usize::from(slot)) {
                    let host = crate::shadow::find(*address, 1)
                        .map(|s| (s.gpu(*address), s.left(*address)));
                    let _ = write!(text, " root{slot}={address:#x} host {host:#x?}");
                }
            }
        }
        let _ = write!(text, " tables {:?}", self.reads(compute));
        if !compute && !list_slots().is_empty() {
            text.push_str(&self.slots_text());
        }
        text
    }

    /// For the frame listing, after a draw whose shader `VICTORY_LIST_SLOTS`
    /// names: the heap slots it will pick textures from by index, and what
    /// the title and the host hold there (one `~slot` line each), with the
    /// words they were read from (`~raw` lines, to decode offline what is
    /// not decoded here).
    ///
    /// Where the slots are, for the title's character shaders
    /// (a deferred character pixel shader, from its native code): in the small
    /// buffers and constants of the draw's own table (every word of those
    /// that names a texture view gets a line); in the material record, at
    /// the byte offset the draw's constants (table 1, dword 56) hold at
    /// 0x1c, of the buffer at table 0, dword 160; and in the record at the
    /// byte offset the second root constant holds of the buffer at table 0,
    /// dword 688. A word that is a small number by chance and names a
    /// texture gets a line too.
    #[cfg(feature = "diagnostics")]
    fn slots_text(&self) -> String {
        use std::fmt::Write;
        const MATERIALS: u32 = 160;
        const GLOBAL: u32 = 688;
        const DRAW_CONSTANTS: u32 = 56;
        let wanted = list_slots();
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return String::new();
        };
        let Some(stage) = p.stages.iter().find(|s| wanted.iter().any(|w| s.shader.hash.starts_with(w.as_str())))
        else {
            return String::new();
        };
        let Some(heap) = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())
        else {
            return String::new();
        };
        let t = stage.translated();
        let mut text = String::new();
        let entry = |slot: u8, dwords: u32| -> Option<View> {
            let Some(Root::Table(table)) = self.roots_ref(false).get(usize::from(slot)) else {
                return None;
            };
            crate::descriptor::view(table + u64::from(dwords) * 4)
        };
        let mut material = None;
        for u in t.used.iter().filter(|u| u.root == RootType::Table && u.kind != ResourceType::Sampler) {
            let label = format!("s{}+{}", u.slot, u.table_offset_dwords);
            let Some(view) = entry(u.slot, u.table_offset_dwords) else {
                let _ = write!(text, "\n    ~table {label} no view");
                continue;
            };
            let range = match &view {
                View::Constant(address, size) => Some((*address, u64::from(*size))),
                other => crate::descriptor::host::buffer_range(other),
            };
            match range {
                // A small buffer or constants: its words.
                Some((address, len)) if len <= 2048 => {
                    Self::slot_words(&mut text, heap, &label, address, len as usize);
                    if u.slot == 1 && u.table_offset_dwords == DRAW_CONSTANTS && len >= 0x20 {
                        let mut word = [0u8; 4];
                        if crate::host::read_into((address + 0x1c) as *const u8, &mut word) {
                            material = Some(u32::from_le_bytes(word));
                        }
                    }
                }
                Some(_) => {}
                // A texture read through the table: the title slot it sits in.
                None => {
                    let Some(Root::Table(table)) = self.roots_ref(false).get(usize::from(u.slot)) else {
                        continue;
                    };
                    match heap.index(table + u64::from(u.table_offset_dwords) * 4) {
                        Some(index) => text.push_str(&Self::slot_line(heap, &format!("table:{label}"), 0, index)),
                        None => {
                            let _ = write!(text, "\n    ~table {label} title=[{}]", view_state(Some(&view)));
                        }
                    }
                }
            }
        }
        let uses = |slot: u8, dwords: u32| {
            t.used.iter().any(|u| u.root == RootType::Table && u.slot == slot && u.table_offset_dwords == dwords)
        };
        if let Some(at) = material
            && uses(0, MATERIALS)
            && let Some((base, len)) = entry(0, MATERIALS).as_ref().and_then(crate::descriptor::host::buffer_range)
        {
            let at = u64::from(at);
            let _ = write!(text, "\n    ~material record at {at:#x} of the buffer at {base:#x} bytes {len}");
            if at + 64 <= len {
                Self::slot_words(&mut text, heap, "M", base + at, 64);
            }
            if at + 1936 + 512 <= len {
                Self::slot_words(&mut text, heap, "M+1936", base + at + 1936, 512);
            }
        }
        if uses(0, GLOBAL)
            && let Some(slot) = t.used.iter().find(|u| u.root == RootType::Constants).map(|u| u.slot)
            && let Some(Root::Constants(words)) = self.roots_ref(false).get(usize::from(slot))
            && let Some(&at) = words.get(1)
            && let Some((base, len)) = entry(0, GLOBAL).as_ref().and_then(crate::descriptor::host::buffer_range)
        {
            let at = u64::from(at);
            let _ = write!(text, "\n    ~root constants {words:08x?}: record at {at:#x} of the buffer at {base:#x} bytes {len}");
            if at + 64 <= len {
                Self::slot_words(&mut text, heap, "G", base + at, 64);
            }
        }
        text
    }

    /// Up to 512 bytes of title memory at `address` as hex words (a `~raw`
    /// line), and a `~slot` line for every word of them that names a
    /// texture view in `heap`.
    #[cfg(feature = "diagnostics")]
    fn slot_words(text: &mut String, heap: &crate::descriptor::Heap, label: &str, address: u64, len: usize) {
        use std::fmt::Write;
        let mut bytes = vec![0u8; len.min(512) & !3];
        if bytes.is_empty() || !crate::host::read_into(address as *const u8, &mut bytes) {
            let _ = write!(text, "\n    ~raw {label} at {address:#x} bytes {len}: unreadable");
            return;
        }
        let words: Vec<u32> =
            bytes.as_chunks::<4>().0.iter().map(|w| u32::from_le_bytes(*w)).collect();
        let _ = write!(text, "\n    ~raw {label} at {address:#x} bytes {len}:");
        for w in &words {
            let _ = write!(text, " {w:08x}");
        }
        for (i, &index) in words.iter().enumerate() {
            let names = index != 0
                && heap
                    .handle(index)
                    .and_then(crate::descriptor::view)
                    .as_ref()
                    .is_some_and(crate::descriptor::host::is_texture_view);
            if names {
                text.push_str(&Self::slot_line(heap, label, i * 4, index));
            }
        }
    }

    /// One `~slot` line: where the slot number was read (`from`, byte
    /// `at`), what the title's heap holds in slot `index` with the state
    /// of that texture ([`view_state`]), what the host slot was made from,
    /// and what the queue of textures picked by index knows of it.
    #[cfg(feature = "diagnostics")]
    fn slot_line(heap: &crate::descriptor::Heap, from: &str, at: usize, index: u32) -> String {
        let title = heap.handle(index).and_then(crate::descriptor::view);
        let host: String = heap
            .host()
            .and_then(|h| h.describe_slot(index))
            .unwrap_or_else(|| "none".into())
            .chars()
            .take(90)
            .collect();
        let queue = match heap.host() {
            Some(h) => h.texture_slot_state(index, crate::frame::current()),
            None => "no host heap".into(),
        };
        format!(
            "\n    ~slot from={from}+{at:#x} slot={index} title=[{}] host=[{host}] queue=[{queue}]",
            view_state(title.as_ref())
        )
    }

    /// A mesh draw under `VICTORY_MESH_TEST=scissor` rasterises one pixel:
    /// the scissors before it (`on`), the title's after it.
    fn mesh_scissor(&self, list: &HostList, on: bool) {
        if mesh_test() != 2 {
            return;
        }
        if on {
            let one = RECT { left: 0, top: 0, right: 1, bottom: 1 };
            crate::draw::scissors(list, &vec![one; self.scissors.len().max(1)]);
        } else if !self.scissors.is_empty() {
            crate::draw::scissors(list, &self.scissors);
        }
    }

    /// Runs a mesh dispatch of `x` groups on the host (the title's mesh
    /// programs are one-dimensional).
    pub(crate) fn dispatch_mesh(&mut self, list: &HostList, x: u32) {
        self.window = None;
        #[cfg(feature = "diagnostics")]
        self.trace("mesh", x, 0);
        // (what `prepare_for` does not name: the mesh switches, a list without mesh dispatches)
        PREPARE_WHY.set(MESH_OFF);
        let ran = mesh_test() != 1
            && mesh_kept(self.pipeline)
            && self.prepare_for(list, false, true)
            && match list.list.cast::<ID3D12GraphicsCommandList6>() {
                Ok(l6) => {
                    #[cfg(feature = "diagnostics")]
                    list.note_op(|| self.op_text(format!("mesh {x}"), false));
                    self.mesh_scissor(list, true);
                    // SAFETY: an open list with everything set.
                    unsafe { l6.DispatchMesh(x, 1, 1) };
                    self.mesh_scissor(list, false);
                    DRAWN.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(_) => {
                    PREPARE_WHY.set(NO_MESH_LIST);
                    false
                }
            };
        if !ran {
            drop_as(PREPARE_WHY.get());
            #[cfg(feature = "diagnostics")]
            dropped(self.pipeline);
        }
    }

    /// The walls trace (`crate::host::walls_trace`): one log line per
    /// frame with the draws of its main pass (four or more colour targets)
    /// by scissor rectangle, and how often each of the title's culling
    /// programs was dispatched. The title clips what it draws
    /// through a window or door to that opening's rectangle while it holds
    /// the camera to be in a room (run 182, frame 1529: 1,386 draws under
    /// `531 0 1789 1337`); in a launch without the room's walls the same
    /// shot has no such rectangle (run 187, frame 1414). The line shows
    /// the frame in which that changes.
    fn walls_census(&self, p: &HostPipeline, compute: bool) {
        const CULL: [(&str, &str); 9] = [
            ("55679a0547649d0b", "tree"),
            ("89d08e561897d4e5", "tree_imposter"),
            ("9209777e8face9dd", "grass"),
            ("17af15c4015cb4b6", "prop"),
            ("fdd56d97a806bcca", "prop_nocull"),
            ("42f717f1b4d18e94", "simple"),
            ("1c4b3c0547e3e6fa", "slod"),
            ("9146291b38ea9ce1", "meshlet_args"),
            ("42683eebec081520", "depth_pyramid_cpu_occlusion"),
        ];
        type Census = (u64, std::collections::BTreeMap<[i32; 4], u32>, [u32; 9]);
        static CENSUS: std::sync::Mutex<Census> =
            std::sync::Mutex::new((u64::MAX, std::collections::BTreeMap::new(), [0; 9]));
        let frame = crate::frame::current();
        let mut census = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
        if census.0 != frame {
            let draws: u32 = census.1.values().sum();
            if draws > 0 {
                let mut rects: Vec<([i32; 4], u32)> = census.1.iter().map(|(r, n)| (*r, *n)).collect();
                rects.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.1));
                let scissors: Vec<String> = rects
                    .iter()
                    .take(5)
                    .map(|(r, n)| format!("{} {} {} {} x{n}", r[0], r[1], r[2], r[3]))
                    .collect();
                let counts = census.2;
                let culling: Vec<String> = CULL
                    .iter()
                    .zip(counts)
                    .filter(|(_, n)| *n > 0)
                    .map(|(c, n)| format!("{}={n}", c.1))
                    .collect();
                tracing::info!(
                    frame = census.0,
                    main_pass_draws = draws,
                    rectangles = rects.len(),
                    scissors = ?scissors,
                    culling = ?culling,
                    "walls trace: the frame's main pass by scissor rectangle, and its culling dispatches"
                );
            }
            census.0 = frame;
            census.1.clear();
            census.2 = [0; 9];
        }
        if compute {
            for (i, c) in CULL.iter().enumerate() {
                if p.stages.iter().any(|s| s.shader.hash.as_str() == c.0) {
                    census.2[i] += 1;
                }
            }
        } else if self.targets.iter().filter(|&&t| t != 0).count() >= 4 {
            let r = self.scissors.first().map_or([0; 4], |r| [r.left, r.top, r.right, r.bottom]);
            *census.1.entry(r).or_insert(0) += 1;
        }
    }

    fn roots_ref(&self, compute: bool) -> &Vec<Root> {
        if compute {
            &self.compute
        } else {
            &self.graphics
        }
    }

    fn table_entry(&self, list: &HostList, u: &UsedResource, compute: bool) {
        let Some(Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
            return;
        };
        let handle = table + u64::from(u.table_offset_dwords) * 4;
        let Some(heap) = self.heap_of(handle) else {
            return;
        };
        let (Some(host), Some(index)) = (heap.host(), heap.index(handle)) else {
            return;
        };
        if let Some(view) = host.mirror(index, handle, true) {
            readable(list, u.kind, &view);
            // Up to date by this draw: not the texture pass's to make.
            if crate::descriptor::host::is_texture_view(&view) {
                host.texture_slot_tabled(index, true);
            }
        }
    }

    /// The descriptors a shader reads through tables. Tables holding a
    /// constant buffer are made afresh for the draw in dynamic slots, with
    /// the constant data copied to upload memory when the list is
    /// submitted: the host wants constant buffers 256-byte aligned, the
    /// console only 16. Returns those tables' new byte offsets by root
    /// parameter; the other tables' slots are made from the title's.
    ///
    /// Within a list, the same table holding the same descriptors is made
    /// once: its constant data is copied at submission either way, and the
    /// big tables the world binds would otherwise take hundreds of slots per
    /// draw, wrapping the dynamic slots onto lists not yet submitted.
    pub(crate) fn tables(
        &mut self,
        list: &HostList,
        used: &[UsedResource],
        indexed: &[RootIndexedView],
        compute: bool,
    ) -> Vec<(u8, u32)> {
        let tables = |u: &&UsedResource| u.root == RootType::Table;
        let mut fresh: Vec<u8> = used
            .iter()
            .filter(tables)
            .filter(|u| u.kind == ResourceType::Cbv)
            .map(|u| u.slot)
            .collect();
        // Tables reaching past a host heap smaller than the title's (the
        // host's limit, 1,000,000 views on NVIDIA) are made afresh too: the
        // title's slots there have no host slot of their own.
        for u in used.iter().filter(tables) {
            if fresh.contains(&u.slot) {
                continue;
            }
            let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
                continue;
            };
            let end = table + u64::from(u.table_offset_dwords) * 4;
            let Some(heap) = self.heap_of(table) else {
                continue;
            };
            let (Some(host), Some(index)) = (heap.host(), heap.index(end)) else {
                continue;
            };
            if index >= host.count() {
                fresh.push(u.slot);
            }
        }
        fresh.sort_unstable();
        fresh.dedup();
        for u in used.iter().filter(tables) {
            if !fresh.contains(&u.slot) {
                self.table_entry(list, u, compute);
            }
        }
        let mut made = Vec::new();
        for slot in fresh {
            let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(slot)) else {
                continue;
            };
            let entries: Vec<&UsedResource> = used
                .iter()
                .filter(tables)
                .filter(|u| u.slot == slot)
                .collect();
            let Some(heap) = self.heap_of(table) else {
                continue;
            };
            if heap.samplers() || (table - heap.start) % 32 != 0 {
                continue;
            }
            let Some(host) = heap.host() else {
                continue;
            };
            let mut selected: Vec<u32> = indexed
                .iter()
                .filter(|index| index.table_slot == slot)
                .map(|index| {
                    let constant = self.user_value(
                        compute,
                        index.constant_slot,
                        RootType::Constants,
                        index.constant_dword,
                    );
                    // The shader adds i32 byte offsets and shifts by 5.
                    index.byte_offset.wrapping_add(constant << index.shift) / 32
                })
                .collect();
            selected.sort_unstable();
            selected.dedup();
            let n = entries
                .iter()
                .map(|u| u.table_offset_dwords / 8)
                .chain(selected.last().copied())
                .max()
                .unwrap_or(0)
                + 1;
            let last = table.checked_add(u64::from(n - 1) * 32);
            if n > crate::descriptor::host::DYNAMIC
                || last.and_then(|handle| heap.index(handle)).is_none()
            {
                once!(slot, n, "fresh descriptor table exceeds the available span");
                continue;
            }
            // The table and what its entries hold (record words).
            let key = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                for k in 0..n {
                    let at = table + u64::from(k) * 32;
                    // SAFETY: a descriptor slot in title memory.
                    unsafe { (at as *const [u32; 3]).read_unaligned() }.hash(&mut h);
                }
                (table, n, h.finish())
            };
            if let Some(&first) = self.fresh_tables.get(&key) {
                for u in entries.iter().filter(|u| u.kind != ResourceType::Cbv) {
                    let handle = table + u64::from(u.table_offset_dwords) * 4;
                    if let Some(view) = crate::descriptor::view(handle) {
                        readable(list, u.kind, &view);
                    }
                }
                for k in selected {
                    if entries.iter().any(|u| u.table_offset_dwords / 8 == k) {
                        continue;
                    }
                    let handle = table + u64::from(k) * 32;
                    if let Some(view) = crate::descriptor::view(handle)
                        && let Some(kind) = view_kind(&view)
                    {
                        readable(list, kind, &view);
                    }
                }
                made.push((slot, first * 32));
                continue;
            }
            let Some(first) = host.dynamic(n) else {
                // No room: the draw must not run with the title's own
                // offsets (slots never made for it); `bind` drops it.
                made.push((slot, u32::MAX));
                continue;
            };
            self.fresh_tables.insert(key, first);
            self.dynamic
                .push((std::ptr::from_ref(heap) as usize, first, n));
            // The title's own slots of this table: what the draw reads of
            // them it brings up to date here, in the dynamic slots.
            let title_first = heap.index(table).filter(|first| first + n <= host.count());
            // The whole range: a shader may index past the entries its
            // used-resource list names (descriptor arrays).
            for k in 0..n {
                let index = first + k;
                let used = entries.iter().find(|u| u.table_offset_dwords / 8 == k);
                match crate::descriptor::view(table + u64::from(k) * 32) {
                    Some(View::Constant(address, size)) => {
                        let size = crate::descriptor::constant_staging_bytes(size);
                        // Mirror `root_view`: when the constant buffer lives in
                        // an existing shadow copy at a 256-byte-aligned host GPU
                        // address and fits within D3D12's 64 KiB CBV limit, bind
                        // the shadow directly instead of allocating upload
                        // scratch and emitting a `Step::Fill` that stalls on
                        // in-flight shader writes.
                        let aligned_size = size.next_multiple_of(256);
                        if aligned_size > 0
                            && let Some(shadow) =
                                crate::shadow::find(address, u64::from(aligned_size))
                            && shadow.gpu(address).is_multiple_of(256)
                            && shadow.left(address) >= u64::from(aligned_size)
                        {
                            shadow.refresh(list, address, u64::from(aligned_size));
                            host.constant_at(index, shadow.gpu(address), size);
                        } else {
                            let Some((gpu, to)) = list.scratch(u64::from(size)) else {
                                host.make_null(index);
                                continue;
                            };
                            list.defer_fill(to, address as *const u8, size as usize);
                            host.constant_at(index, gpu, size);
                        }
                    }
                    Some(view) => {
                        host.make_at(index, &view);
                        let kind = used.map(|u| u.kind).or_else(|| {
                            selected.binary_search(&k).ok().and_then(|_| view_kind(&view))
                        });
                        if let Some(kind) = kind {
                            readable(list, kind, &view);
                        }
                        // The title's slot is a table's: out of the queue
                        // of textures picked by index if the draw read it
                        // (its texture is up to date now), and made in
                        // place as the texture pass would have made it,
                        // for a shader that picks it by index all the same.
                        if let Some(title) = title_first
                            && crate::descriptor::host::is_texture_view(&view)
                        {
                            host.texture_slot_tabled(title + k, kind.is_some());
                            if kind.is_some() {
                                host.mirror(title + k, table + u64::from(k) * 32, true);
                            }
                        }
                    }
                    None => {
                        host.make_null(index);
                        if used.is_some() {
                            once!(slot, "a table entry without a view");
                        }
                    }
                }
            }
            made.push((slot, first * 32));
        }
        made
    }

    /// The first draws, at debug level: what they bind and read
    /// (diagnostics builds).
    #[cfg(feature = "diagnostics")]
    fn trace(&self, kind: &str, count: u32, first: u32) {
        static TRACED: AtomicU64 = AtomicU64::new(0);
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let listed = crate::capture::listed(crate::queue::presented() + 1);
        let logged = listed || TRACED.fetch_add(1, Ordering::Relaxed) < 64;
        let targets: Vec<String> = self
            .targets
            .iter()
            .filter_map(|&t| crate::resource::host_made(t as This))
            .map(|t| {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                format!("{}x{} f{}", d.Width, d.Height, d.Format.0)
            })
            .collect();
        let v = self.vertex[0];
        let line = format!(
            "{kind} {count} from {first} topology {} pipeline {:#x} targets {targets:?} vb {:#x}+{} stride {} reads {:?}",
            self.topology,
            self.pipeline,
            v.address,
            v.size,
            v.stride,
            self.reads(false)
        );
        if logged {
            tracing::debug!("draw {line}");
        }
        let mut frame = FRAME.lock().unwrap_or_else(|e| e.into_inner());
        if frame.len() < 256 {
            frame.push(line);
        }
    }

    /// What a draw's shaders read, one line per used resource (diagnostics
    /// builds).
    #[cfg(feature = "diagnostics")]
    fn reads(&self, compute: bool) -> Vec<String> {
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return vec!["no host pipeline".into()];
        };
        let mut out = Vec::new();
        for stage in &p.stages {
            let t = stage.translated();
            for u in &t.used {
                let what = match (u.root, self.roots_ref(compute).get(usize::from(u.slot))) {
                    (RootType::Table, Some(Root::Table(table))) => {
                        let handle = table + u64::from(u.table_offset_dwords) * 4;
                        describe(crate::descriptor::view(handle).as_ref())
                    }
                    (root, _) => format!("{root:?}"),
                };
                out.push(format!(
                    "{} {:?} s{}+{}: {what}",
                    stage.shader.hash, u.kind, u.slot, u.table_offset_dwords
                ));
            }
        }
        out
    }

    pub(crate) fn draw(
        &mut self,
        list: &HostList,
        vertices: u32,
        instances: u32,
        first: u32,
        first_instance: u32,
    ) {
        self.window = None;
        #[cfg(feature = "diagnostics")]
        self.trace("draw", vertices, first);
        let ready = self.prepare(list, false);
        // A pipeline with the rectangle geometry stage draws the title's
        // vertices as they are; any other gets its rectangles expanded.
        let rects = self.topology == RECTANGLES && !self.completes_rects();
        let ran = ready && (!rects || self.rectangles(list, vertices, first));
        if ran {
            // Rectangles were expanded into their own vertex buffers.
            let (count, from) = if rects {
                (vertices / 3 * 6, 0)
            } else {
                (vertices, first)
            };
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("draw {count}x{instances}"), false));
            // SAFETY: an open list with everything set.
            unsafe {
                list.list
                    .DrawInstanced(count, instances, from, first_instance)
            };
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        if !ran {
            drop_as(if ready { RECTS } else { PREPARE_WHY.get() });
            #[cfg(feature = "diagnostics")]
            dropped(self.pipeline);
        }
    }

    /// Whether the bound pipeline completes rectangles itself, in the
    /// geometry stage rectangle pipelines get (`HostPipeline::rects`).
    fn completes_rects(&self) -> bool {
        crate::pipeline::host_pipeline(self.pipeline as This).is_some_and(|p| p.rects)
    }

    /// Binds vertex buffers holding the rectangles of vertices `first..first
    /// + count` as triangles, made when the list is submitted.
    fn rectangles(&self, list: &HostList, count: u32, first: u32) -> bool {
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return false;
        };
        let rects = u64::from(count / 3);
        let slots: Vec<(u32, Vec<(u32, u32)>)> = (0..32)
            .map(|slot| {
                let elements = p
                    .inputs
                    .iter()
                    .filter(|e| e.slot == slot && !e.per_instance)
                    .map(|e| (e.offset, e.format))
                    .collect();
                (slot, elements)
            })
            .filter(|(_, elements): &(u32, Vec<(u32, u32)>)| !elements.is_empty())
            .collect();
        // Where each slot's vertices are read: checked for every slot before
        // any expansion is recorded.
        let mut reads = Vec::with_capacity(slots.len());
        for (slot, _) in &slots {
            let v = self.vertex[*slot as usize];
            let stride = u64::from(v.stride);
            let from = v.address + u64::from(first) * stride;
            if v.address == 0
                || stride == 0
                || u64::from(first + count) * stride > u64::from(v.size)
                || crate::shadow::find(from, rects * 3 * stride).is_none()
            {
                once!(slot = *slot, "rectangle vertices outside their buffer");
                return false;
            }
            reads.push((from as usize, stride as usize));
        }
        // The right angles come from the slot holding the position, the
        // first one with a float element of two or more components, for every
        // slot alike: a slot of colours alone would find none of its own.
        let position = slots
            .iter()
            .zip(&reads)
            .find(|(slot, read)| crate::primitive::has_position(&slot.1, read.1))
            .map(|(slot, read)| (read.0, read.1, slot.1.clone()));
        for ((slot, elements), &(from, stride)) in slots.into_iter().zip(&reads) {
            let v = self.vertex[slot as usize];
            let stride = stride as u64;
            let Some((gpu, to)) = list.scratch(rects * 6 * stride) else {
                return false;
            };
            let (to, stride) = (to as usize, stride as usize);
            let n = rects as usize;
            let position = position.clone();
            list.defer_host(move || {
                // The vertices are read in place, at submission: no change
                // of mapping under the read (see
                // `crate::watch::title_memory_reading`).
                let _mapped = crate::watch::title_memory_reading();
                // SAFETY: the vertices lie in a title buffer (checked when
                // recorded) and the scratch memory is the list's.
                let (src, dst) = unsafe {
                    (
                        std::slice::from_raw_parts(from as *const u8, n * 3 * stride),
                        std::slice::from_raw_parts_mut(to as *mut u8, n * 6 * stride),
                    )
                };
                let rights = match &position {
                    Some((at, pstride, pelements)) => {
                        // SAFETY: the position slot's vertices lie in a title
                        // buffer too, checked with the others when recorded.
                        let positions =
                            unsafe { std::slice::from_raw_parts(*at as *const u8, n * 3 * pstride) };
                        crate::primitive::right_angles(positions, *pstride, pelements)
                    }
                    None => Vec::new(),
                };
                crate::primitive::expand_rects_at(src, dst, stride, &elements, &rights);
            });
            let view = D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: gpu,
                SizeInBytes: (rects * 6 * stride as u64) as u32,
                StrideInBytes: v.stride,
            };
            // SAFETY: an open list; the view names live scratch memory.
            unsafe { list.list.IASetVertexBuffers(slot, Some(&[view])) };
        }
        true
    }

    pub(crate) fn draw_indexed(
        &mut self,
        list: &HostList,
        indices: u32,
        instances: u32,
        first: u32,
        base: i32,
        first_instance: u32,
    ) {
        self.window = None;
        #[cfg(feature = "diagnostics")]
        self.trace("indexed", indices, first);
        // Indexed rectangles are drawn by the rectangle geometry stage
        // alone (nothing expands them on the CPU). Without a host pipeline
        // the draw is lost to that instead (`prepare` says so).
        let lost = self.topology == RECTANGLES
            && crate::pipeline::host_pipeline(self.pipeline as This).is_some()
            && !self.completes_rects();
        if lost {
            once!("indexed rectangles not drawn");
        }
        let ran = !lost && self.prepare(list, true);
        if ran {
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("indexed {indices}x{instances}"), false));
            // SAFETY: an open list with everything set.
            unsafe {
                list.list
                    .DrawIndexedInstanced(indices, instances, first, base, first_instance)
            };
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        if !ran {
            drop_as(if lost { RECTS } else { PREPARE_WHY.get() });
            #[cfg(feature = "diagnostics")]
            dropped(self.pipeline);
        }
    }
}

impl State {
    /// Indirect execution of a one-argument signature of `kind` (PC
    /// numbering: 0 draw, 1 indexed draw, 2 dispatch, 10 mesh dispatch) with
    /// records `stride` bytes apart at title address `arguments`, and the
    /// record count at `count` if the title gives one: the GPU reads them
    /// from the buffer's host copy, where earlier dispatches may have
    /// written them. False if it cannot run on the host.
    pub(crate) fn execute_indirect(
        &mut self,
        list: &HostList,
        signature: (u32, u32, u32),
        arguments: u64,
        count: Option<u64>,
    ) -> bool {
        // The ray-tracing structure builder's stages run all or none.
        let builder = crate::pipeline::is_builder(self.pipeline as This);
        if builder && !crate::pipeline::builder_runs() {
            #[cfg(feature = "diagnostics")]
            crate::pipeline::note_why("a stage of the ray-tracing structure builder, which is off".to_owned());
            return false;
        }
        let ran = self.run_indirect(list, signature, arguments, count);
        if builder && !ran {
            self.builder_stage_dropped("one of its indirect dispatches could not run");
        }
        ran
    }

    /// A stage of the structure builder is dropped after all (the title
    /// gave it something the host cannot run): it would leave the next
    /// stages garbage to compute their stores from, so none runs from here
    /// on ([`crate::pipeline::builder_off`]).
    pub(crate) fn builder_stage_dropped(&self, why: &'static str) {
        if crate::pipeline::is_builder(self.pipeline as This) {
            crate::pipeline::builder_off(why);
        }
    }

    /// [`Self::execute_indirect`] for a pipeline allowed to run.
    fn run_indirect(
        &mut self,
        list: &HostList,
        (kind, stride, max): (u32, u32, u32),
        arguments: u64,
        count: Option<u64>,
    ) -> bool {
        // Skip before the argument clamp can mutate the indirect buffer.
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            crate::pipeline::note_drop(self.pipeline as This);
            return false;
        }
        const DRAW: u32 = 0;
        const INDEXED: u32 = 1;
        const DISPATCH: u32 = 2;
        const MESH: u32 = 10;
        #[cfg(feature = "diagnostics")]
        let why = |what: &str| crate::pipeline::note_why(format!("indirect kind {kind}: {what}"));
        #[cfg(not(feature = "diagnostics"))]
        let why = |_: &str| {};
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            #[cfg(feature = "diagnostics")]
            crate::pipeline::note_drop(self.pipeline as This);
            return false;
        };
        // Indirect rectangles (the title's tiled lighting, 256
        // indirect draws a frame, makes their corners from the vertex
        // number) are completed by the pipeline's geometry stage: the
        // arguments stay the title's, three vertices a rectangle. They
        // cannot be expanded on the CPU, which does not know their count.
        // shortcut: in a pipeline without that stage, a rectangle with no
        // per-vertex inputs is drawn as its first triangle (three vertices,
        // half of it), and one with vertex buffers is not drawn.
        let made_rects = !p.rects
            && kind == DRAW
            && self.topology == RECTANGLES
            && !p.inputs.iter().any(|e| !e.per_instance);
        if made_rects {
            once!("indirect rectangles without vertex inputs drawn as their first triangle");
        }
        let matches_kind = match kind {
            DRAW | INDEXED => {
                (self.topology != RECTANGLES || made_rects || p.rects) && !p.compute && !p.mesh
            }
            MESH => !p.compute && p.mesh,
            DISPATCH => p.compute,
            _ => false,
        };
        if !matches_kind {
            why(&format!(
                "pipeline mismatch (compute {}, mesh {}, topology {})",
                p.compute, p.mesh, self.topology
            ));
            return false;
        }
        let len = u64::from(stride) * u64::from(max.max(1));
        let Some(args) = crate::shadow::find_any(arguments, len) else {
            why("arguments outside title buffers");
            return false;
        };
        let counter = match count {
            Some(at) => match crate::shadow::find_any(at, 4) {
                Some(s) => Some((s, at)),
                None => {
                    why("count outside title buffers");
                    return false;
                }
            },
            None => None,
        };
        let Some(signature) = matches!(kind, DRAW | INDEXED | DISPATCH | MESH)
            .then(|| host_signature(kind, stride))
            .flatten()
        else {
            why("no host signature");
            return false;
        };
        if kind == MESH && (mesh_test() == 1 || !mesh_kept(self.pipeline)) {
            why("mesh draws are switched off (VICTORY_MESH_TEST=skip)");
            return false;
        }
        // A window of records clamped in one pass (`windows`): for single
        // records without a count buffer, drawn by shaders that store to no
        // buffer. Anything else ends the window: it uses the scratch buffer
        // or may write the arguments.
        const WINDOW_USE: usize = usize::MAX - 0x77;
        let offset = args.offset(arguments);
        let windowed = windows()
            && kind != DISPATCH
            && max == 1
            && count.is_none()
            && !p.stages.iter().any(|stage| stage.translated().stores);
        // Another host list (a new one after a cut, or this one after its
        // reset) has another scratch buffer, or this one's with old bytes.
        if !windowed || list.first_use(WINDOW_USE) {
            self.window = None;
        }
        let same = |w: &Window| w.copy == args.id() && w.kind == kind && w.stride == stride;
        let hit = self
            .window
            .as_ref()
            .filter(|w| same(w) && offset >= w.base)
            .filter(|w| (offset - w.base) % u64::from(w.step) == 0 && (offset - w.base) / u64::from(w.step) < u64::from(w.records))
            .map(|w| (w.scratch.clone(), offset - w.base));
        let (bounded, at) = match hit {
            Some(hit) => hit,
            None => {
                // A record a short step after the last window's last one
                // (the same step as that window's, if it had one): the
                // title walks the buffer, so the ones that step apart
                // after it are clamped with it.
                let (step, records) = match &self.window {
                    Some(w) if windowed && same(w) => {
                        let last = w.base + u64::from(w.records - 1) * u64::from(w.step);
                        let step = offset.saturating_sub(last);
                        let walks = step >= u64::from(stride)
                            && step <= 256
                            && step % 4 == 0
                            && (w.records == 1 || step == u64::from(w.step));
                        if walks {
                            let fit = (args.left(arguments) / step).min(WINDOW_MOST) as u32;
                            (step as u32, (if w.records == 1 { 16 } else { w.records * 2 }).min(fit).max(1))
                        } else {
                            (stride, max)
                        }
                    }
                    _ => (stride, max),
                };
                args.refresh(list, arguments, len.max(u64::from(step) * u64::from(records)));
                if let Some((s, at)) = &counter {
                    s.refresh(list, *at, 4);
                }
                // Bounded first: the clamp binds a compute pipeline of its
                // own and copies clamped active records into a scratch
                // buffer at offset 0.
                let count_arg = counter.as_ref().map(|(s, at)| (&s.buffer, s.offset(*at)));
                // More than the record asked for: a window clamped ahead,
                // whose unfilled slots must not spend the list's budget.
                let bounded = if windowed && records > 1 {
                    crate::indirect::bound_ahead(list, (&args.buffer, offset), (kind, step, records))
                } else {
                    crate::indirect::bound(list, (&args.buffer, offset), count_arg, (kind, step, records))
                };
                let Some(bounded) = bounded else {
                    self.window = None;
                    why("not bounded (no clamp, or arguments not 4-byte aligned)");
                    return false;
                };
                self.host_pipeline = 0;
                self.host_roots[1] = 0;
                self.window = windowed.then(|| Window {
                    copy: args.id(),
                    kind,
                    stride,
                    step,
                    base: offset,
                    records,
                    scratch: bounded.clone(),
                });
                (bounded, 0)
            }
        };
        let ran = match kind {
            DRAW | INDEXED => self.prepare(list, kind == INDEXED),
            MESH => self.prepare_for(list, false, true),
            DISPATCH => self.bind(list, p, true),
            _ => false,
        };
        if !ran {
            why("prepare failed");
            return false;
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| {
            self.op_text(format!("indirect kind {kind} max {max} at {arguments:#x}"), kind == DISPATCH)
        });
        if kind == MESH {
            self.mesh_scissor(list, true);
        }
        // SAFETY: an open list with everything set; the scratch buffer and
        // host copies are kept alive by the list, and the signature is cached.
        unsafe {
            list.list.ExecuteIndirect(
                &signature,
                max,
                &bounded,
                at,
                counter.as_ref().map(|(s, _)| &s.buffer),
                counter.as_ref().map_or(0, |(s, at)| s.offset(*at)),
            )
        };
        if kind == MESH {
            self.mesh_scissor(list, false);
        }
        // The scratch buffer stays readable as arguments: the next draw of
        // the window reads it with no barrier, and the next clamp moves it
        // back (`crate::indirect`).
        if kind == DISPATCH {
            DISPATCHED.fetch_add(1, Ordering::Relaxed);
        } else {
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        true
    }
}

/// The host command signature for one argument of `kind` with records
/// `stride` bytes apart (no root arguments, so no root signature), made
/// once.
fn host_signature(kind: u32, stride: u32) -> Option<ID3D12CommandSignature> {
    static MADE: std::sync::Mutex<Vec<((u32, u32), ID3D12CommandSignature)>> =
        std::sync::Mutex::new(Vec::new());
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, s)) = made.iter().find(|(k, _)| *k == (kind, stride)) {
        return Some(s.clone());
    }
    let argument = D3D12_INDIRECT_ARGUMENT_DESC {
        Type: D3D12_INDIRECT_ARGUMENT_TYPE(kind as i32),
        ..Default::default()
    };
    let desc = D3D12_COMMAND_SIGNATURE_DESC {
        ByteStride: stride,
        NumArgumentDescs: 1,
        pArgumentDescs: &argument,
        NodeMask: 0,
    };
    let mut signature: Option<ID3D12CommandSignature> = None;
    // SAFETY: a description of one argument; no root arguments.
    let result = unsafe {
        crate::host::host()?.device.CreateCommandSignature(
            &desc,
            None::<&ID3D12RootSignature>,
            &mut signature,
        )
    };
    if let Err(e) = result {
        once!(error = %e, kind, stride, "host command signature refused");
        return None;
    }
    let signature = signature?;
    made.push(((kind, stride), signature.clone()));
    Some(signature)
}

/// Gets what a shader reads or writes through a view ready: buffer data
/// copied, textures moved to the state they need.
fn view_kind(view: &View) -> Option<ResourceType> {
    match view {
        View::Shader(..) | View::Placed(false, ..) => Some(ResourceType::Srv),
        View::Unordered(..) | View::Placed(true, ..) => Some(ResourceType::Uav),
        _ => None,
    }
}

/// The host state a depth view needs of its texture: the read state if the
/// view writes no plane the format has (the title's read-only flags), else
/// the write state.
/// shortcut: whole textures have one state, so a view that writes one of
/// two planes counts as writing.
fn depth_state(texture: &ID3D12Resource, desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>) -> u32 {
    // D16_UNORM and D32_FLOAT: depth formats without a stencil plane.
    const ONE_PLANE: [i32; 2] = [55, 40];
    let Some(desc) = desc else {
        return DEPTH_WRITE;
    };
    let format = if desc.Format.0 == 0 {
        // SAFETY: a live texture; a view without a format has its own.
        unsafe { texture.GetDesc() }.Format.0
    } else {
        desc.Format.0
    };
    let mut needed = D3D12_DSV_FLAG_READ_ONLY_DEPTH.0;
    if !ONE_PLANE.contains(&format) {
        needed |= D3D12_DSV_FLAG_READ_ONLY_STENCIL.0;
    }
    if desc.Flags.0 & needed == needed {
        DEPTH_READ
    } else {
        DEPTH_WRITE
    }
}

fn readable(list: &HostList, kind: ResourceType, view: &View) {
    const UNORDERED_ACCESS: u32 = 0x8;
    let placed = crate::descriptor::host::placed_texture(view);
    let view = placed.as_ref().unwrap_or(view);
    // Buffer views read title memory: their range is copied at submission;
    // what shaders write through unordered access goes back to it after.
    if let Some((shadow, address, len)) = crate::descriptor::host::buffer_copy(view) {
        shadow.refresh(list, address, len);
        if kind == ResourceType::Uav {
            // A shader that cannot store to buffers leaves the range as
            // the title has it: nothing to find afterwards.
            if STORES.get() {
                shadow.gpu_writes(list, address, len);
            } else {
                #[cfg(feature = "diagnostics")]
                {
                    crate::copystat::add(crate::copystat::C::GpuWritesLeftOut, 1);
                    crate::copystat::add(crate::copystat::C::GpuWritesLeftOutBytes, len);
                }
            }
        }
    }
    match (view, kind) {
        (View::Shader(resource, _), ResourceType::Srv) => {
            // A view the title placed, in another format, in the memory of
            // a texture the host writes: that texture's texels, copied on
            // the host (title memory does not have them).
            let aliased = crate::resource::alias_of(*resource as This)
                .is_some_and(|source| crate::texture::alias(list, *resource as This, source));
            if !aliased
                && let Some(t) = crate::resource::host_resource(*resource as This)
                && crate::texture::title_written(t)
            {
                crate::texture::upload(list, *resource as This, t);
            }
            // A depth texture shaders also write: what they wrote, first.
            list.twin_use(*resource as This, false, false);
            // The depth target the draw only tests is read in both ways at
            // once: both read states, which its preparation asks for too.
            let (tested, _) = TESTED_DEPTH.get();
            let state = if tested != 0 && tested == *resource {
                TESTED_DEPTH.set((tested, true));
                DEPTH_READ | SHADER_RESOURCE
            } else {
                SHADER_RESOURCE
            };
            crate::list::ensure_state(list, *resource as This, state);
            #[cfg(feature = "diagnostics")]
            if let Some(t) = crate::resource::host_resource(*resource as This) {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                if d.Flags.0 & 7 == 0 {
                    tracing::debug!(
                        resource = format_args!("{resource:#x}"),
                        ?d,
                        "a shader reads a texture the title fills"
                    );
                }
            }
        }
        (View::Unordered(resource, _, _), ResourceType::Uav) => {
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(*resource as This);
            // Unordered access to a depth texture is its colour twin's.
            let twin = list.twin_use(*resource as This, true, true);
            if !twin {
                crate::list::ensure_state(list, *resource as This, UNORDERED_ACCESS);
            }
        }
        _ => {}
    }
}

/// The pixel shaders whose draws get their slots listed
/// ([`State::slots_text`]): `VICTORY_LIST_SLOTS`, hash prefixes separated
/// by commas. None by default.
#[cfg(feature = "diagnostics")]
fn list_slots() -> &'static [String] {
    static LIST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        std::env::var("VICTORY_LIST_SLOTS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// A fingerprint of the 4 KiB of title memory at `at` (FNV-1a, 64 bits; of
/// 4 KiB of zeros it is always the same value, which `slots.py` knows), or
/// "unreadable".
#[cfg(feature = "diagnostics")]
fn print_4k(at: u64) -> String {
    let mut bytes = [0u8; 4096];
    if at == 0 || !crate::host::read_into(at as *const u8, &mut bytes) {
        return "unreadable".into();
    }
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for b in bytes {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The `bytes` of title memory at `at`: how many are mapped, and the
/// fingerprints of the first and the last 4 KiB (a texture's smallest
/// levels lie first, its largest last).
#[cfg(feature = "diagnostics")]
fn memory_state(at: u64, bytes: u64) -> String {
    let mapped = crate::texture::mapped_bytes(at as *const u8, bytes as usize);
    let last = at + bytes.saturating_sub(4096);
    format!("memory={at:#x} mapped={mapped}/{bytes} first4k={} last4k={}", print_4k(at), print_4k(last))
}

/// Title texture `this` for the frame listing: its size, whether it has a
/// host copy (not made here), its staging buffer's state, whether the host
/// copied into it, and its memory ([`memory_state`]).
#[cfg(feature = "diagnostics")]
fn texture_state(this: This) -> String {
    let Some((at, desc)) = crate::resource::texture_memory(this) else {
        return "not a texture".into();
    };
    // SAFETY: 56 bytes in the description's layout.
    let words = unsafe { std::ptr::from_ref(&desc).cast::<[u32; 14]>().read_unaligned() };
    let bytes = crate::resource::placed_bytes(&words).unwrap_or(0);
    let staged = match crate::texture::staged_state(this) {
        Some((uploaded, dirty)) => format!("staged=yes uploaded={uploaded} dirty={dirty}"),
        None => "staged=no".into(),
    };
    format!(
        "{}x{} f{} levels={} host_copy={} {staged} host_written={} title_fills={} {}",
        u64::from(words[4]) | u64::from(words[5]) << 32,
        words[6],
        words[8],
        words[7] >> 16,
        crate::resource::host_made(this).is_some(),
        crate::texture::host_wrote(this as usize),
        crate::resource::title_fills(this),
        memory_state(at as u64, bytes)
    )
}

/// A view of the title's heap for the frame listing: a texture with its
/// state ([`texture_state`]; a placed one that has no resource yet with
/// its memory alone), anything else in a few words.
#[cfg(feature = "diagnostics")]
fn view_state(view: Option<&View>) -> String {
    match view {
        None => "no view".into(),
        Some(View::Placed(false, r, d)) if r[0] != 1 => {
            let address = u64::from(d[10]) | u64::from(d[11]) << 32;
            let state = match crate::resource::texture_at_made(address, r) {
                Some(this) => texture_state(this),
                None => format!(
                    "{}x{} f{} levels={} resource=none {}",
                    u64::from(r[4]) | u64::from(r[5]) << 32,
                    r[6],
                    r[8],
                    r[7] >> 16,
                    memory_state(address, crate::resource::placed_bytes(r).unwrap_or(0))
                ),
            };
            format!("placed texture view_format={} {state}", d[0])
        }
        Some(View::Shader(r, d)) if crate::resource::texture_memory(*r as This).is_some() => {
            format!(
                "texture object {r:#x} view_format={} {}",
                d.as_ref().map_or(0, |d| d[0]),
                texture_state(*r as This)
            )
        }
        Some(v) => format!("{v:?}").chars().take(80).collect(),
    }
}

/// A view in a few words (diagnostics builds).
#[cfg(feature = "diagnostics")]
fn describe(view: Option<&View>) -> String {
    let texture = |r: This| {
        crate::resource::host_made(r).map(|t| {
            // SAFETY: a live resource.
            let d = unsafe { t.GetDesc() };
            format!("{}x{} f{} flags {:#x} of {r:?}", d.Width, d.Height, d.Format.0, d.Flags.0)
        })
    };
    match view {
        None => "no view".into(),
        Some(v @ View::Shader(r, _)) => texture(*r as This)
            .unwrap_or_else(|| format!("buffer {:?}", crate::descriptor::host::buffer_range(v))),
        Some(v @ View::Unordered(r, ..)) => match texture(*r as This) {
            Some(t) => format!("uav texture {t}"),
            None => format!(
                "uav resource {r:#x} buffer {:?}",
                crate::descriptor::host::buffer_range(v)
            ),
        },
        Some(View::Placed(u, r, d)) => format!(
            "placed uav={u} dim {} {}x{} f{} layout {:#x} at {:#x}",
            r[0],
            u64::from(r[4]) | u64::from(r[5]) << 32,
            r[6],
            r[8],
            r[11],
            u64::from(d[10]) | u64::from(d[11]) << 32
        ),
        // The first four values, as a colour rectangle's constants are.
        Some(View::Constant(address, size))
            if *size >= 16 && crate::texture::committed(*address as *const u8, 16) =>
        {
            // SAFETY: 16 committed bytes of title memory.
            let v = unsafe { (*address as *const [f32; 4]).read_unaligned() };
            format!("Constant({address:#x}, {size}) {v:?}")
        }
        Some(v) => format!("{v:?}").chars().take(60).collect(),
    }
}

/// The console's own primitive topologies.
const TRIANGLE_FAN: u32 = 6;
const RECTANGLES: u32 = 17;
const LINE_LOOP: u32 = 18;
const QUADS: u32 = 19;
const QUAD_STRIP: u32 = 20;

/// Fixed-function state, straight onto the host list.
pub(crate) fn viewports(list: &HostList, viewports: &[D3D12_VIEWPORT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetViewports(viewports) };
}

pub(crate) fn scissors(list: &HostList, rects: &[RECT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetScissorRects(rects) };
}

pub(crate) fn blend_factor(list: &HostList, factor: Option<&[f32; 4]>) {
    // SAFETY: an open list.
    unsafe { list.list.OMSetBlendFactor(factor) };
}

pub(crate) fn stencil_ref(list: &HostList, reference: u32) {
    #[cfg(feature = "diagnostics")]
    crate::capture::op(std::ptr::from_ref(list) as usize, || format!("STENCIL REF {reference:#x}"));
    // SAFETY: an open list.
    unsafe { list.list.OMSetStencilRef(reference) };
}

pub(crate) fn depth_bounds(list: &HostList, min: f32, max: f32) {
    #[cfg(feature = "diagnostics")]
    crate::capture::op(std::ptr::from_ref(list) as usize, || {
        format!("DEPTH BOUNDS {min}..{max}")
    });
    if crate::host::host().is_some_and(|h| h.depth_bounds())
        && let Ok(l1) = list.list.cast::<ID3D12GraphicsCommandList1>()
    {
        // SAFETY: an open list on a device that supports depth bounds.
        unsafe { l1.OMSetDepthBounds(min, max) };
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn texture_upload_budgets_are_shared_across_early_passes() {
        use super::{Duration, TexturePass, TextureUploads};

        let regular = Duration::from_millis(12);
        let bake = Duration::from_millis(1_500);
        let mut frame = TextureUploads::new(Some(5));
        frame.spent = 8 << 20;
        assert_eq!(frame.budget(TexturePass::Indexed), Some((Duration::from_millis(250), 192 << 20, false)));
        let ordinary = || TexturePass::Early { regular, bake: Duration::ZERO };
        assert_eq!(frame.budget(ordinary()), Some((regular, 72 << 20, false)));
        assert_eq!(frame.budget(ordinary()), None);

        let before_bake = || TexturePass::Early { regular, bake };
        assert_eq!(frame.budget(before_bake()), Some((bake, (2 << 30) + (8 << 20), true)));
        frame.spent += 128 << 20;
        frame.bake_spent += 128 << 20;
        frame.bake_time += Duration::from_millis(450);
        assert_eq!(frame.budget(before_bake()), Some((Duration::from_millis(1_050), (2 << 30) + (8 << 20), true)));
        frame.bake_time = bake;
        assert_eq!(frame.budget(before_bake()), None);
        frame.bake_time = Duration::ZERO;
        frame.bake_spent = 2 << 30;
        assert_eq!(frame.budget(before_bake()), None);
    }

    use super::{GivesBack, gives_back};

    /// A shader that reads title memory by address and stores only through
    /// views (the ray-tracing passes) gives the copies of title memory
    /// nothing back; a quarantined builder stage gives nothing back at all,
    /// even with every range declared (`VICTORY_WRITE_SKIP=0`).
    #[test]
    fn view_stores_leave_title_memory_by_address_alone() {
        let back = |views, memory| GivesBack { views, memory };
        assert_eq!(gives_back(true, false, true, false), back(true, false));
        assert_eq!(gives_back(true, true, true, false), back(true, true));
        assert_eq!(gives_back(false, false, true, false), back(false, false));
        assert_eq!(gives_back(false, false, false, false), back(true, true));
        for skip in [true, false] {
            assert_eq!(gives_back(true, true, skip, true), back(false, false));
        }
    }
}
