//! One tracing setup for every binary.
//!
//! Filter with `VICTORY_LOG` (falls back to `RUST_LOG`), e.g.
//! `VICTORY_LOG=victory_loader=trace,info`. Debug and trace output exist
//! only in builds with the `diagnostics` feature; a plain release build
//! logs info and above whatever the filter asks for.
//!
//! Set `VICTORY_LOG_FILE=<file>` to also write logs as plain UTF-8 lines
//! without ANSI escapes. Standard error emits ANSI colour codes only when
//! connected to a terminal, unless overridden by `NO_COLOR` or
//! `CLICOLOR_FORCE`.

use std::ffi::OsString;
use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use tracing::Subscriber;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The most verbose level this build can log.
const CAP: LevelFilter = if cfg!(feature = "diagnostics") {
    LevelFilter::TRACE
} else {
    LevelFilter::INFO
};

pub fn init(default_level: &str) {
    init_inner(default_level, true);
}

/// Initialises logging to stderr only without opening `VICTORY_LOG_FILE`, for
/// helper child processes whose parent owns the per-run log file.
pub fn init_stderr_only(default_level: &str) {
    init_inner(default_level, false);
}

fn init_inner(default_level: &str, mirror_file: bool) {
    let env = |k: &str| std::env::var_os(k);
    let ansi = use_stderr_ansi(std::io::stderr().is_terminal(), env);
    let file_writer = if mirror_file {
        log_file_path(env).and_then(|path| match open_log_file(&path) {
            Ok(file) => Some(SyncWriter(Arc::new(Mutex::new(file)))),
            Err(e) => {
                eprintln!(
                    "warning: could not open VICTORY_LOG_FILE {}: {e}",
                    path.display()
                );
                None
            }
        })
    } else {
        None
    };
    let _ = build_subscriber(default_level, ansi, std::io::stderr, file_writer, env).try_init();
}

fn use_stderr_ansi(is_terminal: bool, env: impl Fn(&str) -> Option<OsString>) -> bool {
    if env("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if env("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    is_terminal
}

fn log_file_path(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    env("VICTORY_LOG_FILE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn open_log_file(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    File::create(path)
}

fn build_subscriber<W, F>(
    default_level: &str,
    stderr_ansi: bool,
    stderr_writer: W,
    file_writer: Option<F>,
    env: impl Fn(&str) -> Option<OsString>,
) -> impl Subscriber + Send + Sync
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
    F: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let filter = env("VICTORY_LOG")
        .or_else(|| env("RUST_LOG"))
        .and_then(|v| v.into_string().ok())
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(default_level));

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_ansi(stderr_ansi)
        .with_writer(stderr_writer);

    let file_layer = file_writer.map(|writer| {
        tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_ansi(false)
            .with_writer(writer)
    });

    tracing_subscriber::registry()
        .with(CAP)
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
}

#[derive(Clone)]
struct SyncWriter<W>(Arc<Mutex<W>>);

struct GuardedWriter<'a, W>(MutexGuard<'a, W>);

impl<W: Write> Write for GuardedWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl<'a, W: Write + 'static> MakeWriter<'a> for SyncWriter<W> {
    type Writer = GuardedWriter<'a, W>;

    fn make_writer(&'a self) -> Self::Writer {
        GuardedWriter(self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), OsString::from(*v)))
            .collect();
        move |key| map.get(key).cloned()
    }

    fn take_text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        let bytes = buf.lock().unwrap_or_else(|e| e.into_inner()).clone();
        String::from_utf8(bytes).expect("log output must be valid UTF-8")
    }

    #[test]
    fn ansi_follows_terminal_and_standard_overrides() {
        assert!(use_stderr_ansi(true, env_map(&[])));
        assert!(!use_stderr_ansi(false, env_map(&[])));

        // NO_COLOR disables colour even on a terminal or when CLICOLOR_FORCE=1.
        assert!(!use_stderr_ansi(true, env_map(&[("NO_COLOR", "1")])));
        assert!(!use_stderr_ansi(
            false,
            env_map(&[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")])
        ));
        // An empty NO_COLOR has no effect per no-color.org.
        assert!(use_stderr_ansi(true, env_map(&[("NO_COLOR", "")])));

        // CLICOLOR_FORCE=1 enables colour when redirected; "0" or empty does not.
        assert!(use_stderr_ansi(false, env_map(&[("CLICOLOR_FORCE", "1")])));
        assert!(!use_stderr_ansi(false, env_map(&[("CLICOLOR_FORCE", "0")])));
        assert!(!use_stderr_ansi(false, env_map(&[("CLICOLOR_FORCE", "")])));
    }

    #[test]
    fn empty_log_file_env_is_ignored() {
        assert_eq!(log_file_path(env_map(&[])), None);
        assert_eq!(log_file_path(env_map(&[("VICTORY_LOG_FILE", "")])), None);
        assert_eq!(
            log_file_path(env_map(&[("VICTORY_LOG_FILE", "out/run.log")])),
            Some(PathBuf::from("out/run.log"))
        );
    }

    #[test]
    fn stderr_redirects_without_ansi_and_file_receives_plain_utf8() {
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("nested").join("run.log");
        let file = open_log_file(&log_path).unwrap();

        let sub = build_subscriber(
            "info",
            false,
            SyncWriter(Arc::clone(&stderr_buf)),
            Some(SyncWriter(Arc::new(Mutex::new(file)))),
            env_map(&[("VICTORY_LOG", "info")]),
        );

        let long_msg = format!("utf8 line: Türkçe şğıüöç — {}", "x".repeat(220));
        tracing::subscriber::with_default(sub, || {
            tracing::info!("{long_msg}");
        });

        let stderr_text = take_text(&stderr_buf);
        assert!(
            !stderr_text.contains('\x1b'),
            "redirected stderr must not contain ANSI escapes: {stderr_text:?}"
        );
        assert!(stderr_text.contains(&long_msg));

        let file_bytes = std::fs::read(&log_path).unwrap();
        // No UTF-16 BOM; plain UTF-8 single line without console wrapping or ANSI codes.
        assert!(!file_bytes.starts_with(&[0xFF, 0xFE]));
        assert!(!file_bytes.starts_with(&[0xFE, 0xFF]));
        let file_text = String::from_utf8(file_bytes).unwrap();
        assert!(!file_text.contains('\x1b'));
        let lines: Vec<_> = file_text.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains(&long_msg));
    }

    #[test]
    fn terminal_stderr_keeps_ansi_while_log_file_stays_plain() {
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));
        let file_buf = Arc::new(Mutex::new(Vec::new()));

        let sub = build_subscriber(
            "info",
            true,
            SyncWriter(Arc::clone(&stderr_buf)),
            Some(SyncWriter(Arc::clone(&file_buf))),
            env_map(&[("VICTORY_LOG", "info")]),
        );

        tracing::subscriber::with_default(sub, || {
            tracing::info!("hello terminal");
        });

        let stderr_text = take_text(&stderr_buf);
        let file_text = take_text(&file_buf);
        assert!(
            stderr_text.contains('\x1b'),
            "terminal stderr should include ANSI escapes: {stderr_text:?}"
        );
        assert!(
            !file_text.contains('\x1b'),
            "file output must never include ANSI escapes: {file_text:?}"
        );
        assert!(file_text.contains("hello terminal"));
    }

    #[test]
    fn unwritable_log_file_reports_io_error_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        // Opening a directory path as a log file fails cleanly.
        assert!(open_log_file(dir.path()).is_err());
    }

    #[test]
    fn concurrent_writes_produce_intact_utf8_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("concurrent.log");
        let file = open_log_file(&log_path).unwrap();
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));

        let sub = Arc::new(build_subscriber(
            "info",
            false,
            SyncWriter(Arc::clone(&stderr_buf)),
            Some(SyncWriter(Arc::new(Mutex::new(file)))),
            env_map(&[("VICTORY_LOG", "info")]),
        ));

        std::thread::scope(|s| {
            for t in 0..4 {
                let sub = Arc::clone(&sub);
                s.spawn(move || {
                    let _guard = tracing::subscriber::set_default(sub);
                    for i in 0..10 {
                        tracing::info!("worker={t} seq={i} ok");
                    }
                });
            }
        });

        let text = std::fs::read_to_string(&log_path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 40);
        for line in lines {
            assert!(!line.contains('\x1b'));
            assert!(
                line.contains("worker=") && line.ends_with(" ok"),
                "{line:?}"
            );
        }
    }

    #[test]
    fn two_process_stderr_only_child_preserves_parent_log_file() {
        const ROLE: &str = "VICTORY_LOG_TEST_ROLE";
        const TEST: &str =
            "logging::tests::two_process_stderr_only_child_preserves_parent_log_file";

        match std::env::var(ROLE).as_deref() {
            Ok("parent") => {
                init("info");
                tracing::info!("BEFORE");
                let status = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(ROLE, "child")
                    .stdout(std::process::Stdio::null())
                    .status()
                    .unwrap();
                assert!(status.success());
                tracing::info!("AFTER");
            }
            Ok("child") => {
                init_stderr_only("info");
                tracing::info!("CHILD");
            }
            _ => {
                let dir = tempfile::tempdir().unwrap();
                let log_path = dir.path().join("parent.log");
                let out = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(ROLE, "parent")
                    .env("VICTORY_LOG", "info")
                    .env("VICTORY_LOG_FILE", &log_path)
                    .output()
                    .unwrap();
                assert!(out.status.success(), "{out:?}");

                let stderr = String::from_utf8(out.stderr).unwrap();
                assert!(!stderr.contains('\x1b'), "{stderr:?}");
                assert!(stderr.contains("BEFORE"), "{stderr:?}");
                assert!(stderr.contains("CHILD"), "{stderr:?}");
                assert!(stderr.contains("AFTER"), "{stderr:?}");

                let file_bytes = std::fs::read(&log_path).unwrap();
                assert!(
                    !file_bytes.contains(&0),
                    "log file must not contain NUL gaps: {file_bytes:?}"
                );
                let file_text = String::from_utf8(file_bytes).unwrap();
                let lines: Vec<_> = file_text.lines().collect();
                assert_eq!(lines.len(), 2, "{file_text:?}");
                assert!(lines[0].contains("BEFORE"), "{file_text:?}");
                assert!(lines[1].contains("AFTER"), "{file_text:?}");
                assert!(!file_text.contains("CHILD"), "{file_text:?}");
            }
        }
    }
}
