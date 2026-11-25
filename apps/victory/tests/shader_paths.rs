use std::process::{Command, Output};

fn assert_one_container(command: &str, output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("containers") && line.contains(" 1 (")),
        "{command}: {stdout}"
    );
}

#[test]
fn shader_commands_distinguish_clean_and_incomplete_scans() {
    let pid = std::process::id();
    let mut attempt = 0u32;
    let dir = loop {
        let path = std::env::temp_dir().join(format!("victory-shader-paths-{pid}-{attempt}"));
        match std::fs::create_dir(&path) {
            Ok(()) => break path,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => attempt += 1,
            Err(err) => panic!("could not create {}: {err}", path.display()),
        }
    };
    // Minimal DXBC header with no parts.
    let mut container = [0u8; 32];
    container[..4].copy_from_slice(b"DXBC");
    container[20] = 1; // major version
    container[24] = 32; // container size
    std::fs::write(dir.join("one"), container).unwrap();

    for command in ["shader-info", "translate"] {
        let output = Command::new(env!("CARGO_BIN_EXE_victory"))
            .current_dir(&dir)
            .env_remove("VICTORY_PAIRS")
            .env_remove("VICTORY_EMIT_DUMP")
            .args([command, "first-input", "one", "second-input"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{command}");
        assert_one_container(command, &output);
        let stderr = String::from_utf8_lossy(&output.stderr);
        for name in ["first-input", "second-input", "traversal errors: 2"] {
            assert!(stderr.contains(name), "{command}: {stderr}");
        }

        let clean = Command::new(env!("CARGO_BIN_EXE_victory"))
            .current_dir(&dir)
            .env_remove("VICTORY_PAIRS")
            .env_remove("VICTORY_EMIT_DUMP")
            .args([command, "."])
            .output()
            .unwrap();
        assert_eq!(
            clean.status.code(),
            Some(0),
            "{command}: {}",
            String::from_utf8_lossy(&clean.stderr)
        );
        assert_one_container(command, &clean);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
