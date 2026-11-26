//! More queues on the card, for copies only (`VICTORY_QUEUE2`; off by
//! default): work of ours that does not depend on the order of the title's
//! draws goes to a copy queue, which runs on the card's copy engine beside
//! the engine that draws.
//!
//! One thing goes there in this tree: the read backs of what shaders wrote
//! (texture to buffer copies, `crate::list` and `Host::execute`). They
//! stood in the direct queue behind every draw of the submission.
//!
//! What a copy queue cannot do is move a texture's state: it takes only
//! COMMON, COPY_DEST and COPY_SOURCE, so the title's list still moves the
//! texture to COPY_SOURCE (and leaves it there). The fetch copies it once
//! the submission has run. A texture used again after the read back in the
//! same submission ends in another state or has moved since: its fetch
//! goes on the direct queue instead, with the transitions it needs
//! ([`Fetch::moves`] and the host copy's state tell the two apart).
//!
//! Not ported here: the up lane of the fork's `VICTORY_QUEUE2` bit 2
//! (title bytes on their way to copies in the card's memory on a copy
//! queue). This tree keeps no copies of title memory in the card's memory,
//! so there is nothing for it to move; the bit is accepted and does
//! nothing.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::Interface;

/// A texture's state a copy queue may read it in.
const COPY_SOURCE: u32 = 0x800;

/// `VICTORY_QUEUE2`, a sum of bits; unset or 0: off, everything goes
/// through the one direct queue as before.
///
/// - 1: read backs of what shaders wrote go to a copy queue;
/// - 2: nothing in this tree (accepted, so the fork's switch sets still
///   parse; there are no copies in the card's memory for it to move).
///
/// Read once, when the first fetch is recorded.
pub(crate) fn bits() -> u32 {
    static BITS: LazyLock<u32> = LazyLock::new(|| {
        let bits = std::env::var("VICTORY_QUEUE2")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0)
            & 3;
        if bits & 2 != 0 {
            tracing::warn!(
                "VICTORY_QUEUE2 bit 2 (title bytes to copies in the card's memory on a copy queue) has no counterpart in this tree: no copies of title memory live in the card's memory here"
            );
        }
        if bits & 1 != 0 {
            tracing::info!("read backs of what shaders wrote run on a copy queue (VICTORY_QUEUE2={bits})");
        }
        bits
    });
    *BITS
}

/// A texture-to-buffer copy recorded by a title list and run once its
/// submission has run, on a copy queue when the texture is left in
/// COPY_SOURCE ([`crate::queue2`] module note).
pub(crate) struct Fetch {
    /// The host texture read from, kept alive, with the state and the
    /// number of state moves it had when the fetch was recorded.
    pub(crate) copy: Arc<crate::resource::HostTexture>,
    /// The readback buffer, kept alive by the list as well.
    buffer: ID3D12Resource,
    /// The texture's subresource, and the buffer's footprint.
    pub(crate) subresource: u32,
    footprint: crate::resource::PlacedFootprint,
    /// Where in the buffer's footprint the copy starts.
    at: [u32; 3],
    /// The box of the texture copied, if the title gave one.
    region: Option<D3D12_BOX>,
    /// The host copy's state moves when the fetch was recorded.
    moves: u64,
    /// Bytes the copy reads, for the line.
    pub(crate) bytes: u64,
}

// SAFETY: D3D12 resources are free-threaded; the copy names live ones.
unsafe impl Send for Fetch {}
// SAFETY: as above.
unsafe impl Sync for Fetch {}

/// An allocator and a list of one type, used by one batch at a time.
struct Lists {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
}

impl Lists {
    fn new(device: &ID3D12Device, kind: D3D12_COMMAND_LIST_TYPE) -> windows::core::Result<Self> {
        // SAFETY: plain object creation.
        unsafe {
            let allocator = device.CreateCommandAllocator(kind)?;
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, kind, &allocator, None)?;
            // It comes back open, and the first batch resets it first.
            list.Close()?;
            Ok(Self { allocator, list })
        }
    }

    fn reset(&self) -> windows::core::Result<()> {
        // SAFETY: the batch before it has run and been waited for.
        unsafe { self.allocator.Reset()? };
        // SAFETY: as above.
        unsafe { self.list.Reset(&self.allocator, None) }
    }

    fn close(&self) -> windows::core::Result<()> {
        // SAFETY: ours, open.
        unsafe { self.list.Close() }
    }
}

/// A copy queue and the lists it runs, with a fence of its own, and the
/// direct queue for fetches that came to a texture too late for it.
pub(crate) struct Second {
    queue: ID3D12CommandQueue,
    /// The host's direct queue, for a fetch the copy queue may not make.
    direct: ID3D12CommandQueue,
    fence: ID3D12Fence,
    event: HANDLE,
    next: AtomicU64,
    copy: Mutex<Lists>,
    direct_lists: Mutex<Lists>,
}

// SAFETY: D3D12 queues, fences, allocators and lists are free-threaded;
// the lists and the fence value are used under the locks.
unsafe impl Send for Second {}
// SAFETY: as above.
unsafe impl Sync for Second {}

/// Since the start: fetches on the copy queue, their bytes, batches, those
/// that had to go on the direct queue, and the copy queue's microseconds.
static FETCHED: [AtomicU64; 3] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
static FETCHED_DIRECT: AtomicU64 = AtomicU64::new(0);
static FETCH_US: AtomicU64 = AtomicU64::new(0);

/// A copy queue (or its lists) failed once: read backs stay in the title's
/// lists from then on, as without the switch, rather than silently copying
/// nothing.
static BROKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Says once why read backs stopped going to the copy queue.
fn broke(what: &str, e: windows::core::Error) {
    if !BROKEN.swap(true, Ordering::Relaxed) {
        tracing::warn!(error = %e, "copy queue of ours failed ({what}): read backs stay in the title's lists from now (VICTORY_QUEUE2)");
    }
}

/// The copy queues, made at the first ask if [`bits`] says so; `None` when
/// the switch is off or they could not be made (read backs then stay in
/// the title's lists, as before).
pub(crate) fn second() -> Option<&'static Second> {
    static SECOND: LazyLock<Option<Second>> = LazyLock::new(|| {
        if bits() & 1 == 0 {
            return None;
        }
        let host = crate::host::host()?;
        match Second::new(host) {
            Ok(second) => Some(second),
            Err(e) => {
                tracing::warn!(error = %e, "no copy queue for read backs (VICTORY_QUEUE2): they stay in the title's lists");
                None
            }
        }
    });
    SECOND.as_ref()
}

impl Second {
    fn new(host: &crate::host::Host) -> windows::core::Result<Self> {
        // SAFETY: plain object creation with valid descriptions.
        unsafe {
            let queue: ID3D12CommandQueue = host.device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_COPY,
                ..Default::default()
            })?;
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let event = CreateEventW(None, false, false, None)?;
            Ok(Self {
                queue,
                direct: host.direct_queue().clone(),
                fence,
                event,
                next: AtomicU64::new(0),
                copy: Mutex::new(Lists::new(&host.device, D3D12_COMMAND_LIST_TYPE_COPY)?),
                direct_lists: Mutex::new(Lists::new(&host.device, D3D12_COMMAND_LIST_TYPE_DIRECT)?),
            })
        }
    }

    /// Runs the closed `lists` on `queue` and waits for it; false if it
    /// could not be sent (nothing then runs and the fetch's rows stay as
    /// they were).
    fn send(&self, lists: &Lists, queue: &ID3D12CommandQueue) -> bool {
        if let Err(e) = lists.close() {
            broke("a list did not close", e);
            return false;
        }
        let Some(list) = lists.list.cast::<ID3D12CommandList>().ok() else {
            return false;
        };
        // SAFETY: a closed list of ours, live queue, fence and event.
        unsafe {
            queue.ExecuteCommandLists(&[Some(list)]);
            let value = self.next.fetch_add(1, Ordering::Relaxed) + 1;
            if let Err(e) = queue.Signal(&self.fence, value) {
                broke("a queue was not signalled", e);
                return false;
            }
            let waited = self
                .fence
                .SetEventOnCompletion(value, self.event)
                .map(|()| WaitForSingleObject(self.event, INFINITE));
            if let Err(e) = waited {
                broke("a queue's fence was not waited for", e);
                return false;
            }
        }
        true
    }

    /// Records one texture to buffer copy.
    fn record(&self, lists: &Lists, fetch: &Fetch) {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&fetch.buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: fetch.footprint.offset,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: DXGI_FORMAT(fetch.footprint.format as i32),
                        Width: fetch.footprint.width,
                        Height: fetch.footprint.height,
                        Depth: fetch.footprint.depth,
                        RowPitch: fetch.footprint.row_pitch,
                    },
                },
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&fetch.copy.texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: fetch.subresource,
            },
        };
        // SAFETY: an open list; both locations name live resources in their
        // states (COPY_SOURCE and COPY_DEST).
        unsafe {
            lists.list.CopyTextureRegion(
                &to,
                fetch.at[0],
                fetch.at[1],
                fetch.at[2],
                &from,
                fetch.region.as_ref().map(std::ptr::from_ref),
            )
        };
    }

    /// The fetches the copy queue may make: the texture is left in
    /// COPY_SOURCE and no use of it followed the read back.
    fn fetch_on_copy(&self, fetches: &[&Fetch]) -> bool {
        let lists = self.copy.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = lists.reset() {
            broke("a list did not reset", e);
            return false;
        }
        for fetch in fetches {
            self.record(&lists, fetch);
        }
        let started = std::time::Instant::now();
        let sent = self.send(&lists, &self.queue);
        if sent {
            FETCHED[0].fetch_add(fetches.len() as u64, Ordering::Relaxed);
            FETCHED[1].fetch_add(fetches.iter().map(|f| f.bytes).sum::<u64>(), Ordering::Relaxed);
            FETCH_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
            let batches = FETCHED[2].fetch_add(1, Ordering::Relaxed) + 1;
            if batches.is_multiple_of(2048) {
                let [n, bytes] = [0, 1].map(|i| FETCHED[i].swap(0, Ordering::Relaxed));
                let us = FETCH_US.swap(0, Ordering::Relaxed);
                let direct = FETCHED_DIRECT.swap(0, Ordering::Relaxed);
                tracing::info!(
                    "read backs on the copy queue (VICTORY_QUEUE2=1): {n} in {batches} batches, {:.1} MB, {:.1} ms a batch; {direct} had to go on the direct queue",
                    bytes as f64 / 1e6,
                    us as f64 / 1e3 / batches.max(1) as f64
                );
            }
        }
        sent
    }

    /// The fetches the copy queue may not make: the texture is used again
    /// after the read back, so its state and contents are later than the
    /// copy's point. Runs them on the direct queue, which has run and is
    /// idle, with the transitions the copy needs.
    fn fetch_on_direct(&self, fetches: &[&Fetch]) -> bool {
        let lists = self.direct_lists.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = lists.reset() {
            broke("a list did not reset", e);
            return false;
        }
        for fetch in fetches {
            let real = fetch.copy.state.load(Ordering::Relaxed);
            let source = crate::host::legal_state(&fetch.copy.texture, COPY_SOURCE);
            let back = |from: u32, to: u32| {
                crate::host::transition(
                    &fetch.copy.texture,
                    D3D12_RESOURCE_STATES(from as i32),
                    D3D12_RESOURCE_STATES(to as i32),
                )
            };
            // SAFETY: an open list; the texture is alive and not executing
            // (the submission has run and been waited for).
            unsafe {
                if real != source {
                    lists.list.ResourceBarrier(&[back(real, source)]);
                }
            }
            self.record(&lists, fetch);
            // SAFETY: as above.
            unsafe {
                if real != source {
                    lists.list.ResourceBarrier(&[back(source, real)]);
                }
            }
        }
        let sent = self.send(&lists, &self.direct);
        if sent {
            FETCHED_DIRECT.fetch_add(fetches.len() as u64, Ordering::Relaxed);
        }
        sent
    }
}

/// A fetch of the texture, if the switch is on and its copy queue could be
/// made; `None` means the caller records the copy into its own list, as
/// without the switch.
pub(crate) fn fetch(
    copy: &Arc<crate::resource::HostTexture>,
    buffer: &ID3D12Resource,
    subresource: u32,
    footprint: crate::resource::PlacedFootprint,
    at: [u32; 3],
    region: Option<D3D12_BOX>,
    bytes: u64,
) -> Option<Fetch> {
    if BROKEN.load(Ordering::Relaxed) {
        return None;
    }
    second()?;
    Some(Fetch {
        copy: copy.clone(),
        buffer: buffer.clone(),
        subresource,
        footprint,
        at,
        region,
        moves: copy.moves.load(Ordering::Relaxed),
        bytes,
    })
}

/// Runs the read backs a submission recorded, after it has run and before
/// its rows go into title memory (`Host::execute`).
pub(crate) fn run(fetches: &[Fetch]) {
    if fetches.is_empty() {
        return;
    }
    let Some(second) = second() else { return };
    // The fetches whose texture is left in COPY_SOURCE and has not moved
    // since the read back; the rest go on the direct queue.
    let (now, later): (Vec<&Fetch>, Vec<&Fetch>) = fetches.iter().partition(|f| {
        f.copy.state.load(Ordering::Relaxed) == COPY_SOURCE
            && f.copy.moves.load(Ordering::Relaxed) == f.moves
    });
    if !now.is_empty() && !second.fetch_on_copy(&now) {
        // The copy queue could not make it: the direct queue, which has run
        // and is idle, does (a second copy into the same buffer does no harm).
        second.fetch_on_direct(&now);
    }
    if !later.is_empty() {
        second.fetch_on_direct(&later);
    }
}
