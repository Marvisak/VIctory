use std::fs;

#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

#[test]
fn only_test_module_edits_preserve_the_translator_hash() {
    let workspace = tempfile::tempdir().unwrap();
    let shader = workspace.path().join("crates/victory-shader");
    let emit = shader.join("src/emit");
    fs::create_dir_all(&emit).unwrap();
    fs::write(workspace.path().join("Cargo.lock"), "").unwrap();
    fs::write(shader.join("Cargo.toml"), "").unwrap();
    fs::write(shader.join("build.rs"), "").unwrap();

    let tests = emit.join("valu_tests.rs");
    let translator = emit.join("lower.rs");
    let cases = include_str!("../src/emit/valu_tests.rs");
    fs::write(&tests, cases).unwrap();
    fs::write(&translator, "pub const LIMIT: u32 = 1;").unwrap();
    let hash = || build_script::source_hash(&shader, b"rustc fixture");
    let original = hash();

    fs::write(&tests, format!("{cases}\n#[test] fn another_case() {{}}")).unwrap();
    assert_eq!(hash(), original);

    fs::write(&translator, "pub const LIMIT: u32 = 2;").unwrap();
    let changed = hash();
    assert_ne!(changed, original);

    // The same filename can become production code if its test gate is removed.
    fs::write(&tests, "pub fn second() {}").unwrap();
    assert_ne!(hash(), changed);
}
