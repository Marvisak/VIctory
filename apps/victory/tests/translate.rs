use std::path::Path;
use std::process::Command;

#[test]
fn translate_rejects_zero_workers() {
    let input = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    for extra_args in [&[][..], &["--cache"][..]] {
        let output = Command::new(env!("CARGO_BIN_EXE_victory"))
            .arg("translate")
            .arg(&input)
            .args(["--jobs", "0"])
            .args(extra_args)
            .output()
            .expect("run victory translate");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{extra_args:?}: {stderr}");
        assert!(stderr.contains("--jobs"), "{extra_args:?}: {stderr}");
    }
}

/// A minimal ACCC compute container with no resources.
fn compute_source(code: &[u32], [x, y, z]: [u32; 3]) -> Vec<u8> {
    let packet = [0, 0, 0, x | (y << 16), z | (16 << 16), 0, 0];
    let mut metadata = vec![0x403, 36, code.len() as u32, 0, 0, 0x40, 0x43];
    metadata.resize(14, 0);
    let entries = [(0, &packet[..]), (6, code), (7, &metadata[..])];
    let mut part = vec![u32::from_le_bytes(*b"FSPX"), 1, entries.len() as u32, 0];
    let mut at = (4 + 4 * entries.len()) as u32 * 4;
    for (kind, words) in entries {
        let size = words.len() as u32 * 4;
        part.extend([kind, at, size, 0]);
        at += size;
    }
    part[3] = at;
    for (_, words) in entries {
        part.extend_from_slice(words);
    }
    let mut words = vec![
        u32::from_le_bytes(*b"DXBC"),
        0,
        0,
        0,
        0,
        1,
        44 + at,
        1,
        36,
        u32::from_le_bytes(*b"ACCC"),
        at,
    ];
    words.extend(part);
    words.into_iter().flat_map(u32::to_le_bytes).collect()
}

#[test]
fn lists_compute_dimensions_for_emission_and_refusal() {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    let end = 0xbf81_0000; // s_endpgm
    let trap = 0xbf92_0001; // s_trap 1
    let cases = [
        (&[end][..], [4, 3, 2], "emitted"),
        (&[trap, end][..], [2, 3, 4], "unsupported: s_trap"),
    ];
    let mut bytes = Vec::new();
    let mut rows = Vec::new();
    for (code, threads, outcome) in cases {
        let offset = bytes.len();
        let container = compute_source(code, threads);
        let hash = blake3::hash(&container).to_hex();
        rows.push(format!(
            "@{offset:#x} Cs  {outcome}  hash={} numthreads={threads:?}",
            &hash[..16]
        ));
        bytes.extend(container);
    }
    // Raw scan slices run to EOF. Each hash must stop at its DXBC size.
    bytes.extend(b"trailing bytes");
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "victory-compute-{}-{timestamp}",
        std::process::id()
    ));
    std::fs::File::create_new(&path)
        .unwrap()
        .write_all(&bytes)
        .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_victory"))
        .args(["translate", "--list", "--jobs", "1"])
        .arg(&path)
        .env_remove("VICTORY_EMIT_DUMP")
        .env_remove("VICTORY_PAIRS")
        .output();
    std::fs::remove_file(path).unwrap();
    let output = output.unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for row in rows {
        assert!(stdout.lines().any(|line| line.ends_with(&row)), "{stdout}");
    }
}
