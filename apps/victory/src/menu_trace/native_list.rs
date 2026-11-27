//! The title's native list row (◀ ▶), driven by a VIctory [`ListSetting`].
//!
//! This is the only place that touches the title's row objects. The title
//! makes a list row from a pooled row: cleared fields, a setting descriptor,
//! then the option type's function table. We do the same to a pooled row
//! the title just appended, but with an empty descriptor (so the title never
//! reflects over a setting of its own) and a copy of the quality list's
//! function table in which four slots are ours:
//!
//! | slot | title's role | ours                                        |
//! |------|--------------|---------------------------------------------|
//! | 7    | refresh      | range 0..=last, value = the chosen option   |
//! | 8    | enabled      | always enabled                              |
//! | 9    | publish      | send enabled, visible, value, index, range  |
//! | 11   | commit       | keep and save the player's pick             |
//!
//! Slot 2 (the row's class) stays the quality list's, which is what makes
//! the UI draw a ◀ ▶ row, and also why its option names show for now.

use victory_loader::LoadedImage;

use super::settings_list::{ListSetting, Rows};
use crate::title_pattern::find_once;

/// The quality list's builder case, from its init call through its option
/// type hash: the function tables and defaults every list row gets.
const LIST_CASE: &str = "48 89 F9 48 89 F2 E8 ?? ?? ?? ?? C5 F8 28 05 ?? ?? ?? ?? \
                         48 8D 05 ?? ?? ?? ?? 48 89 07 48 8D 05 ?? ?? ?? ?? 48 89 47 08 \
                         48 8D 05 ?? ?? ?? ?? 48 89 47 10 48 8D 05 ?? ?? ?? ?? 48 89 47 60 \
                         C5 F8 11 47 68 48 C7 47 78 00 00 00 00 C7 47 58 78 2A 1C FF";
/// Sends one int of a row to the UI: (row field, value, ui, key).
const PUBLISH_INT: &str = "48 83 EC 38 4C 89 C0 4C 8B 05 ?? ?? ?? ?? 89 54 24 34 4D 85 C0 74 12 \
                           44 89 4C 24 20 48 8D 54 24 34 49 89 C1 E8 ?? ?? ?? ?? 90 48 83 C4 38 C3";
/// Sends one flag of a row to the UI: (row field, value, ui, key).
const PUBLISH_FLAG: &str = "48 83 EC 38 4C 89 C0 4C 8B 05 ?? ?? ?? ?? 88 54 24 37 4D 85 C0 74 12 \
                            44 89 4C 24 20 48 8D 54 24 37 49 89 C1 E8 ?? ?? ?? ?? 90 48 83 C4 38 C3";
/// Posts the title's "settings changed" event for a setting context, which
/// makes the menu publish its rows again: what the stock commit calls.
const NOTIFY_CHANGED: &str = "56 57 48 83 EC 48 48 8D 7C 24 30 48 89 CE 48 8D 54 24 2C 48 89 F9                               C7 02 17 7E 41 72 E8 ?? ?? ?? ?? 48 8D 05 ?? ?? ?? ?? 48 89 07 48 89 77 10";

/// The quality list's option type hash, kept with its class.
const OPTION_TYPE: u32 = 0xff1c_2a78;
/// The row kind word the title gives list rows (toggles and sliders differ).
const LIST_KIND: u32 = 0xe870_16bd;

/// UI keys the stock list row publishes under.
mod key {
    pub(super) const ENABLED: u32 = 0x654f_7df1;
    pub(super) const VISIBLE: u32 = 0x769b_5cd3;
    pub(super) const VALUE: u32 = 0xf4e5_3011;
    pub(super) const INDEX: u32 = 0x3dbc_f2f4;
    pub(super) const RANGE: u32 = 0x3790_f4f2;
}

/// Row fields, from the stock list row's init, refresh and publish.
mod field {
    pub(super) const SECOND_TABLE: usize = 0x08;
    pub(super) const THIRD_TABLE: usize = 0x10;
    pub(super) const CLEARED: std::ops::Range<usize> = 0x18..0x3b;
    pub(super) const NAME: usize = 0x1c;
    pub(super) const HELP: usize = 0x20;
    pub(super) const KIND: usize = 0x34;
    pub(super) const STATE: usize = 0x38;
    pub(super) const VISIBLE: usize = 0x39;
    pub(super) const EDITABLE: usize = 0x3a;
    pub(super) const DESCRIPTOR: [usize; 3] = [0x40, 0x48, 0x50];
    pub(super) const OPTION_TYPE: usize = 0x58;
    /// Sub-object whose table holds the deleter and commit (slots 10, 11).
    pub(super) const COMMIT_SIDE: usize = 0x60;
    pub(super) const VALUE: usize = 0x68;
    pub(super) const MIN: usize = 0x6c;
    pub(super) const MAX: usize = 0x70;
    pub(super) const STEP: usize = 0x74;
    pub(super) const INDEX: usize = 0x78;
    pub(super) const RANGE: usize = 0x7c;
}

/// Function-table slots we replace (see the module docs).
mod slot {
    pub(super) const REFRESH: usize = 7;
    pub(super) const ENABLED: usize = 8;
    pub(super) const PUBLISH: usize = 9;
    pub(super) const COMMIT_SIDE: usize = 10;
    pub(super) const COMMIT: usize = 11;
}

type Publish = unsafe extern "win64" fn(usize, u32, usize, u32);
type Notify = unsafe extern "win64" fn(usize);

/// What every native list row needs from the title, found once.
pub(super) struct NativeList {
    publish_int: Publish,
    publish_flag: Publish,
    /// Optional: without it the counter catches up only when the tab is
    /// shown again.
    notify: Option<Notify>,
    /// The quality list's table with our slots: 12 entries.
    table: &'static [usize; 12],
    second: usize,
    third: usize,
    defaults: [u8; 16],
}

static NATIVE: std::sync::OnceLock<NativeList> = std::sync::OnceLock::new();
static ROWS: Rows = Rows::new();

impl NativeList {
    /// Finds the title's list row parts; None (said why) in another build.
    pub(super) fn locate(image: &LoadedImage) -> Option<&'static Self> {
        let case = find_once(image, "native list builder", LIST_CASE)?;
        let publish_int = find_once(image, "native row int publisher", PUBLISH_INT)?;
        let publish_flag = find_once(image, "native row flag publisher", PUBLISH_FLAG)?;
        let notify = find_once(image, "settings changed notifier", NOTIFY_CHANGED);
        let base = image.base();
        let at = base + case;
        // SAFETY: the pattern matched these instructions; each rel32 is read
        // from inside the matched bytes.
        let (defaults, table, second, third, side) = unsafe {
            (
                rel(at, 15, 19),
                rel(at, 22, 26),
                rel(at, 32, 36),
                rel(at, 43, 47),
                rel(at, 54, 58),
            )
        };
        // Our copy serves the commit side from its slot 10: only valid if the
        // title's commit-side table is the primary table's slot 10, and all
        // we read lies inside the image.
        let end = base + image.info().size_of_image as usize;
        if side != table + slot::COMMIT_SIDE * 8 || table < base || table + 12 * 8 > end || defaults + 16 > end {
            eprintln!("VIctory: native list rows not installed: the list row tables have another layout");
            return None;
        }
        // SAFETY: checked just above: the quality list's 12-entry function
        // table and its 16 default bytes, inside the mapped image.
        let (mut copy, defaults) = unsafe {
            (
                std::ptr::read_unaligned(table as *const [usize; 12]),
                std::ptr::read_unaligned(defaults as *const [u8; 16]),
            )
        };
        copy[slot::REFRESH] = refresh as *const () as usize;
        copy[slot::ENABLED] = enabled as *const () as usize;
        copy[slot::PUBLISH] = publish as *const () as usize;
        copy[slot::COMMIT] = commit as *const () as usize;
        // SAFETY: both publishers matched their patterns, whose signature is
        // (row field, value, ui, key).
        let native = unsafe {
            Self {
                publish_int: std::mem::transmute::<usize, Publish>(base + publish_int),
                publish_flag: std::mem::transmute::<usize, Publish>(base + publish_flag),
                notify: notify.map(|rva| std::mem::transmute::<usize, Notify>(base + rva)),
                table: Box::leak(Box::new(copy)),
                second,
                third,
                defaults,
            }
        };
        tracing::info!(target: "menu_trace", table = %format_args!("title+{:#x}", table - base),
                       "native list row located");
        NATIVE.set(native).ok()?;
        NATIVE.get()
    }

    /// Turns the pooled `row` the title just appended into a list row for
    /// `setting`.
    ///
    /// # Safety
    /// `row` is a live row from the title's settings row pool, on the menu
    /// thread, not yet shown.
    pub(super) unsafe fn adopt(&self, row: usize, setting: &'static ListSetting) {
        ROWS.bind(row, setting);
        // SAFETY: the caller's row; these stores mirror the title's init
        // and list builder, minus the descriptor (left empty on purpose).
        unsafe {
            std::ptr::write_bytes(
                (row + field::CLEARED.start) as *mut u8,
                0,
                field::CLEARED.len(),
            );
            for off in field::DESCRIPTOR {
                put(row, off, 0usize);
            }
            // The whole 8-byte word: the type hash is written below, the
            // 4 bytes after it stay 0 as the title's init leaves them.
            put(row, field::OPTION_TYPE, 0usize);
            put(row, field::EDITABLE, 1u8);
            put(row, 0, self.table.as_ptr() as usize);
            put(row, field::SECOND_TABLE, self.second);
            put(row, field::THIRD_TABLE, self.third);
            put(
                row,
                field::COMMIT_SIDE,
                self.table.as_ptr().add(slot::COMMIT_SIDE) as usize,
            );
            put(row, field::VALUE, self.defaults);
            put(row, field::INDEX, 0usize);
            put(row, field::OPTION_TYPE, OPTION_TYPE);
            put(row, field::NAME, setting.name().id());
            put(row, field::HELP, setting.help().id());
            put(row, field::KIND, LIST_KIND);
        }
        // The builder's defaults leave max = min, so ◀ ▶ could not move;
        // the title refreshes each row it builds, and so do we.
        refresh(row);
    }
}

/// The target of a rel32 at `at + disp`, relative to `at + next`.
///
/// # Safety
/// `at + disp` is readable (inside matched code).
unsafe fn rel(at: usize, disp: usize, next: usize) -> usize {
    // SAFETY: the caller's guarantee.
    let d = unsafe { std::ptr::read_unaligned((at + disp) as *const i32) };
    (at + next).wrapping_add_signed(d as isize)
}

/// Stores `value` at `row + off`.
///
/// # Safety
/// The caller owns the row's memory for the store.
unsafe fn put<T>(row: usize, off: usize, value: T) {
    // SAFETY: the caller's guarantee.
    unsafe { std::ptr::write_unaligned((row + off) as *mut T, value) }
}

/// Reads the i32 at `row + off`.
///
/// # Safety
/// As [`put`].
unsafe fn get_i32(row: usize, off: usize) -> i32 {
    // SAFETY: the caller's guarantee.
    unsafe { std::ptr::read_unaligned((row + off) as *const i32) }
}

/// Reads the pointer-sized value at `row + off`.
///
/// # Safety
/// As [`put`].
unsafe fn get_usize(row: usize, off: usize) -> usize {
    // SAFETY: the caller's guarantee.
    unsafe { std::ptr::read_unaligned((row + off) as *const usize) }
}

/// Slot 7: the row's range and value, from its setting.
extern "win64" fn refresh(row: usize) {
    let Some(bound) = ROWS.get(row) else { return };
    let (last, chosen) = (bound.setting.last() as u32, bound.setting.chosen() as u32);
    // SAFETY: the title calls this with its live row on the menu thread.
    unsafe {
        put(row, field::MIN, 0u32);
        put(row, field::MAX, last);
        put(row, field::STEP, 1u32);
        put(row, field::VALUE, chosen);
        put(row, field::INDEX, chosen);
        put(row, field::RANGE, last);
    }
    tracing::debug!(target: "menu_trace", row = %format_args!("{row:#x}"), chosen, last, "list row refresh");
}

/// Slot 8: VIctory's rows are always enabled.
extern "win64" fn enabled(row: usize) -> bool {
    // SAFETY: live row, menu thread.
    unsafe { put(row, field::STATE, 0u8) };
    true
}

/// Slot 9: sends the row's state to the UI context `ui`.
///
/// `ui` lives on the caller's stack: valid during this call only. Never
/// keep it (publishing to it later wrote through a dangling pointer).
extern "win64" fn publish(row: usize, ui: usize) {
    let (Some(native), Some(bound)) = (NATIVE.get(), ROWS.get(row)) else {
        return;
    };
    let (last, chosen) = (bound.setting.last() as u32, bound.setting.chosen() as u32);
    // SAFETY: live row and UI context from the title; each publisher takes
    // the row field it caches into, as for the stock row.
    unsafe {
        (native.publish_flag)(row + field::EDITABLE, 1, ui, key::ENABLED);
        (native.publish_flag)(row + field::VISIBLE, 1, ui, key::VISIBLE);
        (native.publish_int)(row + field::VALUE, chosen, ui, key::VALUE);
        (native.publish_int)(row + field::INDEX, chosen, ui, key::INDEX);
        (native.publish_int)(row + field::RANGE, last, ui, key::RANGE);
    }
    tracing::debug!(target: "menu_trace", row = %format_args!("{row:#x}"), ui = %format_args!("{ui:#x}"), chosen, "list row publish");
}

/// Slot 11, called through the commit side at row+0x60 after ◀ or ▶: keep
/// and save the pick.
///
/// The option text follows at once (the UI draws it); the `n / count`
/// counter changes when the title publishes the row again, which the stock
/// commit triggers by posting "settings changed" for the row's setting
/// context (row+0x50). Ours is empty (0), and we post it the same way.
extern "win64" fn commit(side: usize) {
    let row = side - field::COMMIT_SIDE;
    let Some(bound) = ROWS.get(row) else { return };
    // SAFETY: live row, menu thread.
    let raw = unsafe { get_i32(row, field::VALUE) };
    tracing::debug!(target: "menu_trace", row = %format_args!("{row:#x}"), raw, "list row commit");
    if let Err(e) = bound.setting.choose(raw) {
        // The old choice stays; the row shows it again on the next publish.
        tracing::warn!(target: "menu_trace", raw, error = %e, "menu choice not saved, kept the old one");
    }
    if let Some(notify) = NATIVE.get().and_then(|n| n.notify) {
        // SAFETY: live row; the title's notifier takes the row's setting
        // context, read from the row as the stock commit does.
        unsafe { notify(get_usize(row, field::DESCRIPTOR[2])) };
        tracing::debug!(target: "menu_trace", row = %format_args!("{row:#x}"), "list row change posted");
    }
}
