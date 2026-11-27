//! Debugging aid for the title's Settings menu (`VICTORY_MENU_TRACE=1`):
//! logs each section the title registers in it, then watches the menu's
//! controller object for changes, to find where rows are built and where a
//! changed value waits for Apply.
//!
//! Sections are registered by one function, found by a byte pattern, that
//! calls one other function (also checked by its pattern) once per section,
//! eight times. Those eight calls are sent to [`on_add_section`], which logs
//! and passes each on unchanged. Another build of the title, where either
//! pattern or the count does not match, is left alone.
//!
//! `VICTORY_MENU_TRACE_SPAN=<hex>` sets how many bytes of the controller are
//! watched (default 0x2000).
//!
//! `VICTORY_MENU_GRAPHICS=1` (with or without the trace) also adds the
//! title's Graphics section, which its data describes but this build never
//! registers, right after Display, through the same function and with the
//! hashes its other sections follow.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use victory_loader::{LoadedImage, Redirect};

mod menu_text;
mod native_list;
mod rows;
mod settings_list;
mod upscaling;

use crate::title_pattern::find_once;

/// The section registration function, from its start through its first
/// call that adds a section.
const REGISTER: &str = "56 57 48 83 EC 58 48 89 CE 48 81 C1 D8 0E 00 00 E8 ?? ?? ?? ?? \
                        E8 ?? ?? ?? ?? 48 8D 54 24 54 4C 8D 8E 00 0E 00 00 48 8D 8E 6C 1B 00 00 \
                        49 89 C0 C7 02 65 DA 65 B5 48 89 54 24 20 BA FF FF FF FF E8 ?? ?? ?? ?? \
                        48 8D 54 24 50 48 89 F1 41 B8 0D 3A 68 23 41 B9 53 91 E1 67 \
                        C7 44 24 20 0D 3A 68 23 C7 02 DF 3A 24 1B E8 ?? ?? ?? ??";
/// Bytes of the registration function, through its `ret`.
const REGISTER_SPAN: usize = 0x17b;
/// Where in it the first call that adds a section is.
const FIRST_ADD_CALL: usize = 0x67;
/// How many sections it adds.
const ADD_CALLS: usize = 8;

/// The start of the function that adds one section.
const ADD_SECTION: &str = "41 57 41 56 41 54 56 57 55 53 48 83 EC 40 48 8B 81 D8 0E 00 00 \
                           44 8B A4 24 A0 00 00 00 48 8D B9 D8 0E 00 00 49 89 CF 44 89 CE \
                           44 89 C3 49 89 D6 48 89 F9 FF 50 78 45 8B BF 6C 1B 00 00 B9 48 00 00 00";

/// The Display section (`joaat("display")`), after which Graphics goes.
const DISPLAY: u32 = 0x17d8_2a47;
/// The Graphics section (`joaat("graphics")`), its label and its screen, as
/// the title's own section and screen tables name them.
const GRAPHICS: u32 = 0xbc09_8ed9;
const GRAPHICS_LABEL: u32 = 0x7a62_ca2d;
const GRAPHICS_SCREEN: u32 = 0x9a46_cdb1;

/// How often the controller is compared with its last copy.
const POLL: Duration = Duration::from_millis(50);
/// Changes logged per poll, at most.
const MAX_CHANGES_PER_POLL: usize = 32;
/// After this many polls with a change, an offset is a counter or a timer:
/// said once, then no longer logged.
const NOISY_AFTER: u32 = 20;

/// The title's function that adds a section: controller, label hash,
/// section hash, screen hash, parent hash.
type AddSection = extern "win64" fn(usize, *const u32, u32, u32, u32) -> u64;

/// The function the redirected calls reached before.
static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
/// The controller the last section was added to.
static CONTROLLER: AtomicUsize = AtomicUsize::new(0);
/// Whether the Graphics section is added after Display.
static GRAPHICS_SECTION: AtomicBool = AtomicBool::new(false);
/// The title image's base, for the menu text table.
static IMAGE_BASE: AtomicUsize = AtomicUsize::new(0);

/// Sends the section registration's calls to [`on_add_section`] if
/// `VICTORY_MENU_TRACE` or `VICTORY_MENU_GRAPHICS` is set, and starts the
/// watcher with the first. Runs before the title's code and before any
/// diagnostics thread starts. An error means a call was written but its
/// page protection or instruction cache was not settled.
pub(crate) fn start(image: &LoadedImage, config: &victory_core::Config) -> Result<()> {
    let trace = victory_gpu::switch::on("VICTORY_MENU_TRACE");
    let graphics = victory_gpu::switch::on("VICTORY_MENU_GRAPHICS");
    if !trace && !graphics {
        return Ok(());
    }
    GRAPHICS_SECTION.store(graphics, Ordering::Release);
    settings_list::load(config);
    IMAGE_BASE.store(image.base(), Ordering::Release);
    let Some((calls, callee)) = locate(image) else {
        return Ok(());
    };
    if graphics
        && (victory_gpu::switch::on("VICTORY_MENU_ROW_TRACE")
            || victory_gpu::switch::on("VICTORY_MENU_UPSCALING_PROBE"))
    {
        rows::install(image)?;
    }
    // Before any call can reach the handler.
    ORIGINAL.store(image.base() + callee, Ordering::Release);
    let mut redirected = 0;
    for rva in calls {
        // SAFETY: a verified call site; no title code has run yet; the
        // handler is a function of this program with the callee's signature.
        match unsafe { image.redirect_call(rva, on_add_section as *const () as usize) }? {
            Redirect::Done { .. } => redirected += 1,
            Redirect::Refused(why) => {
                eprintln!("VIctory: menu trace: call at title+{rva:#x}: {why}")
            }
        }
    }
    eprintln!("VIctory: menu trace: {redirected} of {ADD_CALLS} section calls traced");
    if redirected > 0 && trace {
        let span = span_from_env();
        std::thread::Builder::new()
            .name("menu trace".into())
            .spawn(move || watch(span))?;
    }
    Ok(())
}

/// The rvas of the calls that add sections and of the function they call,
/// all checked; `None`, said why, otherwise.
fn locate(image: &LoadedImage) -> Option<(Vec<usize>, usize)> {
    let register = find_once(image, "settings section registration", REGISTER)?;
    if register + REGISTER_SPAN > image.info().size_of_image as usize {
        return None;
    }
    // SAFETY: inside the image's executable section the pattern matched
    // in; no title code has run.
    let code = unsafe {
        std::slice::from_raw_parts((image.base() + register) as *const u8, REGISTER_SPAN)
    };
    let Some(sites) = checked_sites(code) else {
        eprintln!("VIctory: menu trace: the section registration has another shape in this build");
        return None;
    };
    let callee = (register + FIRST_ADD_CALL + 5).wrapping_add_signed(rel32(code, FIRST_ADD_CALL));
    let add = find_once(image, "settings section add", ADD_SECTION)?;
    if add != callee {
        eprintln!("VIctory: menu trace: the section registration calls another function");
        return None;
    }
    Some((sites.into_iter().map(|at| register + at).collect(), callee))
}

/// The `rel32` of the call at `at` in `code`.
fn rel32(code: &[u8], at: usize) -> isize {
    i32::from_le_bytes([code[at + 1], code[at + 2], code[at + 3], code[at + 4]]) as isize
}

/// The offsets in `code` of every `call rel32` to the function the call at
/// `first` reaches (none if that is not a call).
fn call_sites(code: &[u8], first: usize) -> Vec<usize> {
    let target = |at: usize| (at + 5).wrapping_add_signed(rel32(code, at));
    if code.get(first) != Some(&0xE8) || first + 5 > code.len() {
        return Vec::new();
    }
    let wanted = target(first);
    (0..code.len().saturating_sub(4))
        .filter(|&at| code[at] == 0xE8 && target(at) == wanted)
        .collect()
}

/// [`call_sites`] of the registration, if there are exactly [`ADD_CALLS`].
fn checked_sites(code: &[u8]) -> Option<Vec<usize>> {
    let sites = call_sites(code, FIRST_ADD_CALL);
    (sites.len() == ADD_CALLS).then_some(sites)
}

/// Stands in for the title's [`AddSection`]: logs the section and adds it,
/// then, after Display and if asked, the Graphics section.
extern "win64" fn on_add_section(
    this: usize,
    label: *const u32,
    section: u32,
    screen: u32,
    parent: u32,
) -> u64 {
    // SAFETY: the title passes a pointer to the label hash (a local).
    let label_hash = (!label.is_null()).then(|| unsafe { label.read_unaligned() });
    let new_controller = CONTROLLER.swap(this, Ordering::AcqRel) != this;
    tracing::info!(
        target: "menu_trace",
        controller = %format_args!("{this:#x}"),
        label = %label_hash.map_or("-".into(), |h| format!("{h:08x}")),
        section = %format_args!("{section:08x}"),
        screen = %format_args!("{screen:08x}"),
        parent = %format_args!("{parent:08x}"),
        thread = ?std::thread::current().id(),
        "settings section added"
    );
    if new_controller {
        let stack = victory_loader::stack_here(24);
        tracing::info!(target: "menu_trace", stack = %stack.join(" < "), "sections added from");
    }
    let original = ORIGINAL.load(Ordering::Acquire);
    if original == 0 {
        return 0; // never: set before any call was redirected
    }
    // SAFETY: the title's function these calls reached before, with this
    // signature (checked by its pattern).
    let original: AddSection = unsafe { std::mem::transmute(original) };
    let added = original(this, label, section, screen, parent);
    if section == DISPLAY && GRAPHICS_SECTION.load(Ordering::Acquire) {
        tracing::info!(target: "menu_trace", controller = %format_args!("{this:#x}"), "graphics section added");
        // The tab bar is drawn from the sections, before any row is built:
        // give the tab its text now.
        // Only into a map already found (no scan here: it stalls the menu
        // thread, and the map is not loaded before Settings first opens).
        let tab = victory_core::settings::graphics::GRAPHICS_TAB;
        menu_text::provide_known(
            IMAGE_BASE.load(Ordering::Acquire),
            &[(tab.id(), tab.english())],
        );
        // The title passes the label by address too; a local lives long enough.
        let label = GRAPHICS_LABEL;
        original(this, &label, GRAPHICS, GRAPHICS_SCREEN, parent);
    }
    added
}

/// `VICTORY_MENU_TRACE_SPAN` (hex bytes), or 0x2000.
fn span_from_env() -> usize {
    std::env::var("VICTORY_MENU_TRACE_SPAN")
        .ok()
        .and_then(|s| usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
        .filter(|&s| s >= 8)
        .unwrap_or(0x2000)
}

/// Compares the controller with its last copy every [`POLL`] and logs
/// what changed, leaving out offsets that keep changing.
fn watch(span: usize) {
    let (mut watched, mut last) = (0usize, None::<Vec<u8>>);
    let mut changes_at: HashMap<usize, u32> = HashMap::new();
    loop {
        std::thread::sleep(POLL);
        let controller = CONTROLLER.load(Ordering::Acquire);
        if controller != watched {
            (watched, last) = (controller, None);
            changes_at.clear();
            tracing::info!(target: "menu_trace", controller = %format_args!("{controller:#x}"), span, "watching");
        }
        if watched == 0 {
            continue;
        }
        let Some(now) = victory_loader::read_memory(watched, span) else {
            last = None;
            continue;
        };
        if let Some(before) = &last {
            let mut logged = 0;
            for (offset, was, is) in changed_qwords(before, &now) {
                let n = changes_at.entry(offset).or_default();
                *n += 1;
                if *n == NOISY_AFTER {
                    tracing::info!(target: "menu_trace", offset = %format_args!("{offset:#x}"), "keeps changing; no longer logged");
                }
                if *n >= NOISY_AFTER || logged == MAX_CHANGES_PER_POLL {
                    continue;
                }
                logged += 1;
                tracing::info!(
                    target: "menu_trace",
                    offset = %format_args!("{offset:#x}"),
                    was = %format_args!("{was:#018x}"),
                    now = %format_args!("{is:#018x}"),
                    "controller changed"
                );
            }
        }
        last = Some(now);
    }
}

/// The qwords that differ between `old` and `new`: (offset, old, new).
fn changed_qwords(old: &[u8], new: &[u8]) -> Vec<(usize, u64, u64)> {
    let (old, new) = (old.as_chunks::<8>().0, new.as_chunks::<8>().0);
    old.iter()
        .zip(new)
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, (a, b))| (i * 8, u64::from_le_bytes(*a), u64::from_le_bytes(*b)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::title_pattern::parse_pattern;
    use std::sync::Mutex;

    const SITES: [usize; 8] = [0x67, 0x8f, 0xb2, 0xd5, 0xf8, 0x11b, 0x13e, 0x16e];

    /// A registration-sized buffer with calls to one target at `sites` and
    /// one call elsewhere to another.
    fn registration(sites: &[usize]) -> Vec<u8> {
        let mut code = vec![0x90; REGISTER_SPAN];
        let mut call = |at: usize, target: usize| {
            let rel = (target as i64 - (at as i64 + 5)) as i32;
            code[at] = 0xE8;
            code[at + 1..at + 5].copy_from_slice(&rel.to_le_bytes());
        };
        for &at in sites {
            call(at, 0x400);
        }
        call(0x20, 0x900);
        code
    }

    #[test]
    fn the_patterns_parse_to_their_lengths() {
        assert_eq!(parse_pattern(REGISTER).len(), 108);
        assert_eq!(parse_pattern(ADD_SECTION).len(), 66);
        // The registration pattern ends with the first section call.
        assert_eq!(parse_pattern(REGISTER).len(), FIRST_ADD_CALL + 5);
    }

    #[test]
    fn eight_calls_to_one_callee_are_found() {
        assert_eq!(call_sites(&registration(&SITES), FIRST_ADD_CALL), SITES);
        assert_eq!(
            checked_sites(&registration(&SITES)).as_deref(),
            Some(&SITES[..])
        );
    }

    #[test]
    fn sites_other_than_eight_are_refused() {
        assert_eq!(checked_sites(&registration(&SITES[..7])), None);
        assert!(call_sites(&[0x90; 0x80], FIRST_ADD_CALL).is_empty());
    }

    #[test]
    fn changed_qwords_reports_offsets() {
        let old = [0u8; 32];
        let mut new = old;
        new[0x10] = 7;
        assert_eq!(changed_qwords(&old, &new), [(0x10, 0, 7)]);
        assert!(changed_qwords(&old, &old).is_empty());
    }

    /// The arguments of a call to [`AddSection`], label read.
    type Call = (usize, u32, u32, u32, u32);

    static SEEN: Mutex<Vec<Call>> = Mutex::new(Vec::new());

    extern "win64" fn recorded(this: usize, label: *const u32, a: u32, b: u32, c: u32) -> u64 {
        // SAFETY: the handler passes a valid pointer.
        let label = unsafe { label.read() };
        SEEN.lock().unwrap().push((this, label, a, b, c));
        0xabc
    }

    fn calls() -> Vec<Call> {
        std::mem::take(&mut *SEEN.lock().unwrap())
    }

    /// One test: the handler's statics are shared.
    #[test]
    fn the_handler_forwards_and_adds_graphics_after_display() {
        ORIGINAL.store(recorded as *const () as usize, Ordering::Release);
        let label = 7u32;
        // Without the switch: passed on unchanged, result returned.
        GRAPHICS_SECTION.store(false, Ordering::Release);
        assert_eq!(on_add_section(1, &label, DISPLAY, 3, 4), 0xabc);
        assert_eq!(calls(), [(1, 7, DISPLAY, 3, 4)]);
        // With it: Graphics follows Display, under the same parent...
        GRAPHICS_SECTION.store(true, Ordering::Release);
        assert_eq!(on_add_section(1, &label, DISPLAY, 3, 4), 0xabc);
        assert_eq!(
            calls(),
            [
                (1, 7, DISPLAY, 3, 4),
                (1, GRAPHICS_LABEL, GRAPHICS, GRAPHICS_SCREEN, 4)
            ]
        );
        // ...and no other section.
        assert_eq!(on_add_section(1, &label, 2, 3, 4), 0xabc);
        assert_eq!(calls(), [(1, 7, 2, 3, 4)]);
        GRAPHICS_SECTION.store(false, Ordering::Release);
    }

    /// The section hashes are `joaat` of the names, as the title's are.
    #[test]
    fn section_hashes_are_joaat_of_their_names() {
        let joaat = |s: &str| {
            let mut h = 0u32;
            for b in s.bytes() {
                h = h.wrapping_add(u32::from(b.to_ascii_lowercase()));
                h = h.wrapping_add(h << 10);
                h ^= h >> 6;
            }
            h = h.wrapping_add(h << 3);
            h ^= h >> 11;
            h.wrapping_add(h << 15)
        };
        assert_eq!(joaat("display"), DISPLAY);
        assert_eq!(joaat("graphics"), GRAPHICS);
    }
}
