use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> io::Result<Self> {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        let temp = std::env::temp_dir();
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        loop {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = temp.join(format!(
                "victory-config-{}-{}-{id}",
                process::id(),
                timestamp.as_nanos(),
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            let message = format!("remove scratch directory {}: {error}", self.0.display());
            if std::thread::panicking() {
                eprintln!("{message}");
            } else {
                panic!("{message}");
            }
        }
    }
}

fn run_config(root: &Path, violations: &[&str]) -> Output {
    let config = root.join("config.toml");
    std::fs::write(
        &config,
        r#"[paths]
game_dir = "game"
cache_dir = "cache"
data_dir = "data"
test_titles = ["test-title"]
"#,
    )
    .unwrap();
    std::fs::create_dir(root.join("game")).unwrap();
    std::fs::create_dir(root.join("test-title")).unwrap();
    for name in violations {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".victory-root"), "").unwrap();
    }

    Command::new(env!("CARGO_BIN_EXE_victory"))
        .arg("config")
        .current_dir(root)
        .env("VICTORY_CONFIG", config)
        .env_remove("VICTORY_GAME_DIR")
        .env_remove("VICTORY_CACHE_DIR")
        .env_remove("VICTORY_DATA_DIR")
        .env_remove("VICTORY_TEST_TITLES")
        .env_remove("VICTORY_FIDELITYFX_DIR")
        // Diagnostics builds can select audio-recorder mode before parsing arguments.
        .env_remove("VICTORY_AUDIO_CAPTURE")
        .env_remove("VICTORY_AUDIO_CAPTURE_OF")
        // The test asserts the complete stderr summary.
        .env("RUST_LIB_BACKTRACE", "0")
        .output()
        .unwrap()
}

fn assert_config(violations: &[&str]) {
    let scratch = ScratchDir::new().expect("create config test scratch directory");
    let root = scratch.0.as_path();
    let output = run_config(root, violations);
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let (code, summary) = if violations.is_empty() {
        (0, None)
    } else {
        (
            1,
            Some(format!(
                "Error: {} configured path(s) violate the asset policy",
                violations.len()
            )),
        )
    };
    assert_eq!(output.status.code(), Some(code), "{stdout}\n{stderr}");
    assert!(
        stderr.lines().eq(summary.as_deref()),
        "expected {summary:?}, got {stderr:?}"
    );

    let config_line = format!("config file : {}", root.join("config.toml").display());
    for line in [
        config_line.as_str(),
        "game_dir    : game",
        "cache_dir   : cache",
        "data_dir    : data",
        "fidelityfx  : (not set)",
        "test_title  : test-title",
    ] {
        assert!(stdout.lines().any(|actual| actual == line), "{stdout}");
    }
    let diagnostics: Vec<_> = stdout
        .lines()
        .filter(|line| line.starts_with("POLICY VIOLATION: "))
        .collect();
    assert_eq!(diagnostics.len(), violations.len(), "{stdout}");
    for (line, path) in diagnostics.iter().zip(violations) {
        assert!(
            line.starts_with(&format!(
                "POLICY VIOLATION: refusing to use {path} for game-derived data:"
            )),
            "{stdout}"
        );
    }
}

#[test]
fn config_succeeds_with_paths_outside_a_source_tree() {
    assert_config(&[]);
}

#[test]
fn config_reports_a_game_dir_violation() {
    assert_config(&["game"]);
}

#[test]
fn config_reports_a_test_title_violation() {
    assert_config(&["test-title"]);
}

#[test]
fn config_reports_multiple_violations() {
    assert_config(&["cache", "data"]);
}
