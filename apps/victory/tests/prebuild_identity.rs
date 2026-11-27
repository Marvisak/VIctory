//! A pack's compute records can name different containers holding identical native code.
#![cfg(windows)]

use std::fs;
use std::process::Command;

fn words(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn container(code: &[u8], tag: u32) -> Vec<u8> {
    // An ACCC compute program with no resources, one group of 32 and s_endpgm.
    let packet = words(&[0, 0, 0, 32 | 1 << 16, 1 | 16 << 16, 1, 0]);
    let mut metadata = words(&[0x403, 36, code.len() as u32 / 4, 0, 0, 0x40, 0x43]);
    metadata.resize(56, 0); // Last metadata record holds the unused shader hash.
    let mut part = vec![0; 0x10 + 3 * 16];
    part[..4].copy_from_slice(b"FSPX");
    part[4..12].copy_from_slice(&words(&[1, 3]));
    for (i, (kind, bytes)) in [(0, packet.as_slice()), (6, code), (7, &metadata)]
        .into_iter()
        .enumerate()
    {
        let start = part.len().next_multiple_of(16);
        part.resize(start, 0);
        let at = 0x10 + i * 16;
        part[at..at + 12].copy_from_slice(&words(&[kind, start as u32, bytes.len() as u32]));
        part.extend_from_slice(bytes);
    }
    let size = part.len() as u32;
    part[12..16].copy_from_slice(&size.to_le_bytes());
    let mut file = b"DXBC".to_vec();
    file.resize(20, 0);
    file.extend(words(&[1, 60 + size, 2, 40, 48 + size]));
    file.extend(b"ACCC");
    file.extend(words(&[size]));
    file.extend(part);
    file.extend(b"TEST");
    file.extend(words(&[4, tag])); // Changes the container without changing the native program.
    file
}

fn database(shaders: &[(u64, &[u8])]) -> Vec<u8> {
    let mut file = b"SGD5".to_vec();
    file.extend([0; 8 + 4 + 4 + 5 * 4]); // No shared blocks, strings or graphics shader objects.
    file.extend(words(&[shaders.len() as u32]));
    for &(hash, container) in shaders {
        let compressed = miniz_oxide::deflate::compress_to_vec(container, 1);
        let size = compressed.len() as u32 + 12;
        file.extend([2, b'x', 0, 0x41]);
        file.extend(words(&[size, 0x000d_dce7, size, container.len() as u32]));
        file.extend(compressed);
        file.extend(hash.to_le_bytes());
        file.extend([0; 0x90 + 4]);
    }
    file.extend(words(&[0])); // No effects.
    file
}

fn pack(code: &[u8], hashes: &[u64]) -> Vec<u8> {
    let mut file = vec![0; 0x80];
    file[..4].copy_from_slice(b"PS\0\x09");
    file[0x50..0x68].copy_from_slice(&words(&[
        0,
        hashes.len() as u32,
        1,
        1,
        1,
        code.len() as u32,
    ]));
    for hash in hashes {
        file.extend(hash.to_le_bytes());
        file.extend(words(&[0, 0, 0, u32::MAX])); // Shared packet, metadata and program.
    }
    file.extend(words(&[0, 1, 0, code.len() as u32])); // Blob and program tables.
    file.push(0);
    file.extend(code);
    file
}

#[test]
fn prebuild_selects_the_primary_shader_by_database_hash() {
    // Two identifying v_nop instructions followed by s_endpgm.
    let code = words(&[
        0xd580_0011,
        0x03ff_0000,
        17,
        0xd580_0022,
        0x03ff_0000,
        34,
        0xbf81_0000,
    ]);
    let (a, b) = (container(&code, 1), container(&code, 2));
    let dir = tempfile::tempdir().unwrap();
    let pack_path = dir.path().join("pack");
    let db_path = dir.path().join("db");
    let keys_path = dir.path().join("keys");
    fs::write(&pack_path, pack(&code, &[11, 22, 33])).unwrap();
    fs::write(&db_path, database(&[(11, &a), (22, &b)])).unwrap();

    for jobs in ["1", "8"] {
        let output = Command::new(env!("CARGO_BIN_EXE_victory"))
            .args(["translate", "--cache", "--pipelines", "--jobs", jobs])
            .args([&pack_path, &db_path])
            .env("VICTORY_CACHE_DIR", dir.path().join("cache"))
            .env("VICTORY_DATA_DIR", dir.path().join("data"))
            .env("VICTORY_PREBUILD_KEYS", &keys_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let keys = fs::read_to_string(&keys_path).unwrap();
        assert_eq!(keys.lines().count(), 3);
        for (i, expected) in [&a, &b, &a].into_iter().enumerate() {
            let key = victory_gpu::pipeline::Prebuild::compute(expected).key();
            let prefix = format!("{key}\tCompute({i})\t");
            assert!(keys.lines().any(|line| line.starts_with(&prefix)), "{keys}");
        }
    }
}
