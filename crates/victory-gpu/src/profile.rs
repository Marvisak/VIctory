//! Diagnostics-only GPU timestamps around the title's recorded work.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;

/// Up to this many measured calls per list. Report any overflow.
const CALLS: u32 = 8192;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum Kind {
    Draw,
    Mesh,
    Dispatch,
    Indirect,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Draw => "draw",
            Self::Mesh => "mesh",
            Self::Dispatch => "dispatch",
            Self::Indirect => "indirect",
        }
    }
}

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| matches!(std::env::var("VICTORY_GPU_PROFILE").as_deref(), Ok("1")))
}

type Key = (Kind, String);

#[derive(Default)]
struct Recording {
    calls: Vec<Key>,
    skipped: u64,
}

#[derive(Default)]
pub(crate) struct List {
    buffers: OnceLock<Option<(ID3D12QueryHeap, ID3D12Resource)>>,
    recording: Mutex<Recording>,
}

pub(crate) struct Stamp<'a> {
    list: &'a ID3D12GraphicsCommandList,
    heap: &'a ID3D12QueryHeap,
    end: u32,
}

impl Drop for Stamp<'_> {
    fn drop(&mut self) {
        // SAFETY: a live query heap, in bounds, and the list still records.
        unsafe {
            self.list
                .EndQuery(self.heap, D3D12_QUERY_TYPE_TIMESTAMP, self.end)
        };
    }
}

impl List {
    pub(crate) fn start<'a>(
        &'a self,
        list: &'a ID3D12GraphicsCommandList,
        kind: Kind,
        pipeline: crate::object::This,
    ) -> Option<Stamp<'a>> {
        if !enabled() {
            return None;
        }
        let mut record = self.recording.lock().unwrap_or_else(|e| e.into_inner());
        let i = record.calls.len() as u32;
        if i == CALLS {
            record.skipped += 1;
            return None;
        }
        let Some((heap, _)) = self
            .buffers
            .get_or_init(|| {
                let host = crate::host::host()?;
                crate::host::timestamp_buffers(&host.device, 2 * CALLS)
            })
            .as_ref()
        else {
            record.skipped += 1;
            return None;
        };
        let name = crate::pipeline::host_pipeline(pipeline)
            .map_or_else(|| "<unknown>".into(), |p| p.name.clone());
        record.calls.push((kind, name));
        // SAFETY: an open list and this list's heap with room for the pair.
        unsafe { list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, 2 * i) };
        Some(Stamp {
            list,
            heap,
            end: 2 * i + 1,
        })
    }

    pub(crate) fn resolve(&self, list: &ID3D12GraphicsCommandList) {
        let Some(Some((heap, back))) = self.buffers.get() else {
            return;
        };
        let count = self
            .recording
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .calls
            .len() as u32;
        if count != 0 {
            // SAFETY: these queries were all ended; the live readback buffer
            // is sized for the heap and remains in COPY_DEST.
            unsafe {
                list.ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2 * count, back, 0)
            };
        }
    }

    pub(crate) fn reset(&self) {
        let mut record = self.recording.lock().unwrap_or_else(|e| e.into_inner());
        record.calls.clear();
        record.skipped = 0;
    }

    /// Duplicate lists in one submission share the last resolve's results.
    pub(crate) fn overwritten(&self) {
        let record = self.recording.lock().unwrap_or_else(|e| e.into_inner());
        WINDOW.lock().unwrap_or_else(|e| e.into_inner()).skipped +=
            record.calls.len() as u64 + record.skipped;
    }

    /// Called after the submission fence completed. A list can be submitted
    /// again without resetting it, in which case the resolve runs again.
    pub(crate) fn collect(&self, frequency: u64) {
        let record = self.recording.lock().unwrap_or_else(|e| e.into_inner());
        if record.calls.is_empty() && record.skipped == 0 {
            return;
        }
        let mut window = WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        window.skipped += record.skipped;
        let Some(Some((_, back))) = self.buffers.get() else {
            return;
        };
        let n = record.calls.len() * 2;
        if n == 0 {
            return;
        }
        let range = D3D12_RANGE {
            Begin: 0,
            End: n * 8,
        };
        let mut data = std::ptr::null_mut();
        // SAFETY: the fence is complete, and the query resolve wrote n words.
        unsafe {
            if back.Map(0, Some(&range), Some(&mut data)).is_err() {
                window.skipped += record.calls.len() as u64;
                return;
            }
            let timestamps = std::slice::from_raw_parts(data.cast::<u64>(), n);
            window.add(&record.calls, timestamps, frequency);
            back.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Time {
    ms: f64,
    calls: u64,
}

#[derive(Default)]
struct Window {
    totals: HashMap<Key, Time>,
    frames: u64,
    skipped: u64,
}

impl Window {
    fn add(&mut self, calls: &[Key], timestamps: &[u64], frequency: u64) {
        let scale = 1000.0 / frequency as f64;
        for (call, pair) in calls.iter().zip(timestamps.chunks_exact(2)) {
            let Some(ticks) = pair[1].checked_sub(pair[0]) else {
                self.skipped += 1;
                continue;
            };
            let total = self.totals.entry(call.clone()).or_default();
            total.ms += ticks as f64 * scale;
            total.calls += 1;
        }
    }

    fn top(&self, n: usize) -> Vec<(&Key, Time)> {
        let mut sorted: Vec<_> = self.totals.iter().map(|(k, &t)| (k, t)).collect();
        sorted.sort_unstable_by(|(a, x), (b, y)| y.ms.total_cmp(&x.ms).then_with(|| a.cmp(b)));
        sorted.truncate(n);
        sorted
    }
}

static WINDOW: std::sync::LazyLock<Mutex<Window>> =
    std::sync::LazyLock::new(|| Mutex::new(Window::default()));

pub(crate) fn presented() {
    if enabled() {
        WINDOW.lock().unwrap_or_else(|e| e.into_inner()).frames += 1;
    }
}

pub(crate) fn log() {
    if !enabled() {
        return;
    }
    let window = std::mem::take(&mut *WINDOW.lock().unwrap_or_else(|e| e.into_inner()));
    let frames = window.frames.max(1) as f64;
    let total: f64 = window.totals.values().map(|v| v.ms).sum();
    tracing::info!(
        target: "victory_gpu::gpu_profile",
        "GPU profile: {} frames, {:.2} timed ms/frame, {} calls skipped",
        window.frames, total / frames, window.skipped
    );
    for ((kind, name), time) in window.top(25) {
        tracing::info!(
            target: "victory_gpu::gpu_profile",
            "{:.3} GPU ms/frame, {:.1} calls/frame ({} total), {} {name}",
            time.ms / frames, time.calls as f64 / frames, time.calls, kind.name()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_repeated_calls_across_lists_and_ranks_by_duration() {
        let draw = (Kind::Draw, "same pipeline".into());
        let dispatch = (Kind::Dispatch, "same pipeline".into());
        let indirect = (Kind::Indirect, "short pipeline".into());
        let mut window = Window {
            frames: 2,
            ..Window::default()
        };
        window.add(
            &[draw.clone(), dispatch.clone(), indirect.clone()],
            &[10, 13, 20, 41, 54, 55],
            1000,
        );
        window.add(
            &[draw.clone(), dispatch.clone(), indirect.clone()],
            &[60, 67, 68, 69, 80, 79],
            1000,
        );
        let top = window.top(2);
        assert_eq!((*top[0].0).clone(), dispatch);
        assert_eq!((*top[1].0).clone(), draw);
        assert_eq!(
            (top[0].1.ms / window.frames as f64, top[0].1.calls),
            (11.0, 2)
        );
        assert_eq!(
            (top[1].1.ms / window.frames as f64, top[1].1.calls),
            (5.0, 2)
        );
        assert_eq!(window.skipped, 1);
    }
}
