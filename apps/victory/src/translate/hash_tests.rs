use super::*;
use crate::{Cli, Command};
use clap::Parser;

fn container(tag: u8) -> Vec<u8> {
    // A deliberately truncated XDAT table gives an observable translation
    // refusal. Vary only the container digest, keeping the DXIL identical.
    let mut part = vec![0; 0x18];
    part.extend(b"XDAT");
    dxil_container(tag, &part)
}

fn dxil_container(tag: u8, part: &[u8]) -> Vec<u8> {
    let mut bytes = b"DXBC".to_vec();
    bytes.extend([tag; 16]);
    for word in [1, 44 + part.len() as u32, 1, 36] {
        bytes.extend(word.to_le_bytes());
    }
    bytes.extend(b"DXIL");
    bytes.extend((part.len() as u32).to_le_bytes());
    bytes.extend(part);
    bytes
}

fn id(bytes: &[u8]) -> u64 {
    parse_hash(&blake3::hash(bytes).to_hex()[..16]).unwrap()
}

fn scan(data: &[u8], requested: &BTreeSet<u64>) -> Tally {
    let mut total = Tally::default();
    let seen = Mutex::default();
    let mut checks = Checks {
        keep: None,
        validator: None,
        host: None,
    };
    for src in sources(data) {
        let mut t = Tally {
            list: true,
            ..Tally::default()
        };
        translate(
            &src,
            Path::new("archive"),
            &seen,
            requested,
            &mut t,
            &mut checks,
        );
        total.merge(t);
    }
    total
}

#[test]
fn replays_selected_raw_and_packed_identities_even_with_identical_dxil() {
    let (a, b, c) = (container(1), container(2), container(3));
    let mut padded = b.clone();
    padded.extend([0xcc; 9]);

    // A final DEFLATE stored block holds the bytes verbatim. The normal
    // scan also sees its DXBC as a raw, embedded copy of the same shader.
    let len = u16::try_from(padded.len()).unwrap();
    let deflate_len = 5 + u32::from(len);
    let n = 12 + deflate_len;
    let mut archive = [c, a.clone(), a.clone()].concat();
    archive.extend(b"SGD5\x02P\x00\x32");
    for word in [n, 0x000d_dce7, n, u32::from(len)] {
        archive.extend(word.to_le_bytes());
    }
    archive.push(1);
    archive.extend(len.to_le_bytes());
    archive.extend((!len).to_le_bytes());
    archive.extend(&padded);
    archive.extend([0xbb; 8]);

    let requested = BTreeSet::from([id(&a), id(&b), id(&padded)]);
    let t = scan(&archive, &requested);
    check_matches(&requested, &t.matched).unwrap();
    assert_eq!((t.containers, t.duplicates, t.lines.len()), (5, 2, 3));
    for id in &requested {
        assert!(t.lines.iter().any(|line| line.contains(&format!("hash={id:016x}")) && line.contains("truncated")));
    }
    let t = scan(&archive, &BTreeSet::new());
    assert_eq!((t.containers, t.duplicates), (5, 4));
}

#[test]
fn padded_file_length_is_a_candidate_only_at_the_start_of_a_raw_file() {
    let declared = container(1);
    let mut padded = declared.clone();
    padded.extend([0xaa; 16]);
    let requested = BTreeSet::from([id(&declared), id(&padded)]);
    let t = scan(&padded, &requested);
    assert_eq!(t.matched.len(), 2);
    check_matches(&requested, &t.matched).unwrap();

    let mut embedded = vec![0xbb; 8];
    embedded.extend(&padded);
    let requested = BTreeSet::from([id(&declared), id(&padded), 0]);
    let t = scan(&embedded, &requested);
    assert_eq!(
        t.matched,
        BTreeMap::from([(id(&declared), String::from("archive@0x8"))])
    );
    let error = check_matches(&requested, &t.matched)
        .unwrap_err()
        .to_string();
    assert!(
        error.ends_with(&format!("0000000000000000, {:016x}", id(&padded))),
        "{error}"
    );
}

#[test]
fn reports_selected_container_without_native_programs() {
    let mut part = vec![0; 0x18];
    part.extend(b"XDAT");
    part.extend([0; 8]); // No XDAT entries.
    let bytes = dxil_container(1, &part);
    let hash = id(&bytes);
    let requested = BTreeSet::from([hash]);
    let t = scan(&bytes, &requested);
    check_matches(&requested, &t.matched).unwrap();
    assert_eq!(
        t.lines,
        [format!("archive@0x0 hash={hash:016x}  no native programs")]
    );
    assert!(scan(&bytes, &BTreeSet::new()).lines.is_empty());
}

#[test]
fn parses_repeated_and_comma_separated_hashes() {
    let cli = Cli::try_parse_from([
        "victory",
        "translate",
        "--shader-hash",
        "0123456789ABCDEF,0000000000000000",
        "--shader-hash",
        "ffffffffffffffff",
    ])
    .unwrap();
    let Command::Translate { shader_hash, .. } = cli.command else {
        panic!("expected translate")
    };
    assert_eq!(shader_hash, [0x0123456789abcdef, 0, u64::MAX]);
}

#[test]
fn rejects_malformed_hashes_and_cache_warm_up() {
    use clap::error::ErrorKind;
    for hash in [
        "0",
        "1234567890abcdef0",
        "+abcdef012345678",
        "xyz0000000000000",
    ] {
        let err = Cli::try_parse_from(["victory", "translate", "--shader-hash", hash])
            .err()
            .unwrap();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
        assert!(err.to_string().contains("16 hexadecimal digits"));
    }
    let err = Cli::try_parse_from([
        "victory",
        "translate",
        "--cache",
        "--shader-hash",
        "0123456789abcdef",
    ])
    .err()
    .unwrap();
    assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
}
