//! Opt-in, exact-identity substitution of selected offline cooked BC mips.
//! The console resource and its original archive remain untouched. A profile
//! can only replace a detiled upload with blocks of the same DXGI format and
//! dimensions; all other uploads use the title's original bytes.

use std::{
    collections::HashMap,
    fs,
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use super::cooked::{tight_bc_digest, verified_local_path};

const ENV: &str = "VICTORY_COOKED_PROFILE_MANIFEST";
const MAX_MANIFEST_BYTES: u64 = 4 << 20;
const MAX_ENTRIES: usize = 16_384;
const MAX_DDS_BYTES: u64 = 64 << 20;
const MAX_CACHED_DDS_BYTES: usize = 128 << 20;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    profile: String,
    texture: Vec<Spec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    source_blake3: String,
    title_width: u32,
    title_height: u32,
    title_mips: u32,
    title_format: u32,
    dropped: u32,
    host_mips: u32,
    host_format: u32,
    source_mip: u32,
    dds_mip: u32,
    dds: String,
    dds_blake3: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Shape {
    title_width: u32,
    title_height: u32,
    title_mips: u32,
    format: u32,
    dropped: u32,
    host_mips: u32,
    source_mip: u32,
    width: u32,
    height: u32,
}

struct Entry {
    path: PathBuf,
    dds_hash: blake3::Hash,
    dds_mip: u32,
}

struct Mip {
    width: u32,
    height: u32,
    at: usize,
    len: usize,
}

struct Dds {
    format: u32,
    hash: blake3::Hash,
    data: Vec<u8>,
    mips: Vec<Mip>,
}

#[derive(Default)]
struct Cache {
    files: HashMap<PathBuf, (Arc<Dds>, u64)>,
    bytes: usize,
    tick: u64,
}

struct Profile {
    name: String,
    entries: HashMap<Shape, HashMap<[u8; 32], Entry>>,
    cache: Mutex<Cache>,
}

fn bc_block_bytes(format: u32) -> Option<u32> {
    match format {
        71 | 72 | 80 | 81 => Some(8),
        74 | 75 | 77 | 78 | 83 | 84 | 98 | 99 => Some(16),
        _ => None,
    }
}

fn mip_extent(size: u32, mip: u32) -> u32 {
    size.checked_shr(mip).unwrap_or(0).max(1)
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(at..at + 4)
            .context("short DDS header")?
            .try_into()?,
    ))
}

fn block_len(width: u32, height: u32, block_bytes: u32) -> Result<usize> {
    let count = usize::try_from(width.div_ceil(4))?
        .checked_mul(usize::try_from(height.div_ceil(4))?)
        .context("DDS block count overflow")?;
    count
        .checked_mul(block_bytes as usize)
        .context("DDS byte count overflow")
}

fn parse_dds(data: Vec<u8>) -> Result<Dds> {
    ensure!(data.get(..4) == Some(b"DDS ".as_slice()), "not a DDS file");
    ensure!(
        u32_at(&data, 4)? == 124 && data.get(84..88) == Some(b"DX10".as_slice()),
        "DDS must have a DX10 header"
    );
    let (height, width, count) = (u32_at(&data, 12)?, u32_at(&data, 16)?, u32_at(&data, 28)?);
    let (format, dimension, array) = (
        u32_at(&data, 128)?,
        u32_at(&data, 132)?,
        u32_at(&data, 140)?,
    );
    let block_bytes = bc_block_bytes(format).context("unsupported cooked BC format")?;
    ensure!(
        width > 0 && height > 0 && width <= 16_384 && height <= 16_384,
        "invalid DDS dimensions"
    );
    ensure!(
        (1..=15).contains(&count) && count <= width.max(height).ilog2() + 1,
        "invalid DDS mip count"
    );
    ensure!(
        dimension == 3 && array == 1 && u32_at(&data, 136)? == 0,
        "DDS must be one ordinary 2D texture"
    );
    ensure!(
        u32_at(&data, 112)? & 0x0000_fe00 == 0,
        "DDS cubemap or volume not supported"
    );
    let mut mips = Vec::with_capacity(count as usize);
    let mut at = 148usize;
    for mip in 0..count {
        let (w, h) = (mip_extent(width, mip), mip_extent(height, mip));
        let len = block_len(w, h, block_bytes)?;
        let end = at.checked_add(len).context("DDS length overflow")?;
        ensure!(end <= data.len(), "truncated DDS payload");
        mips.push(Mip {
            width: w,
            height: h,
            at,
            len,
        });
        at = end;
    }
    ensure!(at == data.len(), "DDS has extra bytes");
    Ok(Dds {
        format,
        hash: blake3::hash(&data),
        data,
        mips,
    })
}

fn parse_hash(hex: &str) -> Result<blake3::Hash> {
    ensure!(hex.len() == 64, "BLAKE3 hash must have 64 hex digits");
    blake3::Hash::from_hex(hex).map_err(|error| anyhow::anyhow!("invalid BLAKE3 hash: {error}"))
}

fn relative_dds(root: &Path, name: &str) -> Result<PathBuf> {
    let rel = Path::new(name);
    ensure!(
        rel.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("dds"))
            && rel.components().next().is_some()
            && rel
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "cooked DDS path must be relative and stay inside its profile root"
    );
    let path = root.join(rel);
    ensure!(
        verified_local_path(&path),
        "cooked DDS must be on a fixed local volume without reparse points"
    );
    Ok(path)
}

fn load(path: &Path) -> Result<Profile> {
    ensure!(
        verified_local_path(path),
        "manifest must be on a fixed local volume without reparse points"
    );
    let length = fs::metadata(path)?.len();
    ensure!(
        (1..=MAX_MANIFEST_BYTES).contains(&length),
        "manifest is empty or too large"
    );
    let text = fs::read_to_string(path)?;
    let manifest: Manifest = toml::from_str(&text).context("invalid cooked profile manifest")?;
    ensure!(manifest.version == 1, "unsupported cooked profile version");
    ensure!(
        matches!(manifest.profile.as_str(), "720p" | "1080p"),
        "invalid cooked profile name"
    );
    ensure!(
        !manifest.texture.is_empty() && manifest.texture.len() <= MAX_ENTRIES,
        "profile needs 1 to {MAX_ENTRIES} verified mip entries"
    );
    let root = path.parent().context("manifest has no parent folder")?;
    let mut entries: HashMap<Shape, HashMap<[u8; 32], Entry>> = HashMap::new();
    for spec in manifest.texture {
        let block_bytes =
            bc_block_bytes(spec.title_format).context("unsupported title BC format")?;
        ensure!(
            spec.host_format == spec.title_format,
            "format-changing profile entry refused"
        );
        ensure!(
            spec.title_width > 0
                && spec.title_height > 0
                && spec.title_width <= 16_384
                && spec.title_height <= 16_384,
            "invalid title dimensions"
        );
        ensure!(
            (1..=15).contains(&spec.title_mips)
                && spec.dropped < spec.title_mips
                && spec.host_mips == spec.title_mips - spec.dropped
                && spec.source_mip >= spec.dropped
                && spec.source_mip < spec.title_mips
                && spec.dds_mip == spec.source_mip - spec.dropped,
            "invalid title/host mip mapping"
        );
        let width = mip_extent(spec.title_width, spec.source_mip);
        let height = mip_extent(spec.title_height, spec.source_mip);
        let _ = block_len(width, height, block_bytes)?;
        let shape = Shape {
            title_width: spec.title_width,
            title_height: spec.title_height,
            title_mips: spec.title_mips,
            format: spec.title_format,
            dropped: spec.dropped,
            host_mips: spec.host_mips,
            source_mip: spec.source_mip,
            width,
            height,
        };
        let source = parse_hash(&spec.source_blake3)?;
        let entry = Entry {
            path: relative_dds(root, &spec.dds)?,
            dds_hash: parse_hash(&spec.dds_blake3)?,
            dds_mip: spec.dds_mip,
        };
        ensure!(
            entries
                .entry(shape)
                .or_default()
                .insert(*source.as_bytes(), entry)
                .is_none(),
            "duplicate profile shape and source hash"
        );
    }
    tracing::info!(profile = %manifest.profile, entries = entries.values().map(HashMap::len).sum::<usize>(),
        path = %path.display(), "verified cooked texture profile indexed");
    Ok(Profile {
        name: manifest.profile,
        entries,
        cache: Mutex::new(Cache::default()),
    })
}

impl Profile {
    fn dds(&self, entry: &Entry) -> Result<Arc<Dds>> {
        {
            let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
            cache.tick = cache.tick.wrapping_add(1);
            let tick = cache.tick;
            if let Some((dds, seen)) = cache.files.get_mut(&entry.path) {
                ensure!(
                    dds.hash == entry.dds_hash,
                    "cached DDS hash differs from entry"
                );
                *seen = tick;
                return Ok(Arc::clone(dds));
            }
        }
        let size = fs::metadata(&entry.path)?.len();
        ensure!(
            (148..=MAX_DDS_BYTES).contains(&size),
            "cooked DDS is too large"
        );
        ensure!(
            verified_local_path(&entry.path),
            "cooked DDS path is no longer local"
        );
        let data = fs::read(&entry.path)?;
        ensure!(
            blake3::hash(&data) == entry.dds_hash,
            "cooked DDS hash changed"
        );
        let dds = Arc::new(parse_dds(data)?);
        let mut cache = self.cache.lock().unwrap_or_else(|error| error.into_inner());
        cache.tick = cache.tick.wrapping_add(1);
        let tick = cache.tick;
        if let Some((existing, seen)) = cache.files.get_mut(&entry.path) {
            ensure!(
                existing.hash == entry.dds_hash,
                "cached DDS hash differs from entry"
            );
            *seen = tick;
            return Ok(Arc::clone(existing));
        }
        while cache.bytes.saturating_add(dds.data.len()) > MAX_CACHED_DDS_BYTES {
            let Some(oldest) = cache
                .files
                .iter()
                .min_by_key(|(_, (_, seen))| *seen)
                .map(|(path, _)| path.clone())
            else {
                break;
            };
            if let Some((evicted, _)) = cache.files.remove(&oldest) {
                cache.bytes = cache.bytes.saturating_sub(evicted.data.len());
            }
        }
        cache.bytes += dds.data.len();
        cache
            .files
            .insert(entry.path.clone(), (Arc::clone(&dds), tick));
        Ok(dds)
    }

    #[allow(clippy::too_many_arguments)]
    fn substitute(&self, shape: Shape, pitch: usize, rows: u32, linear: &mut [u8]) -> Result<bool> {
        let Some(candidates) = self.entries.get(&shape) else {
            return Ok(false);
        };
        let block_bytes = bc_block_bytes(shape.format).context("unsupported BC format")?;
        let (source, _) =
            tight_bc_digest(linear, shape.width, shape.height, block_bytes, pitch, rows)
                .context("invalid detiled upload rows")?;
        let Some(entry) = candidates.get(source.as_bytes()) else {
            return Ok(false);
        };
        let dds = self.dds(entry)?;
        ensure!(
            dds.format == shape.format,
            "cooked DDS format differs from title"
        );
        let mip = dds
            .mips
            .get(entry.dds_mip as usize)
            .context("cooked DDS mip is absent")?;
        ensure!(
            (mip.width, mip.height) == (shape.width, shape.height),
            "cooked DDS mip dimensions differ from title"
        );
        let row_bytes = usize::try_from(shape.width.div_ceil(4))? * block_bytes as usize;
        ensure!(
            mip.len == row_bytes * rows as usize && pitch >= row_bytes,
            "cooked DDS row shape differs from title"
        );
        let needed = pitch
            .checked_mul(rows.saturating_sub(1) as usize)
            .and_then(|start| start.checked_add(row_bytes))
            .context("upload row length overflow")?;
        ensure!(linear.len() >= needed, "short detiled upload buffer");
        for row in 0..rows as usize {
            let from = mip.at + row * row_bytes;
            let to = row * pitch;
            linear[to..to + row_bytes].copy_from_slice(&dds.data[from..from + row_bytes]);
        }
        Ok(true)
    }
}

fn selected() -> Option<&'static Profile> {
    static PROFILE: OnceLock<Option<Profile>> = OnceLock::new();
    PROFILE
        .get_or_init(|| {
            let path = std::env::var_os(ENV)?;
            match load(Path::new(&path)) {
                Ok(profile) => Some(profile),
                Err(error) => {
                    tracing::warn!(path = %Path::new(&path).display(), ?error,
                    "cooked profile disabled; title textures remain original");
                    None
                }
            }
        })
        .as_ref()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn replace(
    title_address: u64,
    title_width: u32,
    title_height: u32,
    title_mips: u32,
    title_format: u32,
    dropped: u32,
    host_mips: u32,
    host_format: u32,
    mip: u32,
    width: u32,
    height: u32,
    pitch: usize,
    rows: u32,
    linear: &mut [u8],
) -> bool {
    if title_format != host_format {
        return false;
    }
    let Some(profile) = selected() else {
        return false;
    };
    let shape = Shape {
        title_width,
        title_height,
        title_mips,
        format: title_format,
        dropped,
        host_mips,
        source_mip: mip,
        width,
        height,
    };
    match profile.substitute(shape, pitch, rows, linear) {
        Ok(true) => {
            static HITS: AtomicU64 = AtomicU64::new(0);
            let count = HITS.fetch_add(1, Ordering::Relaxed) + 1;
            if count <= 16 || count.is_power_of_two() {
                tracing::info!(profile = %profile.name, count, address = format_args!("{title_address:#x}"),
                    mip, width, height, format = title_format,
                    "verified cooked texture mip substituted into host upload");
            }
            true
        }
        Ok(false) => false,
        Err(error) => {
            static FAILURES: AtomicU64 = AtomicU64::new(0);
            let count = FAILURES.fetch_add(1, Ordering::Relaxed) + 1;
            if count <= 8 || count.is_power_of_two() {
                tracing::warn!(profile = %profile.name, count, address = format_args!("{title_address:#x}"),
                    mip, ?error, "cooked texture mip rejected; using original upload");
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dds(color: u8) -> Vec<u8> {
        let mut bytes = vec![0; 148 + 32];
        bytes[..4].copy_from_slice(b"DDS ");
        bytes[4..8].copy_from_slice(&124u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&8u32.to_le_bytes());
        bytes[16..20].copy_from_slice(&8u32.to_le_bytes());
        bytes[28..32].copy_from_slice(&1u32.to_le_bytes());
        bytes[84..88].copy_from_slice(b"DX10");
        bytes[128..132].copy_from_slice(&72u32.to_le_bytes());
        bytes[132..136].copy_from_slice(&3u32.to_le_bytes());
        bytes[140..144].copy_from_slice(&1u32.to_le_bytes());
        bytes[148..].fill(color);
        bytes
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, Vec<u8>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asset.dds");
        let dds = sample_dds(9);
        fs::write(&path, &dds).unwrap();
        let source = vec![3u8; 32];
        let manifest = format!(
            "version = 1\nprofile = \"720p\"\n[[texture]]\nsource_blake3 = \"{}\"\ntitle_width = 8\ntitle_height = 8\ntitle_mips = 1\ntitle_format = 72\ndropped = 0\nhost_mips = 1\nhost_format = 72\nsource_mip = 0\ndds_mip = 0\ndds = \"asset.dds\"\ndds_blake3 = \"{}\"\n",
            blake3::hash(&source),
            blake3::hash(&dds)
        );
        let manifest_path = dir.path().join("runtime_720p.toml");
        fs::write(&manifest_path, manifest).unwrap();
        (dir, manifest_path, source)
    }

    fn shape() -> Shape {
        Shape {
            title_width: 8,
            title_height: 8,
            title_mips: 1,
            format: 72,
            dropped: 0,
            host_mips: 1,
            source_mip: 0,
            width: 8,
            height: 8,
        }
    }

    #[test]
    fn verified_profile_replaces_only_tight_blocks_and_keeps_padding() {
        let (_dir, manifest, source) = fixture();
        let profile = load(&manifest).unwrap();
        let mut linear = vec![0xa5; 272];
        linear[..16].copy_from_slice(&source[..16]);
        linear[256..272].copy_from_slice(&source[16..]);
        assert!(profile.substitute(shape(), 256, 2, &mut linear).unwrap());
        assert_eq!(&linear[..16], &[9; 16]);
        assert_eq!(&linear[256..272], &[9; 16]);
        assert!(linear[16..256].iter().all(|byte| *byte == 0xa5));
        linear[0] = 1;
        assert!(!profile.substitute(shape(), 256, 2, &mut linear).unwrap());
    }

    #[test]
    fn profile_rejects_bad_shape_format_and_changed_dds() {
        let (_dir, manifest, source) = fixture();
        let original = fs::read_to_string(&manifest).unwrap();
        fs::write(
            &manifest,
            original.replace("host_format = 72", "host_format = 99"),
        )
        .unwrap();
        assert!(load(&manifest).is_err());
        fs::write(&manifest, &original).unwrap();
        let profile = load(&manifest).unwrap();
        let mut linear = vec![0; 272];
        linear[..16].copy_from_slice(&source[..16]);
        linear[256..272].copy_from_slice(&source[16..]);
        fs::write(manifest.parent().unwrap().join("asset.dds"), sample_dds(8)).unwrap();
        assert!(profile.substitute(shape(), 256, 2, &mut linear).is_err());
        assert_eq!(&linear[..16], &source[..16]);
    }

    #[test]
    fn dds_rejects_truncated_or_extra_payloads() {
        let dds = sample_dds(7);
        assert!(parse_dds(dds.clone()).is_ok());
        assert!(parse_dds(dds[..dds.len() - 1].to_vec()).is_err());
        let mut extra = dds;
        extra.push(0);
        assert!(parse_dds(extra).is_err());
    }

    /// Optional local verification of an externally cooked profile. Asset
    /// bytes and source identities are never checked into this repository.
    #[test]
    #[ignore = "set VICTORY_COOKED_PROFILE_MANIFEST to a local cooked profile"]
    fn external_profile_and_all_referenced_dds_validate() {
        let path = std::env::var_os(ENV).expect("set VICTORY_COOKED_PROFILE_MANIFEST");
        let profile = load(Path::new(&path)).expect("valid profile manifest");
        let mut count = 0;
        for entries in profile.entries.values() {
            for entry in entries.values() {
                let dds = profile.dds(entry).expect("valid referenced DDS");
                assert!(dds.mips.get(entry.dds_mip as usize).is_some());
                count += 1;
            }
        }
        assert!(count > 0);
    }

    /// Run with a tight original BC mip exported independently from the
    /// supplied title. Confirms the identity and the real DDS block copy.
    #[test]
    #[ignore = "set VICTORY_COOKED_PROFILE_MANIFEST and VICTORY_PROFILE_TEST_SOURCE_MIP"]
    fn external_profile_substitutes_original_mip_blocks() {
        let manifest = std::env::var_os(ENV).expect("set VICTORY_COOKED_PROFILE_MANIFEST");
        let source = std::env::var_os("VICTORY_PROFILE_TEST_SOURCE_MIP")
            .expect("set VICTORY_PROFILE_TEST_SOURCE_MIP");
        let profile = load(Path::new(&manifest)).expect("valid manifest");
        let (shape, source_hash, entry) = profile
            .entries
            .iter()
            .flat_map(|(shape, entries)| {
                entries
                    .iter()
                    .map(move |(hash, entry)| (*shape, hash, entry))
            })
            .next()
            .expect("at least one profile entry");
        let original = fs::read(Path::new(&source)).expect("original detiled BC mip");
        assert_eq!(blake3::hash(&original).as_bytes(), source_hash);
        let block_bytes = bc_block_bytes(shape.format).unwrap() as usize;
        let row_bytes = shape.width.div_ceil(4) as usize * block_bytes;
        let rows = shape.height.div_ceil(4);
        assert_eq!(original.len(), row_bytes * rows as usize);
        let pitch = row_bytes.div_ceil(256) * 256;
        let mut linear = vec![0xcc; pitch * rows as usize];
        for row in 0..rows as usize {
            linear[row * pitch..row * pitch + row_bytes]
                .copy_from_slice(&original[row * row_bytes..(row + 1) * row_bytes]);
        }
        assert!(profile.substitute(shape, pitch, rows, &mut linear).unwrap());
        let dds = profile.dds(entry).unwrap();
        let mip = &dds.mips[entry.dds_mip as usize];
        for row in 0..rows as usize {
            assert_eq!(
                &linear[row * pitch..row * pitch + row_bytes],
                &dds.data[mip.at + row * row_bytes..mip.at + (row + 1) * row_bytes],
            );
        }
    }
}
