//! Diagnostics utilities: DRED device-removal diagnostics, opt-in replay/timeline
//! switches, and bounded diagnostic ring buffers for <= 16 GiB RAM systems.
//!
//! Systems with <= 16 GiB physical RAM operate under tight memory constraints when
//! emulating unified console memory. Diagnostic logging (deep command replay,
//! high-frequency timeline event logging) must enforce strict ring-buffer bounds
//! so that diagnostics never cause memory bloat or out-of-memory crashes.

use std::sync::Mutex;

/// 16 GiB in bytes (17,179,869,184 bytes).
pub const SIXTEEN_GIB: u64 = 16 * 1024 * 1024 * 1024;

/// Default capacity for timeline event slots on systems with > 16 GiB RAM (~64 MiB).
pub const DEFAULT_TIMELINE_CAPACITY: usize = 1 << 20;

/// Bounded capacity for timeline event slots on systems with <= 16 GiB RAM (~4 MiB).
pub const BOUNDED_TIMELINE_CAPACITY: usize = 1 << 16;

/// Default capacity for command replay / snapshot entries on systems with > 16 GiB RAM.
pub const DEFAULT_REPLAY_CAPACITY: usize = 1024;

/// Bounded capacity for command replay / snapshot entries on systems with <= 16 GiB RAM.
pub const BOUNDED_REPLAY_CAPACITY: usize = 64;

/// Returns total host physical RAM in bytes from Windows `GlobalMemoryStatusEx`,
/// or 0 on non-Windows platforms or query failure.
pub fn host_physical_ram() -> u64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: MEMORYSTATUSEX consists entirely of integer fields and is safe to zero-initialize.
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        // SAFETY: `m` is initialized with its expected struct size and valid for `GlobalMemoryStatusEx`.
        if unsafe { GlobalMemoryStatusEx(&mut m) } != 0 {
            m.ullTotalPhys
        } else {
            0
        }
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Checks whether the system has <= 16 GiB physical RAM and requires bounded buffers.
#[inline]
pub fn is_bounded_memory_system(ram_bytes: u64) -> bool {
    ram_bytes <= SIXTEEN_GIB
}

/// Computes the effective diagnostic ring buffer capacity given physical RAM and limits.
///
/// If `ram_bytes` is <= 16 GiB, the capacity is capped to `bounded_capacity`.
/// Otherwise, `default_capacity` is used.
#[inline]
pub fn diagnostic_capacity(ram_bytes: u64, default_capacity: usize, bounded_capacity: usize) -> usize {
    if is_bounded_memory_system(ram_bytes) {
        bounded_capacity
    } else {
        default_capacity
    }
}

/// Whether deep replay tracing is opted in (`VICTORY_REPLAY` or `VICTORY_RECORD`).
#[inline]
pub fn replay_on() -> bool {
    crate::switch::on("VICTORY_REPLAY") || crate::switch::on("VICTORY_RECORD")
}

/// Whether timeline tracing is opted in (`VICTORY_TIMELINE`).
#[inline]
pub fn timeline_on() -> bool {
    crate::switch::on("VICTORY_TIMELINE")
}

/// Whether DRED device-removal diagnostics are enabled (`VICTORY_D3D12_DRED`).
#[inline]
pub fn dred_on() -> bool {
    crate::switch::on("VICTORY_D3D12_DRED")
}

/// Computes the bounded timeline event capacity for this host's RAM.
pub fn timeline_event_capacity() -> usize {
    diagnostic_capacity(host_physical_ram(), DEFAULT_TIMELINE_CAPACITY, BOUNDED_TIMELINE_CAPACITY)
}

/// Computes the bounded replay entry capacity for this host's RAM.
pub fn replay_entry_capacity() -> usize {
    diagnostic_capacity(host_physical_ram(), DEFAULT_REPLAY_CAPACITY, BOUNDED_REPLAY_CAPACITY)
}

/// A fixed-size, bounded ring buffer for diagnostics events.
///
/// When full, subsequent pushes overwrite the oldest entries and record a drop count,
/// guaranteeing that memory allocation never grows or causes OOM on memory-constrained systems.
#[derive(Debug)]
pub struct DiagnosticRingBuffer<T> {
    entries: Vec<Option<T>>,
    capacity: usize,
    head: usize,
    count: usize,
    dropped: u64,
}

impl<T> DiagnosticRingBuffer<T> {
    /// Creates a ring buffer with an explicit maximum capacity.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let mut entries = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            entries.push(None);
        }
        Self {
            entries,
            capacity,
            head: 0,
            count: 0,
            dropped: 0,
        }
    }

    /// Creates a ring buffer bounded according to system physical RAM.
    pub fn for_ram(ram_bytes: u64, default_cap: usize, bounded_cap: usize) -> Self {
        let cap = diagnostic_capacity(ram_bytes, default_cap, bounded_cap);
        Self::new(cap)
    }

    /// Pushes an element into the ring buffer.
    ///
    /// If the buffer is full, the oldest element is replaced and `dropped` is incremented.
    pub fn push(&mut self, item: T) {
        if self.count == self.capacity {
            self.dropped = self.dropped.saturating_add(1);
        } else {
            self.count += 1;
        }
        self.entries[self.head] = Some(item);
        self.head = (self.head + 1) % self.capacity;
    }

    /// Total capacity of this ring buffer.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of active items currently retained.
    #[inline]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether the ring buffer is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Count of items dropped due to capacity saturation.
    #[inline]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Clears all elements from the ring buffer.
    pub fn clear(&mut self) {
        for slot in &mut self.entries {
            *slot = None;
        }
        self.head = 0;
        self.count = 0;
    }

    /// Drains all retained items in chronological order (oldest to newest).
    pub fn drain(&mut self) -> Vec<T> {
        let mut result = Vec::with_capacity(self.count);
        if self.count == 0 {
            return result;
        }
        let start = if self.count < self.capacity {
            0
        } else {
            self.head
        };
        for i in 0..self.count {
            let idx = (start + i) % self.capacity;
            if let Some(item) = self.entries[idx].take() {
                result.push(item);
            }
        }
        self.count = 0;
        self.head = 0;
        result
    }
}

/// Thread-safe concurrent wrapper around [`DiagnosticRingBuffer`].
#[derive(Debug)]
pub struct ConcurrentDiagnosticRing<T> {
    inner: Mutex<DiagnosticRingBuffer<T>>,
}

impl<T> ConcurrentDiagnosticRing<T> {
    /// Creates a new concurrent ring buffer with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(DiagnosticRingBuffer::new(capacity)),
        }
    }

    /// Creates a concurrent ring buffer bounded for the specified RAM size.
    pub fn for_ram(ram_bytes: u64, default_cap: usize, bounded_cap: usize) -> Self {
        Self {
            inner: Mutex::new(DiagnosticRingBuffer::for_ram(ram_bytes, default_cap, bounded_cap)),
        }
    }

    /// Pushes an item into the ring buffer.
    pub fn push(&self, item: T) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).push(item);
    }

    /// Drains all items from the ring buffer.
    pub fn drain(&self) -> Vec<T> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).drain()
    }

    /// Returns the number of retained items and the dropped count.
    pub fn stats(&self) -> (usize, u64) {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        (guard.len(), guard.dropped())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_memory_system_detection() {
        assert!(is_bounded_memory_system(0));
        assert!(is_bounded_memory_system(8 * 1024 * 1024 * 1024));
        assert!(is_bounded_memory_system(16 * 1024 * 1024 * 1024));
        assert!(!is_bounded_memory_system(16 * 1024 * 1024 * 1024 + 1));
        assert!(!is_bounded_memory_system(32 * 1024 * 1024 * 1024));
        assert!(!is_bounded_memory_system(64 * 1024 * 1024 * 1024));
    }

    #[test]
    fn diagnostic_capacity_scales_on_low_ram() {
        let def = 1_000_000;
        let bnd = 10_000;

        assert_eq!(diagnostic_capacity(8 * 1024 * 1024 * 1024, def, bnd), bnd);
        assert_eq!(diagnostic_capacity(16 * 1024 * 1024 * 1024, def, bnd), bnd);
        assert_eq!(diagnostic_capacity(32 * 1024 * 1024 * 1024, def, bnd), def);
        assert_eq!(diagnostic_capacity(64 * 1024 * 1024 * 1024, def, bnd), def);
    }

    #[test]
    fn ring_buffer_bounds_memory_and_overwrites_oldest() {
        let mut ring = DiagnosticRingBuffer::new(3);
        assert_eq!(ring.capacity(), 3);
        assert_eq!(ring.len(), 0);
        assert!(ring.is_empty());

        ring.push(1);
        ring.push(2);
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.dropped(), 0);

        ring.push(3);
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.dropped(), 0);

        // Pushing beyond capacity must not allocate more and must drop the oldest element (1).
        ring.push(4);
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.dropped(), 1);

        ring.push(5);
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.dropped(), 2);

        // Drain in chronological order: [3, 4, 5]
        let items = ring.drain();
        assert_eq!(items, vec![3, 4, 5]);
        assert_eq!(ring.len(), 0);
        assert!(ring.is_empty());
        assert_eq!(ring.dropped(), 2);
    }

    #[test]
    fn concurrent_ring_buffer_thread_safety() {
        let ring = std::sync::Arc::new(ConcurrentDiagnosticRing::new(10));
        let mut threads = Vec::new();

        for t in 0..4 {
            let r = ring.clone();
            threads.push(std::thread::spawn(move || {
                for i in 0..10 {
                    r.push(t * 100 + i);
                }
            }));
        }

        for t in threads {
            t.join().unwrap();
        }

        let (len, dropped) = ring.stats();
        assert_eq!(len, 10);
        assert_eq!(dropped, 30);

        let items = ring.drain();
        assert_eq!(items.len(), 10);
        assert_eq!(ring.stats().0, 0);
    }

    #[test]
    fn bounded_ring_buffer_for_16gib_ram_system() {
        let ram_16gib = 16 * 1024 * 1024 * 1024;
        let ring = DiagnosticRingBuffer::<u32>::for_ram(
            ram_16gib,
            DEFAULT_TIMELINE_CAPACITY,
            BOUNDED_TIMELINE_CAPACITY,
        );
        assert_eq!(ring.capacity(), BOUNDED_TIMELINE_CAPACITY);

        let ram_32gib = 32 * 1024 * 1024 * 1024;
        let ring_large = DiagnosticRingBuffer::<u32>::for_ram(
            ram_32gib,
            DEFAULT_TIMELINE_CAPACITY,
            BOUNDED_TIMELINE_CAPACITY,
        );
        assert_eq!(ring_large.capacity(), DEFAULT_TIMELINE_CAPACITY);
    }
}
