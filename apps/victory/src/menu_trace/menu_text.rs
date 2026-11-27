//! Give VIctory's own Settings rows their text (diagnostics prototype).
//!
//! The title keeps the Settings menu's text in a reflected hash map: groups
//! of 16 one-byte tags (`key & 0x7f`, `0x80` empty) followed by 16 records
//! `{u32 key, u32 0, entry*}`. A key's home group is `(key >> 7) & (groups -
//! 1)`; a full group sends the search to the next one. An entry starts with
//! the title's text-entry vtable and holds a pointer to UTF-8 text.
//!
//! The map object holds the group count at `+0x08`, the record count at
//! `+0x0c`, the groups at `+0x10` and the map type's hash at `+0x18`. The map
//! that holds a known Video row name is the menu's. VIctory's keys are added
//! to free slots there, pointing at entries VIctory owns. Stock records are
//! never changed.

use std::sync::atomic::{AtomicUsize, Ordering};

use victory_loader::{mapped_regions, read_memory};

/// The hash of the map type, as its reflection data names it.
const MAP_TYPE: u32 = 0x9933_1089;
/// A Video row name in the Settings menu's text, used to pick the map.
const KNOWN_KEY: u32 = 0xb631_b899;
/// The Video tab's label ID, to see whether tab names use these maps.
const VIDEO_TAB: u32 = 0x8cb1_18f1;
/// The title's text-entry vtable.
const ENTRY_VTABLE_RVA: usize = 0x6501a20;
const EMPTY: u8 = 0x80;
const GROUP: usize = 16;
const GROUP_BYTES: usize = GROUP + GROUP * 16;
const MAX_GROUPS: u32 = 1 << 16;
/// Where the title's own memory is mapped first; higher views mirror it.
const SCAN_START: usize = 0x1_0000_0000;
const SCAN_END: usize = 0xf_0000_0000;

/// The map found last, checked again before each use.
static MAP: AtomicUsize = AtomicUsize::new(0);
/// Lookups in a row that found nothing in the recent file reads.
static MISSES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// Misses before the slow memory search is tried.
const MISSES_BEFORE_SEARCH: u32 = 3;

#[derive(Clone, Copy)]
struct Map {
    at: usize,
    groups: u32,
    count: u32,
    slots: usize,
}

/// Adds each `(key, text)` the menu map does not have yet. Returns how many
/// of them the map holds afterwards.
pub(super) fn provide(image_base: usize, texts: &[(u32, &'static str)]) -> usize {
    let Some(map) = menu_map() else {
        tracing::info!(target: "menu_trace", "menu text map not found");
        return 0;
    };
    let vtable = image_base + ENTRY_VTABLE_RVA;
    let mut held = 0;
    for &(key, text) in texts {
        if find(map, key).is_some() {
            held += 1;
            continue;
        }
        // SAFETY: the map was validated just now on the menu thread, which
        // builds the rows that will ask for these keys.
        if unsafe { insert(map, key, owned_entry(vtable, text)) } {
            held += 1;
        }
    }
    tracing::info!(target: "menu_trace", map = %format_args!("{:#x}", map.at), held,
                   "VIctory menu text provided");
    held
}

/// [`provide`] into the map found before, if it is still valid; never
/// scans. Returns how many of `texts` it holds (0 without a known map).
pub(super) fn provide_known(image_base: usize, texts: &[(u32, &'static str)]) -> usize {
    let cached = MAP.load(Ordering::Acquire);
    let known = (cached != 0)
        .then(|| map_at(cached))
        .flatten()
        .is_some_and(|map| find(map, KNOWN_KEY).is_some());
    if known { provide(image_base, texts) } else { 0 }
}

fn menu_map() -> Option<Map> {
    let cached = MAP.load(Ordering::Acquire);
    if let Some(map) = (cached != 0).then(|| map_at(cached)).flatten()
        && find(map, KNOWN_KEY).is_some()
    {
        return Some(map);
    }
    // The menu thread never searches memory. The map is loaded from a file
    // when Settings opens, and the file reader notes where its type word
    // landed (see `watch`): checking those few spots takes microseconds. A
    // miss usually means "not loaded yet" and the next visit looks again.
    // After repeated misses (a build that keeps the map elsewhere), a
    // read-only search runs on a background thread and leaves its result in
    // `MAP` for a later visit.
    let started = std::time::Instant::now();
    let map = in_loaded_data();
    if map.is_none() && MISSES.fetch_add(1, Ordering::AcqRel) + 1 >= MISSES_BEFORE_SEARCH {
        search_in_background();
    }
    tracing::info!(target: "menu_trace", us = started.elapsed().as_micros() as u64, found = map.is_some(),
                   "menu text map discovery");
    let map = map?;
    MISSES.store(0, Ordering::Release);
    MAP.store(map.at, Ordering::Release);
    Some(map)
}

/// From now on, the title's file reads note where the map's type word
/// lands. Call before Settings first opens.
pub(super) fn watch() {
    victory_hle::modules::dstorage::recent_reads::watch(MAP_TYPE);
}

/// The map, if one of the spots where loaded data held its type word is a
/// valid map now. Same checks as [`scan`]: no address or offset assumed.
fn in_loaded_data() -> Option<Map> {
    victory_hle::modules::dstorage::recent_reads::hits()
        .into_iter()
        .filter_map(|word| word.checked_sub(0x18).and_then(map_at))
        .find(|&map| find(map, KNOWN_KEY).is_some())
}

/// Starts one read-only memory search off the menu thread; its result is
/// checked again before use like any cached map.
fn search_in_background() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("victory-menu-text-search".into())
        .spawn(|| {
            let started = std::time::Instant::now();
            let map = scan();
            if let Some(map) = map {
                MAP.store(map.at, Ordering::Release);
            }
            tracing::info!(target: "menu_trace", ms = started.elapsed().as_millis() as u64,
                           found = map.is_some(), "menu text map background search");
            STARTED.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        STARTED.store(false, Ordering::Release);
    }
}

/// Reads only (through [`read_memory`], which fails instead of faulting),
/// so it may run on any thread.
fn scan() -> Option<Map> {
    let mut found = None;
    each_type_word(None, |candidate| {
        if let Some(map) = candidate.checked_sub(0x18).and_then(map_at)
            && find(map, KNOWN_KEY).is_some()
        {
            found = Some(map);
            return false;
        }
        true
    });
    found
}

/// Calls `visit` with the address of every 8-aligned [`MAP_TYPE`] word in
/// the title's mapped memory, until it returns false. Reads through
/// [`read_memory`] in chunks: the title may decommit a region between the
/// region query and the read, which must skip it, not fault. Stops at
/// `deadline`, checked before each chunk.
fn each_type_word(deadline: Option<std::time::Instant>, mut visit: impl FnMut(usize) -> bool) {
    const CHUNK: usize = 4 << 20;
    let needle = MAP_TYPE.to_le_bytes();
    for (start, size) in mapped_regions(SCAN_START, SCAN_END) {
        let mut at = start;
        while at < start + size {
            if deadline.is_some_and(|d| std::time::Instant::now() > d) {
                return;
            }
            let len = CHUNK.min(start + size - at);
            if let Some(bytes) = read_memory(at, len) {
                for hit in memchr::memmem::find_iter(&bytes, &needle) {
                    if (at + hit) % 8 == 0 && !visit(at + hit) {
                        return;
                    }
                }
            }
            at += len;
        }
    }
}

/// Logs how many of `keys` any loaded text map of the menu's type holds,
/// across every such map: to tell missing text from text kept elsewhere.
pub(super) fn census(keys: &[u32]) {
    let started = std::time::Instant::now();
    let (mut maps, mut records) = (0u32, 0u64);
    let mut held = std::collections::BTreeSet::new();
    let (mut known, mut tab_control) = (false, false);
    each_type_word(None, |candidate| {
        if let Some(map) = candidate.checked_sub(0x18).and_then(map_at) {
            maps += 1;
            records += u64::from(map.count);
            known |= find(map, KNOWN_KEY).is_some();
            tab_control |= find(map, VIDEO_TAB).is_some();
            held.extend(keys.iter().copied().filter(|&k| find(map, k).is_some()));
        }
        true
    });
    tracing::info!(target: "menu_trace", ms = started.elapsed().as_millis() as u64, maps, records, video_control = known, video_tab_id = tab_control,
                   asked = keys.len(), held = held.len(),
                   found = %format_args!("{:08x?}", held.iter().take(16).collect::<Vec<_>>()),
                   "text census of the Graphics row IDs across all loaded text maps");
}

/// The map whose object is at `at`, if its fields are consistent.
fn map_at(at: usize) -> Option<Map> {
    let bytes = read_memory(at, 0x20)?;
    let groups = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    let count = u32::from_le_bytes(bytes[12..16].try_into().ok()?);
    let slots = usize::from_le_bytes(bytes[16..24].try_into().ok()?);
    let kind = u32::from_le_bytes(bytes[24..28].try_into().ok()?);
    let fits = groups.is_power_of_two()
        && groups <= MAX_GROUPS
        && count < groups * GROUP as u32
        && kind == MAP_TYPE
        && read_memory(slots, groups as usize * GROUP_BYTES).is_some();
    fits.then_some(Map {
        at,
        groups,
        count,
        slots,
    })
}

/// The record address for `key`, by the title's probe order.
fn find(map: Map, key: u32) -> Option<usize> {
    let tag = (key & 0x7f) as u8;
    let mask = map.groups as usize - 1;
    let home = (key >> 7) as usize & mask;
    for step in 0..map.groups as usize {
        let group = map.slots + ((home + step) & mask) * GROUP_BYTES;
        let bytes = read_memory(group, GROUP_BYTES)?;
        for slot in 0..GROUP {
            let record = GROUP + slot * 16;
            if bytes[slot] == tag && bytes[record..record + 4] == key.to_le_bytes() {
                return Some(group + record);
            }
        }
        if bytes[..GROUP].contains(&EMPTY) {
            return None;
        }
    }
    None
}

/// Puts `key` in the first free slot on its probe path.
///
/// # Safety
/// `map` was validated on the thread that uses the menu's text.
unsafe fn insert(map: Map, key: u32, entry: usize) -> bool {
    let mask = map.groups as usize - 1;
    let home = (key >> 7) as usize & mask;
    for step in 0..map.groups as usize {
        let group = map.slots + ((home + step) & mask) * GROUP_BYTES;
        let Some(bytes) = read_memory(group, GROUP) else {
            return false;
        };
        if let Some(slot) = bytes.iter().position(|&t| t == EMPTY) {
            let record = (group + GROUP + slot * 16) as *mut u64;
            // SAFETY: a free slot inside the validated map. The record is
            // complete before its tag marks it used.
            unsafe {
                record.write_volatile(u64::from(key));
                record.add(1).write_volatile(entry as u64);
                ((group + slot) as *mut u8).write_volatile((key & 0x7f) as u8);
                ((map.at + 12) as *mut u32).write_volatile(map.count + 1);
            }
            return true;
        }
    }
    false
}

/// A text entry the title can read: its vtable, then the text.
fn owned_entry(vtable: usize, text: &'static str) -> usize {
    let text = std::ffi::CString::new(text).expect("menu text has no NUL");
    let text = Box::leak(text.into_boxed_c_str()).as_ptr() as usize;
    Box::leak(Box::new([vtable, text])).as_ptr() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A map of `groups` empty groups laid out as the title's, and its object.
    fn fake(groups: u32) -> (Vec<u8>, Box<[u64; 4]>) {
        let mut slots = vec![0u8; groups as usize * GROUP_BYTES];
        for g in 0..groups as usize {
            slots[g * GROUP_BYTES..g * GROUP_BYTES + GROUP].fill(EMPTY);
        }
        let object = Box::new([
            0,
            u64::from(groups),
            slots.as_ptr() as u64,
            u64::from(MAP_TYPE),
        ]);
        (slots, object)
    }

    #[test]
    fn an_added_key_is_found_and_a_full_group_sends_it_on() {
        let (mut slots, object) = fake(4);
        let map = map_at(object.as_ptr() as usize).expect("consistent map");
        // Fill home group 1 with other keys of the same home.
        for slot in 0..GROUP {
            let key = (1u32 << 7) | (slot as u32 + 1) << 9;
            slots[GROUP_BYTES + slot] = (key & 0x7f) as u8;
            let record = GROUP_BYTES + GROUP + slot * 16;
            slots[record..record + 4].copy_from_slice(&key.to_le_bytes());
        }
        let key = (1u32 << 7) | 0x55;
        assert_eq!(find(map, key), None);
        // SAFETY: the fake map lives through the test.
        assert!(unsafe { insert(map, key, 0x1234) });
        let record = find(map, key).expect("found after insert");
        assert_eq!(record, slots.as_ptr() as usize + 2 * GROUP_BYTES + GROUP);
        assert_eq!(object[1] >> 32, 1, "count went up by one");
        // The stock records in the full group are unchanged.
        assert!(find(map, (1 << 7) | 1 << 9).is_some());
    }

    #[test]
    fn a_map_of_another_type_is_refused() {
        let (_slots, mut object) = fake(4);
        object[3] = 0x1234_5678;
        assert!(map_at(object.as_ptr() as usize).is_none());
    }
}
