//! How far the startup preparation is (`crate::startup`), for the progress
//! window and the window caption: the translation's and the pre-build's
//! progress lines (`crate::translate::report`) set it, and their summary
//! lines go into its log.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

/// Log lines kept for the progress window.
const LOG: usize = 12;

/// The preparation's state.
#[derive(Clone, Debug, Default)]
pub(crate) struct State {
    /// What is being done ("Translating shaders").
    pub(crate) phase: String,
    /// The file being worked through, and what its jobs are.
    pub(crate) file: String,
    pub(crate) what: String,
    /// Jobs of that file done, and in all.
    pub(crate) done: usize,
    pub(crate) total: usize,
    /// When the file's work started.
    pub(crate) started: Option<Instant>,
    /// The last lines said, oldest first.
    pub(crate) log: VecDeque<String>,
}

impl State {
    /// One line: `412 of 24,387 containers (1%), 12 s, about 9 min left`.
    pub(crate) fn line(&self) -> String {
        if self.total == 0 {
            return String::new();
        }
        let mut line = format!(
            "{} of {} {} ({}%)",
            self.done,
            self.total,
            self.what,
            self.done * 100 / self.total
        );
        if let Some(started) = self.started {
            let s = started.elapsed().as_secs_f64();
            line += &format!(", {}", duration(s));
            if self.done > 0 && self.done < self.total {
                let left = s / self.done as f64 * (self.total - self.done) as f64;
                line += &format!(", about {} left", duration(left));
            }
        }
        line
    }
}

/// Seconds as `42 s`, `12 min` or `2 h 5 min`.
fn duration(s: f64) -> String {
    match s as u64 {
        s @ 0..120 => format!("{s} s"),
        s @ 120..7200 => format!("{} min", s / 60),
        s => format!("{} h {} min", s / 3600, s / 60 % 60),
    }
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    f(STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(State::default))
}

/// A new phase begins.
pub(crate) fn phase(name: &str) {
    with(|s| {
        s.phase = name.to_owned();
        (s.file, s.what, s.done, s.total, s.started) = (String::new(), String::new(), 0, 0, None);
    });
    note(name);
}

/// `done` of `total` jobs (`what`) of `file` are done, started at `started`.
pub(crate) fn update(file: &Path, what: &str, done: usize, total: usize, started: Instant) {
    with(|s| {
        let name = file.file_name().unwrap_or(file.as_os_str());
        s.file = name.to_string_lossy().into_owned();
        s.what = what.to_owned();
        (s.done, s.total, s.started) = (done, total, Some(started));
    });
}

/// A line for the progress window's log.
pub(crate) fn note(line: &str) {
    with(|s| {
        if s.log.len() == LOG {
            s.log.pop_front();
        }
        s.log.push_back(line.to_owned());
    });
}

/// Prints `line` and keeps it for the progress window.
pub(crate) fn say(line: String) {
    println!("{line}");
    note(line.trim());
}

/// The state now.
pub(crate) fn snapshot() -> State {
    with(|s| s.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line says how far, how long, and how long is left.
    #[test]
    fn a_line_says_how_far_and_how_long_is_left() {
        let mut s = State {
            what: "pipelines".into(),
            done: 250,
            total: 1000,
            ..State::default()
        };
        assert_eq!(s.line(), "250 of 1000 pipelines (25%)");
        s.started = Some(Instant::now() - std::time::Duration::from_secs(60));
        let line = s.line();
        assert!(
            line.starts_with("250 of 1000 pipelines (25%), 60 s, about "),
            "{line}"
        );
        assert!(line.ends_with("3 min left"), "{line}");
        assert_eq!(State::default().line(), "");
    }

    #[test]
    fn durations_read_in_the_unit_that_fits() {
        assert_eq!(duration(42.0), "42 s");
        assert_eq!(duration(720.0), "12 min");
        assert_eq!(duration(7500.0), "2 h 5 min");
    }

    /// The log keeps its last lines, oldest first.
    #[test]
    fn the_log_keeps_its_last_lines() {
        for k in 0..LOG + 3 {
            note(&format!("line {k}"));
        }
        let log = snapshot().log;
        assert_eq!(log.len(), LOG);
        assert_eq!(
            log.back().map(String::as_str),
            Some(&*format!("line {}", LOG + 2))
        );
        assert_eq!(log.front().map(String::as_str), Some("line 3"));
    }
}
