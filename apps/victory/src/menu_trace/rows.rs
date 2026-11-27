//! Compare the row metadata created for the stock Display tab and the hidden Graphics tab.

use std::cell::Cell;
use std::sync::OnceLock;

use anyhow::Result;
use victory_loader::{LoadedImage, Redirect, read_memory};

use crate::title_pattern::find_once;

use super::GRAPHICS_LABEL;
use super::upscaling::UpscalingSection;

const DISPLAY_LABEL: u32 = 0x8cb1_18f1;
const VISIT_SITE: &str = "48 8D 54 24 40 49 89 73 18 4C 89 1A 4C 89 4A 08 \
                          4C 8D 0D ?? ?? ?? ?? 4C 89 52 10 4C 89 4A 18 E8 ?? ?? ?? ?? \
                          48 8D 54 24 34";
const VISIT_CALL: usize = 31;
const VISIT_ENTRY: &str = "48 83 EC 58 48 8D 44 24 28 4C 8D 0D ?? ?? ?? ?? \
                           48 89 10 48 8D 54 24 38 4C 89 40 08 4C 8D 05 2D 00 00 00";
const MAKE_SITE: &str = "C5 F8 29 01 48 8B 43 10 48 89 41 10 E8 ?? ?? ?? ?? 48 85 C0 74 ??";
const MAKE_CALL: usize = 12;
const MAKE_ENTRY: &str = "56 57 53 48 83 EC 20 48 8B 01 48 89 CE 48 85 C0 74 ?? \
                           48 8B 40 08 48 8B 08";
const MAX_ROWS: u32 = 96;

type Visit = extern "win64" fn(usize, usize, usize);
type Make = extern "win64" fn(usize) -> usize;

struct Hooks {
    visit: Visit,
    make: Make,
    image_base: usize,
    image_size: usize,
    upscaling: Option<UpscalingSection>,
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();

#[derive(Clone, Copy)]
struct VisitState {
    label: u32,
    made: u32,
    /// Name ID of the last Graphics row that had one.
    last_named: u32,
    /// Nameless rows built since that one.
    unnamed_run: u8,
}

impl VisitState {
    const fn new(label: u32) -> Self {
        Self {
            label,
            made: 0,
            last_named: 0,
            unnamed_run: 0,
        }
    }
}

/// Whether the text census has run.
static CENSUS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

thread_local! {
    /// The stock Graphics rows' name and help IDs, gathered during a visit.
    static GRAPHICS_TEXT: std::cell::RefCell<Vec<u32>> = const { std::cell::RefCell::new(Vec::new()) };
    static VISIT: Cell<VisitState> = const { Cell::new(VisitState::new(0)) };
}

pub(super) fn install(image: &LoadedImage) -> Result<()> {
    let Some((visit_site, visit)) = locate_call(
        image,
        "settings row visitor",
        VISIT_SITE,
        VISIT_CALL,
        VISIT_ENTRY,
    ) else {
        return Ok(());
    };
    let Some((make_site, make)) = locate_call(
        image,
        "settings row factory",
        MAKE_SITE,
        MAKE_CALL,
        MAKE_ENTRY,
    ) else {
        return Ok(());
    };
    let image_base = image.base();
    // SAFETY: both original callees and the argument setup at their call sites
    // were identified above. The loaded image is live for the title's lifetime.
    let hooks = unsafe {
        Hooks {
            visit: std::mem::transmute::<usize, Visit>(image_base + visit),
            make: std::mem::transmute::<usize, Make>(image_base + make),
            image_base,
            image_size: image.info().size_of_image as usize,
            upscaling: UpscalingSection::locate(image),
        }
    };
    if HOOKS.set(hooks).is_err() {
        return Ok(());
    }
    // Install the observer first so a refused visit hook leaves it inactive.
    for (site, handler) in [
        (make_site, make_row as *const () as usize),
        (visit_site, visit_rows as *const () as usize),
    ] {
        // SAFETY: the site is a verified call and no title thread has started.
        if let Redirect::Refused(why) = unsafe { image.redirect_call(site, handler) }? {
            eprintln!("VIctory: menu row trace: call at title+{site:#x}: {why}");
            return Ok(());
        }
    }
    eprintln!("VIctory: menu row trace: Display and Graphics row construction armed");
    Ok(())
}

fn locate_call(
    image: &LoadedImage,
    name: &str,
    site_pattern: &str,
    call_offset: usize,
    entry_pattern: &str,
) -> Option<(usize, usize)> {
    let call = find_once(image, name, site_pattern)? + call_offset;
    let entry = find_once(image, name, entry_pattern)?;
    let code = read_memory(image.base() + call, 5)?;
    if code[0] != 0xe8 || (call + 5).checked_add_signed(i32_at(&code, 1)? as isize) != Some(entry) {
        eprintln!("VIctory: menu row trace: {name} calls a different function");
        return None;
    }
    Some((call, entry))
}

extern "win64" fn visit_rows(source: usize, visitor: usize, after: usize) {
    let Some(hooks) = HOOKS.get() else { return };
    // The visitor descriptor starts with its context pointer. The caller
    // writes the selected tab's ID to the context before calling the visitor.
    let context = read_memory(visitor, 8)
        .and_then(|p| usize_at(&p, 0))
        .and_then(|context| read_memory(context, 0x20));
    let label = context
        .as_deref()
        .and_then(|bytes| i32_at(bytes, 0x10))
        .unwrap_or_default() as u32;
    if label == GRAPHICS_LABEL {
        GRAPHICS_TEXT.with_borrow_mut(Vec::clear);
    }
    // Every visit: the tab bar needs the Graphics name before any tab is
    // built. The search is time-boxed (see `menu_text`), so a visit before
    // the text map is loaded costs little and the next one tries again.
    if let Some(probe) = &hooks.upscaling {
        probe.provide_text(hooks.image_base);
    }
    let previous = VISIT.replace(VisitState::new(label));
    (hooks.visit)(source, visitor, after);
    let current = VISIT.replace(previous);
    if label == GRAPHICS_LABEL
        && current.made != 0
        && let Some(probe) = &hooks.upscaling
        && let Some(owner) = context.as_deref().and_then(|bytes| usize_at(bytes, 0x18))
        && owner != 0
    {
        // The census sweeps every loaded text map: diagnostics only, it
        // stalls the menu thread while Settings opens.
        if victory_gpu::switch::on("VICTORY_MENU_TEXT_CENSUS")
            && !CENSUS.swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            let mut keys = GRAPHICS_TEXT.with_borrow(Clone::clone);
            keys.sort_unstable();
            keys.dedup();
            super::menu_text::census(&keys);
        }
        // SAFETY: this is the live menu for the tab being built at the
        // verified call, after the stock row traversal and before flattening.
        unsafe { probe.append(owner) };
    }
    if matches!(current.label, DISPLAY_LABEL | GRAPHICS_LABEL) {
        tracing::info!(target: "menu_trace", tab = %format_args!("{:08x}", current.label),
                       rows = current.made, "native settings rows constructed");
    }
}

extern "win64" fn make_row(descriptor: usize) -> usize {
    let Some(hooks) = HOOKS.get() else { return 0 };
    let row = (hooks.make)(descriptor);
    let mut state = VISIT.get();
    if row == 0 || !matches!(state.label, DISPLAY_LABEL | GRAPHICS_LABEL) {
        return row;
    }
    state.made += 1;
    if state.label == GRAPHICS_LABEL {
        name_unnamed_row(row, &mut state);
    }
    VISIT.set(state);
    if state.made <= MAX_ROWS {
        record_row(hooks, state, row);
    }
    row
}

fn record_row(hooks: &Hooks, state: VisitState, row: usize) {
    let Some(bytes) = read_memory(row, 0x38) else {
        return;
    };
    let vtable_rva = usize_at(&bytes, 0)
        .and_then(|vtable| vtable.checked_sub(hooks.image_base))
        .filter(|&rva| rva < hooks.image_size);
    let Some(vtable_rva) = vtable_rva else { return };
    let mut fields = [0; 8];
    for (field, bytes) in fields.iter_mut().zip(bytes[0x18..].as_chunks::<4>().0) {
        *field = u32::from_le_bytes(*bytes);
    }
    if state.label == GRAPHICS_LABEL {
        GRAPHICS_TEXT.with_borrow_mut(|keys| keys.extend(fields[1..3].iter().filter(|&&k| k != 0)));
    }
    tracing::info!(target: "menu_trace", tab = %format_args!("{:08x}", state.label),
                   number = state.made, vtable = %format_args!("title+{vtable_rva:#x}"),
                   metadata = %format_args!("{fields:08x?}"), "native settings row metadata");
}

/// Gives a stock Graphics row that has no name ID (so no text can be found
/// for it) VIctory's IDs, recognised by the named row before it.
fn name_unnamed_row(row: usize, state: &mut VisitState) {
    use victory_core::settings::stock_labels::{unnamed, unnamed_for_locale};
    const NAME: usize = 0x1c;
    const HELP: usize = 0x20;
    let Some(ids) = read_memory(row + NAME, 8) else {
        return;
    };
    let (name, help) = (
        u32::from_le_bytes(ids[..4].try_into().unwrap()),
        u32::from_le_bytes(ids[4..].try_into().unwrap()),
    );
    if name != 0 {
        (state.last_named, state.unnamed_run) = (name, 0);
        if help == 0 {
            give_extra_help(row, name);
        }
        return;
    }
    state.unnamed_run = state.unnamed_run.saturating_add(1);
    let locale = std::env::var("VICTORY_LOCALE").unwrap_or_default();
    let Some(entry) = unnamed(
        unnamed_for_locale(&locale),
        state.last_named,
        state.unnamed_run,
    ) else {
        return;
    };
    // SAFETY: the row the title's factory just returned, on the menu thread,
    // before the caller sees it; the same fields its own init writes.
    unsafe {
        std::ptr::write_unaligned((row + NAME) as *mut u32, entry.name.id());
        if help == 0 {
            std::ptr::write_unaligned((row + HELP) as *mut u32, entry.help.id());
        }
    }
    tracing::debug!(target: "menu_trace", after = %format_args!("{:08x}", state.last_named),
                    nth = state.unnamed_run, name = entry.name.english(), "nameless row named");
}

/// Gives a named stock row without a help ID VIctory's help, if it has one.
fn give_extra_help(row: usize, name: u32) {
    use victory_core::settings::stock_labels::extra_help_for_locale;
    let locale = std::env::var("VICTORY_LOCALE").unwrap_or_default();
    if let Some(extra) = extra_help_for_locale(&locale)
        .iter()
        .find(|e| e.row == name)
    {
        // SAFETY: as in `name_unnamed_row`: the factory's fresh row, its
        // help field, on the menu thread.
        unsafe { std::ptr::write_unaligned((row + 0x20) as *mut u32, extra.help.id()) };
    }
}

fn i32_at(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn usize_at(bytes: &[u8], offset: usize) -> Option<usize> {
    Some(usize::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
}
