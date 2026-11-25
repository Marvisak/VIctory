#![cfg(all(windows, feature = "diagnostics"))]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn audio_capture_child_does_not_truncate_parent_log_file() {
    const ROLE: &str = "VICTORY_APP_LOG_TEST_ROLE";
    const TEST: &str = "audio_capture_child_does_not_truncate_parent_log_file";

    if std::env::var(ROLE).as_deref() == Ok("parent") {
        let wav = std::env::var_os("VICTORY_APP_TEST_CAPTURE_WAV").unwrap();
        victory_core::logging::init("info");
        tracing::info!("BEFORE");

        let status = Command::new(env!("CARGO_BIN_EXE_victory"))
            .env("VICTORY_AUDIO_CAPTURE", wav)
            .env("VICTORY_AUDIO_CAPTURE_OF", "0")
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "{status:?}");

        tracing::info!("AFTER");
        return;
    }

    let dir = TempDir::new("victory-capture-log-test");
    let log_path = dir.0.join("parent.log");
    let wav_path = dir.0.join("capture.wav");

    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"])
        .env(ROLE, "parent")
        .env("VICTORY_LOG", "info")
        .env("VICTORY_LOG_FILE", &log_path)
        .env("VICTORY_APP_TEST_CAPTURE_WAV", &wav_path)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");

    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains("BEFORE"), "{stderr:?}");
    assert!(stderr.contains("audio capture stopped"), "{stderr:?}");
    assert!(stderr.contains("AFTER"), "{stderr:?}");

    let file_bytes = fs::read(&log_path).unwrap();
    assert!(
        !file_bytes.contains(&0),
        "log file must not contain NUL gaps: {file_bytes:?}"
    );
    let file_text = String::from_utf8(file_bytes).unwrap();
    let lines: Vec<_> = file_text.lines().collect();
    assert_eq!(lines.len(), 2, "{file_text:?}");
    assert!(lines[0].contains("BEFORE"), "{file_text:?}");
    assert!(lines[1].contains("AFTER"), "{file_text:?}");
    assert!(
        !file_text.contains("audio capture stopped"),
        "{file_text:?}"
    );
}
