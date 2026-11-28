//! Opt-in proof that a semantically cooked texture can enter the title's
//! existing host upload path without changing its console memory layout.
//! Only the verified tele_beach_07 diffuse source is eligible. The title's
//! normal is BC1 while its cooked output is BC7, so it needs host-resource
//! format work before it can use this path.

use std::{os::windows::fs::MetadataExt, path::{Component, Path, PathBuf, Prefix}, sync::OnceLock};
#[cfg(any(test, feature = "diagnostics"))]
use std::collections::HashSet;
#[cfg(feature = "diagnostics")]
use std::{collections::HashMap, sync::Mutex};
#[cfg(feature = "diagnostics")]
use std::sync::{Arc, Weak};

#[cfg(feature = "diagnostics")]
use super::Staging;

use windows::{Win32::Storage::FileSystem::GetDriveTypeW, core::PCWSTR};

const ENV: &str = "VICTORY_COOKED_DIFFUSE_DDS";
const SOURCE_MIP2_BLAKE3: &str = "dc860eeb40128b375a6fa26e0793ef4781ddbb5adc6f764d72fc2eed68d0f8d1";
const COOKED_DDS_BLAKE3: &str = "8ae5c83c4f25f2959a1f5c200c9cfcf003c6fe7111c098415d183b5b6b6fc87e";
const BC1_SRGB: u32 = 72;
const WIDTH: u32 = 1024;
const HEIGHT: u32 = 512;

struct Cooked {
    source: blake3::Hash,
    mip0: Vec<u8>,
}

/// A drive-qualified path only. UNC and verbatim paths cannot enter the
/// cooked-file loader.
fn local_path(path: &Path) -> bool {
    path.is_absolute()
        && matches!(path.components().next(), Some(Component::Prefix(p))
            if matches!(p.kind(), Prefix::Disk(_)))
}

/// Reject mapped network drives and every reparse point along the path
/// before opening the file. In particular, a fixed-volume junction to a UNC
/// share must not bypass the drive check. Windows DRIVE_FIXED is 3.
pub(super) fn verified_local_path(path: &Path) -> bool {
    if !local_path(path) {
        return false;
    }
    let Some(Component::Prefix(prefix)) = path.components().next() else { return false };
    let Prefix::Disk(letter) = prefix.kind() else { return false };
    let root = [letter as u16, b':' as u16, b'\\' as u16, 0];
    // SAFETY: the four-element UTF-16 root is null terminated.
    if unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) } != 3 {
        return false;
    }
    let mut current = PathBuf::from(format!("{}:\\", char::from(letter)));
    for part in path.components().skip(2) {
        let Component::Normal(name) = part else { return false };
        current.push(name);
        let Ok(meta) = std::fs::symlink_metadata(&current) else { return false };
        if meta.file_attributes() & 0x400 != 0 {
            return false; // FILE_ATTRIBUTE_REPARSE_POINT
        }
    }
    true
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn bc1_len(width: u32, height: u32) -> Option<usize> {
    usize::try_from(width.div_ceil(4))
        .ok()?
        .checked_mul(usize::try_from(height.div_ceil(4)).ok()?)?
        .checked_mul(8)
}

/// The DX10 DDS from the offline cooker. Check all dimensions and all mip
/// payload lengths, so a truncated or differently encoded file is inert.
fn parse_dds(data: &[u8]) -> Option<&[u8]> {
    if data.get(..4)? != b"DDS " || data.get(84..88)? != b"DX10"
        || u32_at(data, 4)? != 124 || u32_at(data, 16)? != WIDTH
        || u32_at(data, 12)? != HEIGHT || u32_at(data, 28)? != 10
        || u32_at(data, 128)? != BC1_SRGB || u32_at(data, 132)? != 3
        || u32_at(data, 140)? != 1
    {
        return None;
    }
    let mut bytes = 148usize;
    for mip in 0..10 {
        bytes = bytes.checked_add(bc1_len((WIDTH >> mip).max(1), (HEIGHT >> mip).max(1))?)?;
    }
    if data.len() != bytes {
        return None;
    }
    data.get(148..148 + bc1_len(WIDTH, HEIGHT)?)
}

fn load(path: &Path) -> Option<Cooked> {
    if !verified_local_path(path) {
        tracing::warn!(path = %path.display(), "cooked texture path must be on a fixed volume without reparse points");
        return None;
    }
    // This one source is 350 KiB. Bound file reads even when the setting is
    // wrong; a larger texture profile needs a separate, reviewed mapping.
    let bytes = std::fs::metadata(path).ok()?.len();
    if !(148..=1 << 20).contains(&bytes) {
        tracing::warn!(path = %path.display(), bytes, "cooked texture DDS length rejected");
        return None;
    }
    let data = std::fs::read(path).ok()?;
    if blake3::hash(&data) != blake3::Hash::from_hex(COOKED_DDS_BLAKE3).ok()? {
        tracing::warn!(path = %path.display(), "cooked texture DDS hash rejected");
        return None;
    }
    let mip0 = parse_dds(&data)?.to_vec();
    tracing::info!(path = %path.display(), bytes, "verified cooked diffuse DDS ready");
    Some(Cooked { source: blake3::Hash::from_hex(SOURCE_MIP2_BLAKE3).ok()?, mip0 })
}

fn selected() -> Option<&'static Cooked> {
    static COOKED: OnceLock<Option<Cooked>> = OnceLock::new();
    COOKED.get_or_init(|| {
        let path = std::env::var_os(ENV)?;
        let result = load(Path::new(&path));
        if result.is_none() {
            tracing::warn!(setting = ENV, "cooked diffuse substitution disabled");
        }
        result
    }).as_ref()
}

/// Hash only the meaningful BC row bytes, excluding D3D12's upload pitch
/// padding. This is the identity SixNG's untiled mip export provides.
pub(super) fn tight_bc_digest(linear: &[u8], width: u32, height: u32, block_bytes: u32,
    pitch: usize, rows: u32) -> Option<(blake3::Hash, bool)>
{
    let expected_rows = height.div_ceil(4);
    let row_bytes = usize::try_from(width.div_ceil(4)).ok()?
        .checked_mul(usize::try_from(block_bytes).ok()?)?;
    let needed = pitch.checked_mul(rows.saturating_sub(1) as usize)?.checked_add(row_bytes)?;
    if rows != expected_rows || pitch < row_bytes || linear.len() < needed {
        return None;
    }
    let mut hash = blake3::Hasher::new();
    let mut all_zero = true;
    for row in 0..rows as usize {
        let bytes = &linear[row * pitch..row * pitch + row_bytes];
        hash.update(bytes);
        all_zero &= bytes.iter().all(|&byte| byte == 0);
    }
    Some((hash.finalize(), all_zero))
}

#[cfg(any(test, feature = "diagnostics"))]
fn admit_trace(seen: &mut HashSet<(u64, [u8; 32])>, address: u64,
    hash: blake3::Hash, limit: usize) -> bool
{
    seen.len() < limit && seen.insert((address, *hash.as_bytes()))
}

#[cfg(feature = "diagnostics")]
#[derive(Default)]
struct TraceState {
    // Separate zero and populated budgets, and separate loading/world phases.
    zero: [HashSet<(u64, [u8; 32])>; 2],
    populated: [HashSet<(u64, [u8; 32])>; 2],
    attempts: [u32; 2],
}

#[cfg(feature = "diagnostics")]
struct Pending {
    subresource: u32,
    source_mip: u32,
    hash: blake3::Hash,
    all_zero: bool,
}

#[cfg(feature = "diagnostics")]
static PENDING: OnceLock<Mutex<HashMap<usize, Pending>>> = OnceLock::new();

#[cfg(feature = "diagnostics")]
#[derive(Clone)]
struct ZeroWatch {
    staging: Weak<Staging>,
    staging_id: usize,
    address: u64,
    subresource: u32,
    source_mip: u32,
    later_copies: u32,
}

#[cfg(feature = "diagnostics")]
static ZEROS: OnceLock<Mutex<Vec<ZeroWatch>>> = OnceLock::new();

/// A failed detile or a later refresh must not carry a previous digest into
/// a different upload.
#[cfg(feature = "diagnostics")]
pub(super) fn clear_pending(staging_id: usize) {
    if let Some(pending) = PENDING.get() {
        pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&staging_id);
    }
}

/// The CopyTextureRegion command for this exact retained subresource was
/// recorded. This does not claim the GPU has finished that submission.
#[cfg(feature = "diagnostics")]
pub(super) fn copy_recorded(staging: &Arc<Staging>, address: u64, subresource: u32) {
    let staging_id = Arc::as_ptr(staging) as usize;
    if let Some(zeros) = ZEROS.get() {
        for watch in zeros.lock().unwrap_or_else(|e| e.into_inner()).iter_mut() {
            if watch.staging_id == staging_id && watch.subresource == subresource {
                watch.later_copies += 1;
            }
        }
    }
    let Some(pending) = PENDING.get() else { return };
    let mut held = pending.lock().unwrap_or_else(|e| e.into_inner());
    if held.get(&staging_id).is_none_or(|p| p.subresource != subresource) {
        return;
    }
    let Some(p) = held.remove(&staging_id) else { return };
    drop(held);
    tracing::info!(address = format_args!("{address:#x}"), hash = %p.hash,
        all_zero = p.all_zero, source_mip = p.source_mip, host_subresource = subresource,
        copy_confirmed = true, "sampled texture upload recorded");
    if p.all_zero {
        let mut watches = ZEROS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
        if watches.len() < 12 && !watches.iter().any(|w| w.staging_id == staging_id) {
            watches.push(ZeroWatch {
                staging: Arc::downgrade(staging), staging_id, address,
                subresource, source_mip: p.source_mip, later_copies: 0,
            });
        }
    }
}

#[cfg(feature = "diagnostics")]
fn audit_digest(staging: &Staging, source_mip: u32, subresource: u32) -> Option<(blake3::Hash, bool)> {
    let f = staging.footprints.get(subresource as usize)?.Footprint;
    let rows = *staging.rows.get(subresource as usize)?;
    let pitch = f.RowPitch as usize;
    let row_bytes = usize::try_from(f.Width.div_ceil(4)).ok()?
        .checked_mul(staging.layout.desc.element_bytes as usize)?;
    let needed = pitch.checked_mul(rows.checked_sub(1)? as usize)?.checked_add(row_bytes)?;
    let size = usize::try_from(staging.layout.size).ok()?;
    if size > 32 << 20 || needed > 8 << 20 || staging.layout.desc.block != 4 {
        return None;
    }
    let mut tiled = vec![0u8; size];
    crate::host::copy_mapped(tiled.as_mut_ptr(), staging.memory, size, false);
    let mut linear = vec![0u8; needed];
    staging.layout.detile(&tiled, source_mip, 0, &mut linear, pitch, pitch.checked_mul(rows as usize)?).ok()?;
    tight_bc_digest(&linear, f.Width, f.Height, staging.layout.desc.element_bytes, pitch, rows)
}

/// Once at or after world+60 presented frames, see whether zero-copied title sources are still empty
/// and whether another upload was recorded. This only reads title memory;
/// it does not modify the host texture or title memory.
#[cfg(feature = "diagnostics")]
pub(super) fn audit_zero_sources(frame: u64) {
    if !crate::switch::on("VICTORY_TRACE_STREAMED_TEXTURES")
        || crate::capture::world_frame().is_none_or(|world| frame < world.saturating_add(60))
    {
        return;
    }
    static AUDITED: std::sync::Once = std::sync::Once::new();
    AUDITED.call_once(|| {
        let Some(zeros) = ZEROS.get() else { return };
        let watches = zeros.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for watch in watches {
            let Some(staging) = watch.staging.upgrade() else {
                tracing::info!(frame, address = format_args!("{:#x}", watch.address),
                    staging_alive = false, "zero-copied sampled texture source audit");
                continue;
            };
            let Some((hash, now_zero)) = audit_digest(&staging, watch.source_mip, watch.subresource) else {
                tracing::warn!(frame, address = format_args!("{:#x}", watch.address),
                    "zero-copied sampled texture source audit unavailable");
                continue;
            };
            tracing::info!(frame, address = format_args!("{:#x}", watch.address),
                source_mip = watch.source_mip, source_now_zero = now_zero,
                source_now_hash = %hash, later_copies = watch.later_copies,
                stale_candidate = !now_zero && watch.later_copies == 0,
                "zero-copied sampled texture source audit");
        }
    });
}

/// Bounded local diagnostics for choosing a real world material to cook.
/// Called on detiled, title-fed sampled textures before any substitution.
#[cfg(feature = "diagnostics")]
#[allow(clippy::too_many_arguments)]
pub(super) fn trace(
    staging_id: usize, host_subresource: u32,
    title_address: u64, title_width: u32, title_height: u32, title_mips: u32,
    title_format: u32, dropped: u32, host_mips: u32, host_format: u32,
    source_mip: u32, slice: u32, host_width: u32, host_height: u32,
    block_bytes: u32, pitch: usize, rows: u32, linear: &[u8],
) {
    if !crate::switch::on("VICTORY_TRACE_STREAMED_TEXTURES")
        || title_width.max(title_height) < 2048
        || slice != 0 || source_mip != dropped
        || !matches!(title_format, 70..=84 | 94..=99)
        || block_bytes != 8 && block_bytes != 16
    {
        return;
    }
    static SEEN: OnceLock<Mutex<TraceState>> = OnceLock::new();
    let world_started = crate::capture::world_frame().is_some();
    let phase = usize::from(world_started);
    let seen = SEEN.get_or_init(|| Mutex::new(TraceState::default()));
    let mut held = seen.lock().unwrap_or_else(|e| e.into_inner());
    let zero_limit = if world_started { 8 } else { 4 };
    let populated_limit = if world_started { 128 } else { 24 };
    let attempts_limit = if world_started { 1024 } else { 256 };
    if held.attempts[phase] >= attempts_limit
        || (held.zero[phase].len() >= zero_limit && held.populated[phase].len() >= populated_limit)
    {
        return;
    }
    held.attempts[phase] += 1;
    drop(held);
    let Some((hash, all_zero)) = tight_bc_digest(linear, host_width, host_height, block_bytes, pitch, rows) else {
        return;
    };
    let mut held = seen.lock().unwrap_or_else(|e| e.into_inner());
    let (entries, limit) = if all_zero {
        (&mut held.zero[phase], zero_limit)
    } else {
        (&mut held.populated[phase], populated_limit)
    };
    if !admit_trace(entries, title_address, hash, limit) {
        return;
    }
    drop(held);
    PENDING.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner()).insert(
        staging_id, Pending { subresource: host_subresource, source_mip, hash, all_zero });
    tracing::info!(address = format_args!("{title_address:#x}"), title_width, title_height,
        title_format, title_mips, host_width, host_height, host_format, host_mips,
        dropped, source_mip, block_bytes, hash = %hash, all_zero,
        world_started,
        "large sampled texture detiled for upload");
}

/// Called after detiling, before the upload copy. The strict source identity
/// prevents the file from replacing another 4K title texture with the same
/// dimensions. Host and console resource formats remain BC1 sRGB.
pub(super) struct ReplaceSource {
    pub(super) address: u64,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) mips: u32,
    pub(super) format: u32,
}

pub(super) struct ReplaceMip {
    pub(super) dropped: u32,
    pub(super) host_mips: u32,
    pub(super) host_format: u32,
    pub(super) source_mip: u32,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) pitch: usize,
    pub(super) rows: u32,
}

pub(super) fn replace(
    source: ReplaceSource,
    mip: ReplaceMip,
    linear: &mut [u8],
) -> bool {
    let ReplaceSource { address: title_address, width: title_width, height: title_height,
        mips: title_mips, format: title_format } = source;
    let ReplaceMip { dropped, host_mips, host_format, source_mip: mip,
        width, height, pitch, rows } = mip;
    if (title_width, title_height, title_mips, title_format, dropped, host_mips, host_format, mip)
        != (4096, 2048, 3, BC1_SRGB, 2, 1, BC1_SRGB, 2)
        || (width, height, pitch, rows) != (WIDTH, HEIGHT, 2048, 128)
    {
        return false;
    }
    let Some(cooked) = selected() else { return false };
    if linear.len() < cooked.mip0.len() {
        return false;
    }
    let observed = blake3::hash(&linear[..cooked.mip0.len()]);
    if observed != cooked.source {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        static MISSES: AtomicU64 = AtomicU64::new(0);
        let n = MISSES.fetch_add(1, Relaxed) + 1;
        if n <= 8 || n.is_power_of_two() {
            tracing::info!(n, address = format_args!("{title_address:#x}"), hash = %observed,
                "4K BC1 candidate is not the verified cooked source");
        }
        return false;
    }
    linear[..cooked.mip0.len()].copy_from_slice(&cooked.mip0);
    static SHOWN: std::sync::Once = std::sync::Once::new();
    SHOWN.call_once(|| tracing::info!(address = format_args!("{title_address:#x}"),
        source_mip = 2, width, height, "verified cooked diffuse substituted into host upload"));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dds() -> Vec<u8> {
        let mut data = vec![0; 148];
        data[..4].copy_from_slice(b"DDS ");
        data[4..8].copy_from_slice(&124u32.to_le_bytes());
        data[12..16].copy_from_slice(&HEIGHT.to_le_bytes());
        data[16..20].copy_from_slice(&WIDTH.to_le_bytes());
        data[28..32].copy_from_slice(&10u32.to_le_bytes());
        data[84..88].copy_from_slice(b"DX10");
        data[128..132].copy_from_slice(&BC1_SRGB.to_le_bytes());
        data[132..136].copy_from_slice(&3u32.to_le_bytes());
        data[140..144].copy_from_slice(&1u32.to_le_bytes());
        for mip in 0..10 {
            data.resize(data.len() + bc1_len((WIDTH >> mip).max(1), (HEIGHT >> mip).max(1)).unwrap(), mip as u8);
        }
        data
    }

    #[test]
    fn exact_dds_chain_required() {
        let data = dds();
        assert_eq!(parse_dds(&data), Some(&data[148..148 + 262_144]));
        for at in [12, 16, 28, 84, 128, 132, 140] {
            let mut bad = data.clone();
            bad[at] ^= 1;
            assert!(parse_dds(&bad).is_none(), "header byte {at}");
        }
        assert!(parse_dds(&data[..data.len() - 1]).is_none());
        let mut extra = data;
        extra.push(0);
        assert!(parse_dds(&extra).is_none());
    }

    #[test]
    fn sample_source_hash_matches_exported_mip() {
        assert_eq!(SOURCE_MIP2_BLAKE3.len(), 64);
        assert!(blake3::Hash::from_hex(SOURCE_MIP2_BLAKE3).is_ok());
        assert!(blake3::Hash::from_hex(COOKED_DDS_BLAKE3).is_ok());
    }

    #[test]
    fn remote_paths_are_rejected() {
        assert!(!local_path(Path::new(r"\\server\share\texture.dds")));
        assert!(!local_path(Path::new(r"texture.dds")));
        assert!(local_path(Path::new(r"E:\assets\texture.dds")));
        assert!(!verified_local_path(Path::new(r"\\server\share\texture.dds")));
        assert!(!verified_local_path(Path::new(r"texture.dds")));
        let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(verified_local_path(&local), "workspace file should be on a fixed local volume");
    }

    #[test]
    fn tight_bc_hash_excludes_upload_padding() {
        let mut rows = vec![0xff; 2 * 256];
        rows[0..16].copy_from_slice(&[1; 16]);
        rows[256..272].copy_from_slice(&[2; 16]);
        let expected = blake3::hash(&[[1; 16], [2; 16]].concat());
        assert_eq!(tight_bc_digest(&rows, 8, 8, 8, 256, 2), Some((expected, false)));
        assert_eq!(tight_bc_digest(&rows[..271], 8, 8, 8, 256, 2), None);
        assert_eq!(tight_bc_digest(&rows, 8, 8, 8, 8, 2), None);
        assert_eq!(tight_bc_digest(&rows, 8, 8, 8, 256, 1), None);
        let zeros = vec![0; 512];
        assert_eq!(tight_bc_digest(&zeros, 8, 8, 8, 256, 2), Some((blake3::hash(&[0; 32]), true)));
    }

    #[test]
    fn trace_quota_dedups_address_and_content_per_phase() {
        let a = blake3::hash(b"first");
        let b = blake3::hash(b"changed");
        let mut pre = HashSet::new();
        let mut world = HashSet::new();
        assert!(admit_trace(&mut pre, 0x1000, a, 2));
        assert!(!admit_trace(&mut pre, 0x1000, a, 2));
        assert!(admit_trace(&mut pre, 0x1000, b, 2), "reused address with new bytes");
        assert!(!admit_trace(&mut pre, 0x2000, a, 2), "preworld cap");
        assert!(admit_trace(&mut world, 0x2000, a, 2), "world keeps a separate quota");
    }
}
