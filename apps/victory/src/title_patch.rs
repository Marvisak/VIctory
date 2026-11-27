//! Debugging aids that patch the title's code before it runs, for its
//! FidelityFX path on Series X (with `VICTORY_FIDELITYFX_DIR`):
//!
//! - `VICTORY_UPSCALER=<kind>` (decimal; 7 is AMD FidelityFX) makes the
//!   title's upscaler picker return `kind`. On Series X it picks the title's
//!   own upscaler.
//! - `VICTORY_FFX_SCALE=<scale>` sets the render scale of that path (0.5:
//!   1080p for a 4K output, 0.6667: 1440p). On Series X it is 1.0, so with
//!   FidelityFX forced on the title renders at full output size, more than
//!   its memory plan allows.
//!
//! No title address is kept here: each finds its code by a byte pattern
//! (call and RIP-relative offsets left open) that must match exactly once in
//! the title's executable sections. Another build of the title, where it does
//! not, is left alone. With both set, both are found before either is
//! written, and the scale goes first, since the picker alone renders at full
//! size.

use anyhow::{Result, bail};
use victory_core::{Config, config::GraphicsUpscaler};
use victory_loader::{CodeWrite, LoadedImage};

use crate::Console;
use crate::title_pattern::find_once;
#[cfg(test)]
use crate::title_pattern::{longest_fixed_run, matches, parse_pattern};

/// Locates the picker by its conditional selection between the title's
/// upscaler and FidelityFX. The call and global addresses may vary.
const PICKER: &str = "48 83 EC 28 80 3D ?? ?? ?? ?? 00 75 09 83 3D ?? ?? ?? ?? 00 74 28 \
                      E8 ?? ?? ?? ?? 8A 0D ?? ?? ?? ?? 80 E1 40 80 F9 01 B9 07 00 00 00 \
                      83 D9 00 84 C0 B8 06 00 00 00 0F 45 C1 48 83 C4 28 C3";

/// Locates the scale selection and the padding after the function that
/// [`patch_scale`] uses for the replacement constant.
const SCALE: &str = "41 56 56 57 53 48 83 EC 28 44 89 CB 4C 89 C6 48 89 D7 49 89 CE \
                     E8 ?? ?? ?? ?? 0F B6 C0 41 89 86 90 00 00 00 84 C0 75 0A \
                     C5 FA 10 05 ?? ?? ?? ?? EB 08 C5 FA 10 05 ?? ?? ?? ?? \
                     C5 F2 2A 54 24 70 C5 F2 2A CB C5 FA 59 C9 C5 FA 2C C1 89 07 \
                     C5 FA 59 C2 C5 FA 2C C0 89 06 48 83 C4 28 5B 5F 5E 41 5E C3 \
                     CC CC CC CC CC CC";

/// The bytes of the scale method [`patch_scale`] reads and writes: all of
/// [`SCALE`].
const SCALE_SPAN: usize = 0x68;

/// Applies the patches the environment asks for. Runs before the title's
/// code and before any diagnostics thread starts. An error stops the launch:
/// a write that went in but whose page protection or instruction cache could
/// not be settled, or only the scale taking of the two.
pub(crate) fn apply(image: &LoadedImage, config: &Config, console: Console) -> Result<()> {
    let mut kind = requested(
        "VICTORY_UPSCALER",
        "<kind> (decimal, 7 for FidelityFX)",
        |s| s.parse::<u32>().ok(),
    );
    let mut scale = requested("VICTORY_FFX_SCALE", "<scale in (0, 1]>", |s| {
        s.parse::<f32>().ok().filter(|s| *s > 0.0 && *s <= 1.0)
    });
    if kind.is_none()
        && scale.is_none()
        && matches!(console, Console::SeriesX)
        && config.graphics_upscaler == GraphicsUpscaler::AmdFsr
    {
        if config
            .fidelityfx_dir
            .as_deref()
            .is_some_and(|dir| dir.join("amd_fidelityfx_upscaler_dx12.dll").is_file())
        {
            kind = Some(Some(7));
            scale = Some(Some(0.5));
        } else {
            eprintln!(
                "VIctory: AMD FSR selected in [graphics]; the PC FidelityFX upscaler DLL is required"
            );
        }
    }
    // Everything asked for is found before anything is written.
    let picker = kind.map(|kind| kind.and_then(|kind| picker_patch(image, kind)));
    let scale = scale.map(|scale| scale.and_then(|scale| scale_patch(image, scale)));
    match (scale, picker) {
        (None, None) => {}
        (Some(scale), None) => {
            if let Some(scale) = scale {
                scale.write(image)?;
            }
        }
        (None, Some(picker)) => {
            if let Some(picker) = picker
                && picker.write(image)?
            {
                eprintln!(
                    "VIctory: without VICTORY_FFX_SCALE, Series X renders FidelityFX at full output size"
                );
            }
        }
        (Some(Some(scale)), Some(Some(picker))) => {
            if !scale.write(image)? {
                eprintln!("VIctory: upscaler picker left alone without its render scale");
            } else if !picker.write(image)? {
                bail!("only the FidelityFX render scale was patched, not the upscaler picker");
            }
        }
        (Some(_), Some(_)) => {
            eprintln!(
                "VIctory: neither FidelityFX patch applied: VICTORY_UPSCALER and VICTORY_FFX_SCALE go together"
            );
        }
    }
    Ok(())
}

/// Switch `name`: `None` if unset, `Some(None)` (said why) if malformed.
fn requested<T>(name: &str, form: &str, parse: impl Fn(&str) -> Option<T>) -> Option<Option<T>> {
    let spec = std::env::var(name).ok()?;
    let value = parse(spec.trim());
    if value.is_none() {
        eprintln!("VIctory: {name}={spec}: expected {form}");
    }
    Some(value)
}

/// Code to write at title+`rva`, found and checked.
struct Patch {
    what: String,
    rva: usize,
    bytes: Vec<u8>,
}

impl Patch {
    /// Writes it. False, said why, if nothing was changed.
    fn write(&self, image: &LoadedImage) -> Result<bool> {
        let at = format!("{} at title+{:#x}", self.what, self.rva);
        // SAFETY: `apply` runs before the title's code and before any
        // diagnostics thread that could read the image.
        match unsafe { image.write_code(self.rva, &self.bytes) } {
            Ok(CodeWrite::Done) => {
                eprintln!("VIctory: patched {at}");
                Ok(true)
            }
            Ok(CodeWrite::Refused(why)) => {
                eprintln!("VIctory: {at} not patched: {why}");
                Ok(false)
            }
            Err(e) => bail!("{at}: {e}"),
        }
    }
}

fn picker_patch(image: &LoadedImage, kind: u32) -> Option<Patch> {
    let rva = find_once(image, "upscaler picker", PICKER)?;
    Some(Patch {
        what: format!("upscaler picker (returns {kind})"),
        rva,
        bytes: picker_stub(kind).to_vec(),
    })
}

fn scale_patch(image: &LoadedImage, scale: f32) -> Option<Patch> {
    let rva = find_once(image, "FidelityFX render scale", SCALE)?;
    // SAFETY: a match of `SCALE` (`SCALE_SPAN` bytes) in an executable
    // section of the mapped image, read before any title code runs.
    let mut bytes =
        unsafe { std::slice::from_raw_parts((image.base() + rva) as *const u8, SCALE_SPAN) }
            .to_vec();
    if !patch_scale(&mut bytes, scale) {
        eprintln!(
            "VIctory: FidelityFX render scale at title+{rva:#x} not patched: unexpected code"
        );
        return None;
    }
    Some(Patch {
        what: format!("FidelityFX render scale ({scale})"),
        rva,
        bytes,
    })
}

/// `mov eax, kind; ret`.
fn picker_stub(kind: u32) -> [u8; 6] {
    let mut stub = [0xB8, 0, 0, 0, 0, 0xC3];
    stub[1..5].copy_from_slice(&kind.to_le_bytes());
    stub
}

/// Points both scale loads of the method `code` starts with
/// (`vmovss xmm0, [rip+d]` at +0x28 and +0x32) at `scale`, written into the
/// method's `int3` padding at +0x64. False, with nothing written, for other
/// code.
fn patch_scale(code: &mut [u8], scale: f32) -> bool {
    const LOADS: [usize; 2] = [0x28, 0x32];
    const SLOT: usize = 0x64;
    const VMOVSS_RIP: [u8; 4] = [0xC5, 0xFA, 0x10, 0x05];
    if code.len() < SCALE_SPAN
        || LOADS.iter().any(|&l| code[l..l + 4] != VMOVSS_RIP)
        || code[SLOT..SLOT + 4] != [0xCC; 4]
    {
        return false;
    }
    code[SLOT..SLOT + 4].copy_from_slice(&scale.to_le_bytes());
    for l in LOADS {
        let disp = (SLOT - (l + 8)) as i32;
        code[l + 4..l + 8].copy_from_slice(&disp.to_le_bytes());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pattern` with its open bytes filled with `fill`.
    fn instance(pattern: &str, fill: u8) -> Vec<u8> {
        parse_pattern(pattern)
            .into_iter()
            .map(|p| p.unwrap_or(fill))
            .collect()
    }

    #[test]
    fn the_patterns_parse_to_the_spans_patched() {
        assert_eq!(parse_pattern(PICKER).len(), 62);
        assert!(parse_pattern(PICKER).len() >= picker_stub(7).len());
        assert_eq!(parse_pattern(SCALE).len(), SCALE_SPAN);
    }

    #[test]
    fn the_scale_pattern_is_what_the_scale_patch_expects() {
        let mut code = instance(SCALE, 0x11);
        assert!(patch_scale(&mut code, 0.5));
    }

    #[test]
    fn a_pattern_is_found_with_any_open_bytes() {
        let mut code = vec![0x90u8; 0x400];
        code[0x123..0x123 + 62].copy_from_slice(&instance(PICKER, 0xAB));
        assert_eq!(matches(&code, &parse_pattern(PICKER)), [0x123]);
        // A changed fixed byte is not a match.
        code[0x123 + 0x28] = 0x08; // mov ecx, 8
        assert!(matches(&code, &parse_pattern(PICKER)).is_empty());
    }

    #[test]
    fn every_match_is_counted() {
        let one = instance(SCALE, 0);
        let code = [one.clone(), vec![0x90; 3], one].concat();
        assert_eq!(matches(&code, &parse_pattern(SCALE)), [0, SCALE_SPAN + 3]);
    }

    #[test]
    fn a_match_cut_off_at_either_end_is_not_one() {
        let pattern = parse_pattern(PICKER);
        let whole = instance(PICKER, 0);
        assert!(matches(&whole[1..], &pattern).is_empty());
        assert!(matches(&whole[..whole.len() - 1], &pattern).is_empty());
    }

    #[test]
    fn the_longest_fixed_run_anchors_the_search() {
        let (at, run) = longest_fixed_run(&parse_pattern("01 ?? 02 03 ?? 04 05 06 ??"));
        assert_eq!((at, run), (5, vec![4, 5, 6]));
    }

    #[test]
    fn the_scale_loads_point_at_the_constant() {
        // Synthetic code: the two loads, filler, and int3 padding.
        let mut code = vec![0x90u8; SCALE_SPAN];
        for at in [0x28, 0x32] {
            code[at..at + 8].copy_from_slice(&[0xC5, 0xFA, 0x10, 0x05, 1, 2, 3, 4]);
        }
        code[0x60..].fill(0xCC);
        assert!(patch_scale(&mut code, 0.5));
        assert_eq!(code[0x64..0x68], 0.5f32.to_le_bytes());
        // Each load's next instruction plus its displacement is the constant.
        for at in [0x28, 0x32] {
            let disp = i32::from_le_bytes(code[at + 4..at + 8].try_into().unwrap());
            assert_eq!(at as i32 + 8 + disp, 0x64);
        }
        // Already patched, so no padding left: nothing is written again.
        let before = code.clone();
        assert!(!patch_scale(&mut code, 0.25));
        assert_eq!(code, before);
    }

    #[test]
    fn other_code_is_left_alone() {
        let mut code = vec![0xCCu8; SCALE_SPAN];
        assert!(!patch_scale(&mut code, 0.5));
        assert!(code.iter().all(|&b| b == 0xCC));
    }

    #[test]
    fn the_picker_returns_the_kind() {
        assert_eq!(picker_stub(7), [0xB8, 7, 0, 0, 0, 0xC3]);
    }
}
