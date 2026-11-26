//! Diagnostics: which host copies of title memory are hot, and what each
//! would gain in video memory (#880, decision 0005's unified-memory item).
//!
//! Host copies are in system memory (`Host::shared_buffer`), so on a
//! discrete GPU every shader access crosses the bus. Per copy (by its title
//! start and length), since the last frame-rate line:
//!
//! - **GPU time**: with per-command timing (`VICTORY_GPU_TIMING` and
//!   `VICTORY_D3D12_DRED`), each command's time split over the copies it
//!   binds, by the bytes bound. Commands that take views from the heap
//!   itself, or reach title memory at addresses they compute, may read any
//!   copy: their time is counted apart, as two rows of their own;
//! - **bytes copied in**: refreshes, the pages the title wrote (what a copy
//!   in video memory would also need, as a copy up);
//! - **bytes copied back**: what shaders wrote, going back into title
//!   memory (a copy down from video memory).
//!
//! The estimate per copy is the GPU time video memory would save, less what
//! moving its changed bytes would cost there. The constants come from
//! synthetic measurements on an RTX 5070 Ti (`uma_bench`, #2912):
//! - video memory is ~30× (linear) to ~300× (scattered lines) faster for
//!   shaders than system memory, counted as [`SPEEDUP`] to stay low;
//! - the GPU copies 64 KiB pages between the two at ~28.7 GB/s
//!   ([`COPY_BYTES_PER_MS`]).
//!
//! It ranks candidates and moves nothing.
//!
//! shortcut: a command's copies are the refreshes it asks for
//! (`HostList::defer_refresh`, once per bound range); a copy it reads
//! without asking is not counted for it.

use std::collections::HashMap;
use std::sync::Mutex;

/// A host copy: its title start and length.
pub(crate) type Key = (u64, u64);
/// Commands that take views from the heap itself (`crate::bindless`).
pub(crate) const HEAP: Key = (u64::MAX, 0);
/// Commands that reach title memory at computed addresses (`crate::memory`).
pub(crate) const MEMORY: Key = (u64::MAX, 1);

/// How much faster shaders reach video memory than system memory, at the
/// low end of what was measured.
const SPEEDUP: f64 = 20.0;
/// Bytes the GPU copies between system and video memory per millisecond.
const COPY_BYTES_PER_MS: f64 = 28.7e6;

#[derive(Default, Clone, Copy)]
struct Heat {
    gpu_ms: f64,
    commands: u64,
    copied_in: u64,
    copied_back: u64,
}

impl Heat {
    /// What the copy would save per frame in video memory, in ms: the GPU
    /// time less what it would take there, less moving its changed bytes.
    fn gain(&self) -> f64 {
        self.gpu_ms * (1.0 - 1.0 / SPEEDUP)
            - (self.copied_in + self.copied_back) as f64 / COPY_BYTES_PER_MS
    }
}

static HEAT: Mutex<Option<HashMap<Key, Heat>>> = Mutex::new(None);

fn with(f: impl FnOnce(&mut HashMap<Key, Heat>)) {
    f(HEAT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new));
}

/// A refresh copied `bytes` into the copy `key`.
pub(crate) fn copied_in(key: Key, bytes: usize) {
    if bytes > 0 {
        with(|m| m.entry(key).or_default().copied_in += bytes as u64);
    }
}

/// `bytes` the shaders wrote went back from the copy `key`.
pub(crate) fn copied_back(key: Key, bytes: usize) {
    if bytes > 0 {
        with(|m| m.entry(key).or_default().copied_back += bytes as u64);
    }
}

/// A timed list ran: `times[i]` is its command `i`'s GPU time (ms), and
/// `uses` the copies each command binds, as (command, copy, bytes bound).
pub(crate) fn commands(times: &[f64], uses: &[(u32, Key, u64)]) {
    if !times.is_empty() && !uses.is_empty() {
        with(|m| attribute(m, times, uses));
    }
}

/// [`commands`] into `m`.
fn attribute(m: &mut HashMap<Key, Heat>, times: &[f64], uses: &[(u32, Key, u64)]) {
    // Bytes bound per command, to split its time by.
    let mut bound: HashMap<u32, u64> = HashMap::new();
    for &(i, _, bytes) in uses {
        *bound.entry(i).or_default() += bytes.max(1);
    }
    for &(i, key, bytes) in uses {
        let Some(&ms) = times.get(i as usize) else {
            continue;
        };
        let e = m.entry(key).or_default();
        e.gpu_ms += ms * bytes.max(1) as f64 / bound[&i] as f64;
        e.commands += 1;
    }
}

/// Logs the copies by estimated gain, per frame over `frames`.
pub(crate) fn log(frames: f64) {
    let Some(heat) = HEAT.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    if let Some(line) = report(&heat, frames, 8) {
        tracing::info!("{line}");
    }
}

/// The report line: totals, the shared rows, then the `top` copies by gain.
fn report(heat: &HashMap<Key, Heat>, frames: f64, top: usize) -> Option<String> {
    let mut copies: Vec<(&Key, &Heat)> = heat
        .iter()
        .filter(|(k, _)| **k != HEAP && **k != MEMORY)
        .collect();
    if copies.is_empty() && !heat.contains_key(&HEAP) && !heat.contains_key(&MEMORY) {
        return None;
    }
    let per = |v: f64| v / frames;
    let mb = |b: u64| b as f64 / 1e6 / frames;
    let gpu: f64 = copies.iter().map(|(_, h)| h.gpu_ms).sum();
    let mut line = format!(
        "host copies per frame: {} used, {:.1} ms of GPU time bound to them",
        copies.len(),
        per(gpu)
    );
    for (key, what) in [(HEAP, "heap views"), (MEMORY, "title memory by address")] {
        if let Some(h) = heat.get(&key) {
            line += &format!(
                ", {:.1} ms in {:.0} commands reading {what}",
                per(h.gpu_ms),
                per(h.commands as f64)
            );
        }
    }
    copies.sort_by(|a, b| b.1.gain().total_cmp(&a.1.gain()));
    let rows: Vec<String> = copies
        .iter()
        .take(top)
        .map(|((start, len), h)| {
            format!(
                "{start:#x}+{len:#x}: GPU {:.2} ms in {:.0} commands, in {:.1} MB, back {:.1} MB, \
                 {:+.2} ms in video memory",
                per(h.gpu_ms),
                per(h.commands as f64),
                mb(h.copied_in),
                mb(h.copied_back),
                -per(h.gain()),
            )
        })
        .collect();
    if !rows.is_empty() {
        line += &format!("; by gain: {}", rows.join("; "));
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_s_time_is_split_by_the_bytes_it_binds() {
        let mut m = HashMap::new();
        let (a, b) = ((0x1000, 0x100), (0x2000, 0x100));
        // Command 0 binds 200 bytes of a and 100 of b; command 1 a little
        // of a and reads the heap; command 2 is past the times read.
        let uses = [
            (0, a, 200),
            (0, b, 100),
            (1, a, 1),
            (1, HEAP, 0),
            (2, b, 64),
        ];
        attribute(&mut m, &[3.0, 1.0], &uses);
        assert!(
            (m[&a].gpu_ms - 2.5).abs() < 1e-9,
            "2 of 3 ms, and half of 1"
        );
        assert!((m[&b].gpu_ms - 1.0).abs() < 1e-9);
        assert!((m[&HEAP].gpu_ms - 0.5).abs() < 1e-9);
        assert_eq!((m[&a].commands, m[&b].commands), (2, 1));
    }

    #[test]
    fn copies_rank_by_what_video_memory_would_save() {
        let mut m = HashMap::new();
        // Read a lot, rarely written: the best candidate.
        m.insert(
            (0x1000, 0x10000),
            Heat {
                gpu_ms: 20.0,
                commands: 40,
                copied_in: 0,
                copied_back: 0,
            },
        );
        // Read as much, but rewritten whole every frame: less.
        m.insert(
            (0x2000_0000, 0x1000_0000),
            Heat {
                gpu_ms: 20.0,
                commands: 40,
                copied_in: 256 << 20,
                copied_back: 0,
            },
        );
        // Barely read: last.
        m.insert(
            (0x3000, 0x1000),
            Heat {
                gpu_ms: 0.1,
                commands: 1,
                copied_in: 0,
                copied_back: 4096,
            },
        );
        m.insert(
            HEAP,
            Heat {
                gpu_ms: 5.0,
                commands: 3,
                ..Default::default()
            },
        );
        let line = report(&m, 2.0, 8).unwrap();
        assert!(
            line.starts_with("host copies per frame: 3 used, 20.1 ms"),
            "{line}"
        );
        assert!(
            line.contains("2.5 ms in 2 commands reading heap views"),
            "{line}"
        );
        let first = line.find("0x1000+0x10000").unwrap();
        let second = line.find("0x20000000+0x10000000").unwrap();
        let third = line.find("0x3000+0x1000").unwrap();
        assert!(first < second && second < third, "{line}");
        assert!(line.contains("-9.50 ms in video memory"), "{line}");
        assert_eq!(report(&HashMap::new(), 1.0, 8), None);
    }
}
