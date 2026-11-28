//! PC-style display settings for a title that stores them in `settings.xml`.
//!
//! The console output mode alone does not resize the title's render
//! targets. Grand Theft Auto VI writes `uiResolution` and
//! `resolutionFullscreen` into its persistent settings, and those stayed at
//! 3840x2160 after `VICTORY_OUTPUT_SIZE=1280x720`. This module patches that
//! file, and the quality fields around it, before the title starts.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Quality {
    Low,
    Medium,
    High,
    Extreme,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Aa {
    Off,
    Msaa,
}

/// What `victory run` / `victory graphics` was asked to change.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub resolution: Option<String>,
    pub quality: Option<Quality>,
    pub aa: Option<Aa>,
    pub anisotropic: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Effects {
    disable_shadows: Option<bool>,
    ray_tracing: Option<bool>,
    motion_blur: Option<bool>,
    reflections: Option<bool>,
    vrs: Option<bool>,
    vsync: Option<bool>,
    hdr: Option<bool>,
    sharpening: Option<bool>,
    particle_shadows: Option<bool>,
    long_shadows: Option<bool>,
    tree_tessellation: Option<bool>,
    frame_limit: Option<u32>,
}

impl Effects {
    fn from_env() -> Result<Self> {
        let frame_limit = match std::env::var("VICTORY_FRAME_LIMIT") {
            Ok(value) if !value.is_empty() => {
                let value: u32 = value.parse().context("VICTORY_FRAME_LIMIT must be 0, 30, or 60")?;
                if !matches!(value, 0 | 30 | 60) {
                    bail!("VICTORY_FRAME_LIMIT must be 0, 30, or 60");
                }
                Some(value)
            }
            _ => None,
        };
        let disable_shadows = env_effect("VICTORY_DISABLE_SHADOWS")?;
        Ok(Self {
            disable_shadows,
            // The port's DXR path is incomplete, so keep title RT disabled
            // even for direct CLI launches unless the user opts in.
            ray_tracing: Some(if disable_shadows == Some(true) {
                false
            } else {
                env_effect("VICTORY_RAY_TRACING")?.unwrap_or(false)
            }),
            motion_blur: env_effect("VICTORY_MOTION_BLUR")?,
            reflections: env_effect("VICTORY_REFLECTIONS")?,
            vrs: env_effect("VICTORY_VRS")?,
            vsync: env_effect("VICTORY_VSYNC")?,
            hdr: env_effect("VICTORY_HDR")?,
            sharpening: env_effect("VICTORY_SHARPENING")?,
            particle_shadows: env_effect("VICTORY_PARTICLE_SHADOWS")?,
            long_shadows: env_effect("VICTORY_LONG_SHADOWS")?,
            tree_tessellation: env_effect("VICTORY_TREE_TESSELLATION")?,
            frame_limit,
        })
    }

    fn any(self) -> bool {
        self.disable_shadows.is_some() || self.ray_tracing.is_some() || self.motion_blur.is_some()
            || self.reflections.is_some() || self.vrs.is_some()
            || self.vsync.is_some() || self.hdr.is_some()
            || self.sharpening.is_some() || self.particle_shadows.is_some()
            || self.long_shadows.is_some() || self.tree_tessellation.is_some()
            || self.frame_limit.is_some()
    }
}

fn env_effect(name: &str) -> Result<Option<bool>> {
    match std::env::var(name) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "on" | "true" => Ok(Some(true)),
            "0" | "off" | "false" => Ok(Some(false)),
            _ => bail!("{name} must be 0 or 1"),
        },
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {name}")),
    }
}

impl Selection {
    pub fn any(&self) -> bool {
        self.resolution.is_some()
            || self.quality.is_some()
            || self.aa.is_some()
            || self.anisotropic.is_some()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Profile {
    width: Option<u32>,
    height: Option<u32>,
    quality: Option<Quality>,
    aa: Option<Aa>,
    anisotropic: Option<u32>,
}

impl Profile {
    fn any(&self) -> bool {
        self.width.is_some()
            || self.quality.is_some()
            || self.aa.is_some()
            || self.anisotropic.is_some()
    }
}

struct Tier {
    preset: &'static str,
    aniso: &'static str,
    decoration: &'static str,
    grass: &'static str,
    lod: &'static str,
    blur: &'static str,
    ssro: &'static str,
    water_x: &'static str,
    water_y: &'static str,
    tessellation: &'static str,
    long_shadows: &'static str,
    dir_shadow: &'static str,
    height_shadow: &'static str,
    mip_bias: &'static str,
}

fn tier(quality: Quality) -> Tier {
    match quality {
        Quality::Low => Tier {
            preset: "0.000000",
            aniso: "0",
            decoration: "2",
            grass: "0.400000",
            lod: "0.600000",
            blur: "false",
            ssro: "1",
            water_x: "256",
            water_y: "128",
            tessellation: "false",
            long_shadows: "false",
            dir_shadow: "0.000000",
            height_shadow: "0.000000",
            mip_bias: "0.000000",
        },
        Quality::Medium => Tier {
            preset: "1.000000",
            aniso: "4",
            decoration: "1",
            grass: "0.800000",
            lod: "1.000000",
            blur: "true",
            ssro: "2",
            water_x: "512",
            water_y: "256",
            tessellation: "false",
            long_shadows: "false",
            dir_shadow: "0.000000",
            height_shadow: "0.000000",
            mip_bias: "-1.500000",
        },
        Quality::High => Tier {
            preset: "2.000000",
            aniso: "8",
            decoration: "0",
            grass: "1.000000",
            lod: "1.000000",
            blur: "true",
            ssro: "4",
            water_x: "1024",
            water_y: "512",
            tessellation: "false",
            long_shadows: "true",
            dir_shadow: "1.000000",
            height_shadow: "1.000000",
            mip_bias: "-1.500000",
        },
        Quality::Extreme => Tier {
            preset: "3.000000",
            aniso: "16",
            decoration: "0",
            grass: "1.000000",
            lod: "1.250000",
            blur: "true",
            ssro: "8",
            water_x: "1280",
            water_y: "720",
            tessellation: "true",
            long_shadows: "true",
            dir_shadow: "2.000000",
            height_shadow: "2.000000",
            mip_bias: "-1.500000",
        },
    }
}

/// `720p`, `1080p`, or `WIDTHxHEIGHT` inside the console mode limits.
pub fn parse_resolution(value: &str) -> Option<(u32, u32)> {
    let value = value.trim().to_ascii_lowercase();
    let (width, height) = match value.as_str() {
        "720" | "720p" => (1280, 720),
        "1080" | "1080p" => (1920, 1080),
        other => {
            let (width, height) = other.split_once('x')?;
            (width.parse().ok()?, height.parse().ok()?)
        }
    };
    ((640..=7680).contains(&width) && (360..=4320).contains(&height)).then_some((width, height))
}

fn profile_path(data_dir: &Path) -> PathBuf {
    data_dir.join("graphics.txt")
}

fn parse_profile(text: &str) -> Result<Profile> {
    let mut profile = Profile::default();
    for (line, raw) in text.lines().enumerate() {
        let line_no = line + 1;
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let Some((key, value)) = raw.split_once('=') else {
            bail!("{raw} on line {line_no} is not key=value");
        };
        let value = value.trim();
        match key.trim() {
            "width" => profile.width = Some(parse_u32(value, line_no)?),
            "height" => profile.height = Some(parse_u32(value, line_no)?),
            "quality" => {
                profile.quality = Some(parse_quality(value).with_context(|| format!("line {line_no}"))?)
            }
            "aa" => profile.aa = Some(parse_aa(value).with_context(|| format!("line {line_no}"))?),
            "anisotropic" => profile.anisotropic = Some(parse_aniso(value, line_no)?),
            other => bail!("unknown graphics key {other} on line {line_no}"),
        }
    }
    if profile.width.is_some() != profile.height.is_some() {
        bail!("graphics.txt needs both width and height");
    }
    Ok(profile)
}

fn format_profile(profile: &Profile) -> String {
    let mut text = String::from("# victory graphics profile\n");
    if let (Some(width), Some(height)) = (profile.width, profile.height) {
        text.push_str(&format!("width={width}\nheight={height}\n"));
    }
    if let Some(quality) = profile.quality {
        text.push_str(&format!("quality={}\n", quality_name(quality)));
    }
    if let Some(aa) = profile.aa {
        text.push_str(&format!("aa={}\n", aa_name(aa)));
    }
    if let Some(aniso) = profile.anisotropic {
        text.push_str(&format!("anisotropic={aniso}\n"));
    }
    text
}

fn parse_u32(value: &str, line: usize) -> Result<u32> {
    value
        .parse()
        .with_context(|| format!("line {line}: {value} is not a number"))
}

fn parse_aniso(value: &str, line: usize) -> Result<u32> {
    let n = parse_u32(value, line)?;
    if !matches!(n, 0 | 2 | 4 | 8 | 16) {
        bail!("line {line}: anisotropic filtering must be 0, 2, 4, 8, or 16");
    }
    Ok(n)
}

fn parse_quality(value: &str) -> Result<Quality> {
    match value.trim().to_ascii_lowercase().as_str() {
        "low" => Ok(Quality::Low),
        "medium" | "med" => Ok(Quality::Medium),
        "high" => Ok(Quality::High),
        "extreme" | "ultra" => Ok(Quality::Extreme),
        other => bail!("unknown quality {other} (low, medium, high, extreme)"),
    }
}

fn parse_aa(value: &str) -> Result<Aa> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" | "none" => Ok(Aa::Off),
        "msaa" | "on" => Ok(Aa::Msaa),
        other => bail!("unknown anti-aliasing {other} (off or msaa)"),
    }
}

fn quality_name(quality: Quality) -> &'static str {
    match quality {
        Quality::Low => "low",
        Quality::Medium => "medium",
        Quality::High => "high",
        Quality::Extreme => "extreme",
    }
}

fn aa_name(aa: Aa) -> &'static str {
    match aa {
        Aa::Off => "off",
        Aa::Msaa => "msaa",
    }
}

fn load_profile(data_dir: &Path) -> Result<Profile> {
    let path = profile_path(data_dir);
    if !path.is_file() {
        return Ok(Profile::default());
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    parse_profile(&text).with_context(|| format!("parsing {}", path.display()))
}

fn save_profile(data_dir: &Path, profile: &Profile) -> Result<()> {
    let path = profile_path(data_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&path, format_profile(profile)).with_context(|| format!("writing {}", path.display()))
}

struct Resolved {
    profile: Profile,
    save: bool,
    width: Option<u32>,
    height: Option<u32>,
    size_from: &'static str,
    env_warning: Option<String>,
}

fn resolve(data_dir: &Path, selection: &Selection, use_env: bool) -> Result<Resolved> {
    let mut profile = load_profile(data_dir)?;
    let mut save = false;
    if let Some(value) = &selection.resolution {
        let (width, height) = parse_resolution(value)
            .with_context(|| format!("resolution {value} is not 720p, 1080p, or WIDTHxHEIGHT"))?;
        profile.width = Some(width);
        profile.height = Some(height);
        save = true;
    }
    if let Some(quality) = selection.quality {
        profile.quality = Some(quality);
        save = true;
    }
    if let Some(aa) = selection.aa {
        profile.aa = Some(aa);
        save = true;
    }
    if let Some(aniso) = selection.anisotropic {
        if !matches!(aniso, 0 | 2 | 4 | 8 | 16) {
            bail!("anisotropic filtering must be 0, 2, 4, 8, or 16");
        }
        profile.anisotropic = Some(aniso);
        save = true;
    }
    let mut env_warning = None;
    let (width, height, size_from) = if selection.resolution.is_some() {
        (profile.width, profile.height, "flag")
    } else if use_env {
        match std::env::var("VICTORY_OUTPUT_SIZE") {
            Ok(value) if !value.trim().is_empty() => match parse_resolution(&value) {
                Some((width, height)) => (Some(width), Some(height), "VICTORY_OUTPUT_SIZE"),
                None => {
                    env_warning = Some(format!(
                        "ignored invalid VICTORY_OUTPUT_SIZE {value}; using the graphics profile"
                    ));
                    (profile.width, profile.height, "profile")
                }
            },
            _ => (profile.width, profile.height, "profile"),
        }
    } else {
        (profile.width, profile.height, "profile")
    };
    Ok(Resolved {
        profile,
        save,
        width,
        height,
        size_from,
        env_warning,
    })
}

/// Patch `settings.xml` text. A missing tag fails the patch so a partial
/// file is not written.
fn apply_xml(xml: &str, plan: &Profile, width: Option<u32>, height: Option<u32>, effects: Effects) -> Result<String> {
    let mut xml = xml.to_owned();
    let mut missing = Vec::new();
    if let (Some(width), Some(height)) = (width, height) {
        xml = set_required(&xml, "width", &width.to_string(), &mut missing);
        xml = set_required(&xml, "height", &height.to_string(), &mut missing);
        if missing.iter().any(|tag| tag == "width" || tag == "height") {
            bail!("settings.xml has no width and height values to set");
        }
    }
    if let Some(quality) = plan.quality {
        let tier = tier(quality);
        let fields = [
            ("graphicsQualityPreset", tier.preset),
            ("decorationResolutionShift", tier.decoration),
            ("grassLod", tier.grass),
            ("lodScale", tier.lod),
            ("motionBlur", tier.blur),
            ("ssroSampleCount", tier.ssro),
            ("waterReflectionResX", tier.water_x),
            ("waterReflectionResY", tier.water_y),
            ("treeTessellationEnabled", tier.tessellation),
            ("shadowLongShadows", tier.long_shadows),
            ("directionalScreenSpaceShadowQuality", tier.dir_shadow),
            ("worldHeightShadowQuality", tier.height_shadow),
            ("manualMipBias", tier.mip_bias),
        ];
        for (tag, value) in fields {
            xml = set_optional(&xml, tag, value, &mut missing);
        }
        if quality == Quality::Low {
            // The title's template enables several expensive effects even
            // when its preset is low. Keep the low preset truly minimal.
            for tag in ["rtLODLightShadows", "rtDynamicShadows", "waterReflectionSSR",
                "shadowParticleShadows", "vrsEnabled", "sharpenEnabled", "hdr"] {
                xml = set_if_present(&xml, tag, "false");
            }
            xml = set_if_present(&xml, "fsrSharpen", "0.000000");
        }
        if plan.anisotropic.is_none() {
            xml = set_optional(&xml, "anisotropicFiltering", tier.aniso, &mut missing);
        }
    }
    if let Some(aniso) = plan.anisotropic {
        xml = set_optional(&xml, "anisotropicFiltering", &aniso.to_string(), &mut missing);
    }
    if let Some(aa) = plan.aa {
        let value = match aa {
            Aa::Off => "false",
            Aa::Msaa => "true",
        };
        xml = set_optional(&xml, "mapMSAA", value, &mut missing);
    }
    if let Some(on) = effects.ray_tracing {
        for tag in ["rtLODLightShadows", "rtDynamicShadows"] {
            xml = set_optional(&xml, tag, if on { "true" } else { "false" }, &mut missing);
        }
    }
    if let Some(on) = effects.motion_blur {
        xml = set_optional(&xml, "motionBlur", if on { "true" } else { "false" }, &mut missing);
    }
    if let Some(on) = effects.reflections {
        xml = set_optional(&xml, "waterReflectionSSR", if on { "true" } else { "false" }, &mut missing);
        xml = set_optional(&xml, "ssroSampleCount", if on { "2" } else { "0" }, &mut missing);
    }
    if let Some(on) = effects.vrs {
        xml = set_optional(&xml, "vrsEnabled", if on { "true" } else { "false" }, &mut missing);
    }
    if let Some(on) = effects.vsync {
        xml = set_optional(&xml, "vSync", if on { "1" } else { "0" }, &mut missing);
    }
    for (on, tag) in [
        (effects.hdr, "hdr"),
        (effects.sharpening, "sharpenEnabled"),
        (effects.particle_shadows, "shadowParticleShadows"),
        (effects.long_shadows, "shadowLongShadows"),
        (effects.tree_tessellation, "treeTessellationEnabled"),
    ] {
        if let Some(on) = on {
            xml = set_optional(&xml, tag, if on { "true" } else { "false" }, &mut missing);
        }
    }
    if effects.disable_shadows == Some(true) {
        for tag in ["rtLODLightShadows", "rtDynamicShadows", "shadowAlphaTintedShadows",
            "shadowLongShadows", "shadowParticleShadows"] {
            xml = set_if_present(&xml, tag, "false");
        }
        for tag in ["directionalScreenSpaceShadowQuality", "worldHeightShadowQuality"] {
            xml = set_if_present(&xml, tag, "0.000000");
        }
    }
    if let Some(limit) = effects.frame_limit {
        for tag in ["desiredFPS", "frameRateLimit"] {
            xml = set_optional(&xml, tag, &limit.to_string(), &mut missing);
        }
    }
    if !missing.is_empty() {
        let list = missing.join(", ");
        bail!("settings.xml is missing {list}");
    }
    Ok(xml)
}

fn set_if_present(xml: &str, tag: &str, value: &str) -> String {
    set_attr(xml, tag, value).0
}

fn set_required(xml: &str, tag: &str, value: &str, missing: &mut Vec<String>) -> String {
    let (next, found) = set_attr(xml, tag, value);
    if !found {
        missing.push(tag.to_owned());
    }
    next
}

fn set_optional(xml: &str, tag: &str, value: &str, missing: &mut Vec<String>) -> String {
    let (next, found) = set_attr(xml, tag, value);
    if !found {
        missing.push(tag.to_owned());
    }
    next
}

fn set_attr(xml: &str, tag: &str, value: &str) -> (String, bool) {
    let needle = format!("<{tag} value=\"");
    if !xml.contains(&needle) {
        return (xml.to_owned(), false);
    }
    let mut out = String::with_capacity(xml.len() + value.len());
    let mut rest = xml;
    while let Some(at) = rest.find(&needle) {
        out.push_str(&rest[..at + needle.len()]);
        let after = &rest[at + needle.len()..];
        let Some(end) = after.find('"') else {
            out.push_str(after);
            return (out, true);
        };
        out.push_str(value);
        rest = &after[end..];
    }
    out.push_str(rest);
    (out, true)
}

fn first_attr(xml: &str, tag: &str) -> Option<String> {
    let needle = format!("<{tag} value=\"");
    let at = xml.find(&needle)? + needle.len();
    let end = xml[at..].find('"')? + at;
    Some(xml[at..end].to_owned())
}

fn section_attr(xml: &str, section: &str, tag: &str) -> Option<String> {
    let start = xml.find(&format!("<{section}>"))?;
    let rest = &xml[start..];
    let end = rest.find(&format!("</{section}>"))?;
    first_attr(&rest[..end], tag)
}

const SETTINGS_TEMPLATE: &str = include_str!("settings_template.xml");

fn with_card(xml: &str, card: &str) -> Result<String> {
    const OPEN: &str = "<videoCardDescription>";
    const CLOSE: &str = "</videoCardDescription>";
    let start = xml.find(OPEN).context("settings.xml has no videoCardDescription")? + OPEN.len();
    let end = xml[start..]
        .find(CLOSE)
        .context("settings.xml has an unclosed videoCardDescription")?
        + start;
    let mut next = String::with_capacity(xml.len() + card.len());
    next.push_str(&xml[..start]);
    next.push_str(card);
    next.push_str(&xml[end..]);
    Ok(next)
}

fn settings_path(storage: &Path) -> PathBuf {
    storage.join("settings").join("settings.xml")
}

fn write_settings(path: &Path, xml: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, xml).with_context(|| format!("writing {}", path.display()))
}

fn patch_one(path: &Path, profile: &Profile, width: Option<u32>, height: Option<u32>, effects: Effects, card: Option<&str>) -> Result<String> {
    let existing = path.is_file();
    let source = if existing {
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        SETTINGS_TEMPLATE.to_owned()
    };
    let next = apply_xml(&source, profile, width, height, effects)?;
    let next = match card {
        Some(card) => with_card(&next, card)?,
        None => next,
    };
    if next != source || !existing {
        write_settings(path, &next)?;
    }
    Ok(describe(path, &next, existing))
}

fn describe(path: &Path, xml: &str, existed: bool) -> String {
    let ui_w = section_attr(xml, "uiResolution", "width").unwrap_or_else(|| "?".into());
    let ui_h = section_attr(xml, "uiResolution", "height").unwrap_or_else(|| "?".into());
    let preset = first_attr(xml, "graphicsQualityPreset").unwrap_or_else(|| "?".into());
    let aa = first_attr(xml, "mapMSAA").unwrap_or_else(|| "?".into());
    let aniso = first_attr(xml, "anisotropicFiltering").unwrap_or_else(|| "?".into());
    let verb = if existed { "patched" } else { "wrote" };
    let mut line = format!(
        "{verb} {} ui {ui_w}x{ui_h} preset {preset} mapMSAA {aa} aniso {aniso}",
        path.display()
    );
    // The 5700 XT has 8 GB. A 4K UI target is the usual reason a Series S
    // profile still allocates a 3840-wide surface.
    if ui_w.parse::<u32>().ok().is_some_and(|w| w >= 2560) {
        line.push_str("; 8 GB is tight at this UI size, use --resolution 1080p");
    }
    line
}

fn summary(resolved: &Resolved, detail: &str) -> String {
    let mut line = String::new();
    if let Some(warning) = &resolved.env_warning {
        line.push_str(warning);
        line.push_str("; ");
    }
    if let (Some(width), Some(height)) = (resolved.width, resolved.height) {
        line.push_str(&format!("{width}x{height} ({}) ", resolved.size_from));
    }
    if let Some(quality) = resolved.profile.quality {
        line.push_str(quality_name(quality));
        line.push(' ');
    }
    if let Some(aa) = resolved.profile.aa {
        line.push_str("aa ");
        line.push_str(aa_name(aa));
        line.push(' ');
    }
    line.push_str(detail);
    line
}

/// Apply the saved profile and this launch's selection to one title.
/// `storage` is the title's persistent storage directory.
pub fn launch(data_dir: &Path, storage: &Path, console_name: &str, selection: &Selection) -> Result<Option<String>> {
    let resolved = resolve(data_dir, selection, true)?;
    let effects = Effects::from_env()?;
    if resolved.save {
        save_profile(data_dir, &resolved.profile)?;
    }
    let changing = resolved.width.is_some() || resolved.profile.quality.is_some() || resolved.profile.aa.is_some() || resolved.profile.anisotropic.is_some() || effects.any();
    if !changing && !resolved.save && !settings_path(storage).is_file() {
        return Ok(resolved.env_warning);
    }
    if let (Some(width), Some(height)) = (resolved.width, resolved.height)
        && !victory_hle::request_output_size(width, height)
    {
        bail!("refused output size {width}x{height}");
    }
    let path = settings_path(storage);
    let create = !path.is_file();
    if create && !resolved.profile.any() && resolved.width.is_none() && !effects.any() {
        return Ok(resolved.env_warning);
    }
    let mut detail = patch_one(
        &path,
        &resolved.profile,
        resolved.width,
        resolved.height,
        effects,
        Some(console_name),
    )?;
    if let Some(enabled) = effects.ray_tracing {
        detail.push_str(if enabled {
            "; ray tracing requested (experimental)"
        } else {
            "; ray tracing off, BVH builder and native ray-traversal shaders suppressed"
        });
    }
    if effects.disable_shadows == Some(true) {
        detail.push_str("; exposed shadow settings off");
    }
    Ok(Some(summary(&resolved, &detail)))
}

/// Edit the profile and every title `settings.xml` under `data_dir`.
pub fn edit(data_dir: &Path, selection: &Selection) -> Result<String> {
    let resolved = resolve(data_dir, selection, false)?;
    if resolved.save {
        save_profile(data_dir, &resolved.profile)?;
    }
    let mut lines = Vec::new();
    lines.push(format!("profile {}", profile_path(data_dir).display()));
    lines.push(format_profile(&resolved.profile).trim().to_owned());
    if selection.any() {
        let files = settings_files(data_dir);
        if files.is_empty() {
            lines.push("no settings.xml yet; the next run writes one".to_owned());
        }
        for path in files {
            lines.push(patch_one(
                &path,
                &resolved.profile,
                resolved.width,
                resolved.height,
                Effects::default(),
                None,
            )?);
        }
    } else {
        for path in settings_files(data_dir) {
            let xml = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            lines.push(describe(&path, &xml, true));
        }
    }
    Ok(lines.join("\n"))
}

fn settings_files(data_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(titles) = fs::read_dir(data_dir) else {
        return found;
    };
    for title in titles.flatten() {
        let path = title.path().join("storage").join("settings").join("settings.xml");
        if path.is_file() {
            found.push(path);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<graphics>
    <anisotropicFiltering value="0" />
    <graphicsQualityPreset value="0.000000" />
  </graphics>
  <advancedGraphics>
    <decorationResolutionShift value="1" />
    <grassLod value="0.800000" />
    <lodScale value="1.000000" />
    <motionBlur value="true" />
    <ssroSampleCount value="2" />
    <treeTessellationEnabled value="false" />
    <shadowLongShadows value="false" />
    <directionalScreenSpaceShadowQuality value="0.000000" />
    <worldHeightShadowQuality value="0.000000" />
    <manualMipBias value="-1.500000" />
    <waterReflectionResX value="512" />
    <waterReflectionResY value="256" />
    <mapMSAA value="false" />
  </advancedGraphics>
  <video>
    <uiResolution>
      <width value="3840" />
      <height value="2160" />
    </uiResolution>
    <resolutionFullscreen>
      <width value="1920" />
      <height value="1080" />
    </resolutionFullscreen>
  </video>"#;

    #[test]
    fn resolution_names_and_bounds() {
        assert_eq!(parse_resolution("720p"), Some((1280, 720)));
        assert_eq!(parse_resolution("1080P"), Some((1920, 1080)));
        assert_eq!(parse_resolution("1280x720"), Some((1280, 720)));
        assert_eq!(parse_resolution("1920X1080"), Some((1920, 1080)));
        assert_eq!(parse_resolution("9000x1000"), None);
        assert_eq!(parse_resolution("4k"), None);
    }

    #[test]
    fn low_720p_rewrites_the_title_settings() {
        let profile = Profile {
            quality: Some(Quality::Low),
            aa: Some(Aa::Off),
            anisotropic: None,
            ..Profile::default()
        };
        let xml = apply_xml(SAMPLE, &profile, Some(1280), Some(720), Effects::default()).unwrap();
        assert!(xml.contains("<width value=\"1280\" />"));
        assert!(!xml.contains("value=\"3840\""));
        assert!(xml.contains("<height value=\"720\" />"));
        assert!(xml.contains("<graphicsQualityPreset value=\"0.000000\" />"));
        assert!(xml.contains("<mapMSAA value=\"false\" />"));
        assert!(xml.contains("<anisotropicFiltering value=\"0\" />"));
        assert!(xml.contains("<decorationResolutionShift value=\"2\" />"));
        assert!(xml.contains("<motionBlur value=\"false\" />"));
        assert!(xml.contains("<waterReflectionResX value=\"256\" />"));
        assert!(xml.contains("<manualMipBias value=\"0.000000\" />"));
        assert_eq!(
            section_attr(&xml, "uiResolution", "width").as_deref(),
            Some("1280")
        );
        assert_eq!(
            section_attr(&xml, "resolutionFullscreen", "width").as_deref(),
            Some("1280")
        );
    }

    #[test]
    fn extreme_msaa_and_aniso_override_the_preset() {
        let profile = Profile {
            quality: Some(Quality::Extreme),
            aa: Some(Aa::Msaa),
            anisotropic: Some(4),
            ..Profile::default()
        };
        let xml = apply_xml(SAMPLE, &profile, Some(1920), Some(1080), Effects::default()).unwrap();
        assert!(xml.contains("<graphicsQualityPreset value=\"3.000000\" />"));
        assert!(xml.contains("<mapMSAA value=\"true\" />"));
        assert!(xml.contains("<anisotropicFiltering value=\"4\" />"));
        assert!(xml.contains("<treeTessellationEnabled value=\"true\" />"));
        assert!(xml.contains("<ssroSampleCount value=\"8\" />"));
    }

    #[test]
    fn the_captured_template_accepts_a_preset() {
        let profile = Profile {
            quality: Some(Quality::Medium),
            aa: Some(Aa::Off),
            ..Profile::default()
        };
        let xml = apply_xml(SETTINGS_TEMPLATE, &profile, Some(1920), Some(1080), Effects::default()).unwrap();
        assert!(xml.contains("<width value=\"1920\" />"));
        assert!(xml.contains("<graphicsQualityPreset value=\"1.000000\" />"));
        assert!(xml.contains("<mapMSAA value=\"false\" />"));
        assert!(section_attr(&xml, "uiResolution", "height").as_deref() == Some("1080"));
    }

    #[test]
    fn low_preset_and_effect_switches_control_expensive_features() {
        let profile = Profile { quality: Some(Quality::Low), ..Profile::default() };
        let low = apply_xml(SETTINGS_TEMPLATE, &profile, Some(1280), Some(720), Effects::default()).unwrap();
        for tag in ["rtLODLightShadows", "rtDynamicShadows", "waterReflectionSSR",
            "shadowParticleShadows", "vrsEnabled", "motionBlur"] {
            assert_eq!(first_attr(&low, tag).as_deref(), Some("false"), "{tag}");
        }
        let effects = Effects {
            disable_shadows: None,
            ray_tracing: Some(true),
            motion_blur: Some(true),
            reflections: Some(true),
            vrs: Some(true),
            vsync: Some(false),
            hdr: Some(true),
            sharpening: Some(true),
            particle_shadows: Some(true),
            long_shadows: Some(true),
            tree_tessellation: Some(true),
            frame_limit: Some(60),
        };
        let changed = apply_xml(&low, &profile, Some(1280), Some(720), effects).unwrap();
        for tag in ["rtLODLightShadows", "rtDynamicShadows", "waterReflectionSSR",
            "vrsEnabled", "motionBlur"] {
            assert_eq!(first_attr(&changed, tag).as_deref(), Some("true"), "{tag}");
        }
        assert_eq!(first_attr(&changed, "vSync").as_deref(), Some("0"));
        assert_eq!(first_attr(&changed, "frameRateLimit").as_deref(), Some("60"));
        for tag in ["hdr", "sharpenEnabled", "shadowParticleShadows", "shadowLongShadows", "treeTessellationEnabled"] {
            assert_eq!(first_attr(&changed, tag).as_deref(), Some("true"), "{tag}");
        }
    }

    #[test]
    fn ray_tracing_off_overrides_an_existing_enabled_setting_at_high_quality() {
        let enabled = SETTINGS_TEMPLATE.replace(
            "<rtDynamicShadows value=\"false\" />",
            "<rtDynamicShadows value=\"true\" />",
        ).replace(
            "<rtLODLightShadows value=\"false\" />",
            "<rtLODLightShadows value=\"true\" />",
        );
        let xml = apply_xml(
            &enabled,
            &Profile { quality: Some(Quality::High), ..Profile::default() },
            None,
            None,
            Effects { ray_tracing: Some(false), ..Effects::default() },
        ).unwrap();
        for tag in ["rtLODLightShadows", "rtDynamicShadows"] {
            assert_eq!(first_attr(&xml, tag).as_deref(), Some("false"), "{tag}");
        }
    }

    #[test]
    fn disable_shadows_overrides_high_quality_and_individual_switches() {
        let xml = apply_xml(
            SETTINGS_TEMPLATE,
            &Profile { quality: Some(Quality::High), ..Profile::default() },
            None,
            None,
            Effects {
                disable_shadows: Some(true),
                ray_tracing: Some(true),
                particle_shadows: Some(true),
                long_shadows: Some(true),
                ..Effects::default()
            },
        ).unwrap();
        for tag in ["rtLODLightShadows", "rtDynamicShadows", "shadowAlphaTintedShadows",
            "shadowLongShadows", "shadowParticleShadows"] {
            assert_eq!(first_attr(&xml, tag).as_deref(), Some("false"), "{tag}");
        }
        for tag in ["directionalScreenSpaceShadowQuality", "worldHeightShadowQuality"] {
            assert_eq!(first_attr(&xml, tag).as_deref(), Some("0.000000"), "{tag}");
        }
    }

    #[test]
    fn profile_text_roundtrips() {
        let profile = Profile {
            width: Some(1920),
            height: Some(1080),
            quality: Some(Quality::High),
            aa: Some(Aa::Off),
            anisotropic: Some(8),
        };
        let parsed = parse_profile(&format_profile(&profile)).unwrap();
        assert_eq!(parsed, profile);
    }

    #[test]
    fn existing_settings_follow_console_switches_without_other_edits() {
        let custom = SETTINGS_TEMPLATE.replace(
            "<gamma value=\"15\" />",
            "<gamma value=\"21\" />",
        );
        let series_x = with_card(&custom, "Xbox Series X").unwrap();
        assert!(series_x.contains("<videoCardDescription>Xbox Series X</videoCardDescription>"));
        assert!(series_x.contains("<gamma value=\"21\" />"));
        let series_s = with_card(&series_x, "Xbox Series S").unwrap();
        assert_eq!(series_s, custom);
    }
}
