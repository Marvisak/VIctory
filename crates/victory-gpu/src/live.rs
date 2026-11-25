//! Switches that can change while the title runs, so that two settings can
//! be compared in one session, in the same scene, without loading it again.
//!
//! A [`Live`] switch takes its value from its environment variable, as the
//! other switches do. With `VICTORY_LIVE=<file>` a thread reads that file
//! every quarter second; a `NAME=VALUE` line there overrides the variable
//! (`0` is off, anything else on; a name left out goes back to the
//! variable's value). Each change is logged ("live switch NAME=VALUE"), so a
//! log can be cut into the stretches each value held. A read that fails
//! (the file being rewritten, say) leaves the values as they are.
//!
//! Only switches whose code reads them afresh each time, with no state that
//! a change in the middle would leave inconsistent, are made live.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// How often the file is read.
const LOOK: Duration = Duration::from_millis(250);

/// A switch whose value can change while the title runs.
pub(crate) struct Live {
    name: &'static str,
    /// Its value with the variable unset.
    default: bool,
    /// 0 not read yet, 1 off, 2 on.
    state: AtomicU8,
    /// The environment's value, once read: what a name left out of the file
    /// goes back to.
    env: AtomicU8,
}

impl Live {
    pub(crate) const fn new(name: &'static str, default: bool) -> Self {
        Self { name, default, state: AtomicU8::new(0), env: AtomicU8::new(0) }
    }

    /// Whether the switch is on now: one atomic load once it has been read.
    pub(crate) fn on(&'static self) -> bool {
        match self.state.load(Ordering::Relaxed) {
            1 => false,
            2 => true,
            _ => self.first(),
        }
    }

    fn first(&'static self) -> bool {
        let env = match std::env::var_os(self.name) {
            Some(v) => v != "0",
            None => self.default,
        };
        self.env.store(code(env), Ordering::Relaxed);
        let mut live = lock();
        let now = live.values.get(self.name).copied().unwrap_or(env);
        if self.state.compare_exchange(0, code(now), Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            live.switches.push(self);
        }
        drop(live);
        watch();
        self.state.load(Ordering::Relaxed) == 2
    }
}

fn code(on: bool) -> u8 {
    if on { 2 } else { 1 }
}

/// The switches read so far, and the file's values by name.
#[derive(Default)]
struct State {
    switches: Vec<&'static Live>,
    values: HashMap<String, bool>,
}

fn lock() -> std::sync::MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// `NAME=VALUE` lines: `0` off, anything else on. Other lines are left out.
fn parse(text: &str) -> HashMap<String, bool> {
    text.lines()
        .filter_map(|line| {
            let (name, value) = line.split_once('=')?;
            let (name, value) = (name.trim(), value.trim());
            (!name.is_empty() && !name.starts_with('#')).then(|| (name.to_owned(), value != "0"))
        })
        .collect()
}

/// Applies the file's values to the switches read so far; logs each change,
/// and each value the file names anew even if the switch already had it (so
/// a log shows the state from the first line that names the switch).
fn apply(values: HashMap<String, bool>) {
    let mut live = lock();
    for s in &live.switches {
        let env = s.env.load(Ordering::Relaxed) == 2;
        let now = values.get(s.name).copied().unwrap_or(env);
        let named = values.get(s.name) != live.values.get(s.name);
        if s.state.swap(code(now), Ordering::Relaxed) != code(now) || named {
            tracing::info!("live switch {}={}", s.name, u8::from(now));
        }
    }
    live.values = values;
}

/// Starts the thread that reads `VICTORY_LIVE`, once, if it is set.
fn watch() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let Some(path) = std::env::var_os("VICTORY_LIVE") else {
            return;
        };
        let path = std::path::PathBuf::from(path);
        tracing::info!(path = %path.display(), "live switches read from a file");
        let _ = std::thread::Builder::new().name("victory-live".into()).spawn(move || {
            let mut last: Option<String> = None;
            loop {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    std::thread::sleep(LOOK);
                    continue;
                };
                if last.as_deref() != Some(text.as_str()) {
                    apply(parse(&text));
                    last = Some(text);
                }
                std::thread::sleep(LOOK);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_set_switches_on_and_off() {
        let v = parse("A=1\nB=0\n# C=1\nnot a line\n D = 2 \n=1\n");
        assert_eq!(v.get("A"), Some(&true));
        assert_eq!(v.get("B"), Some(&false));
        assert_eq!(v.get("D"), Some(&true));
        assert!(!v.contains_key("# C"));
        assert_eq!(v.len(), 3);
    }

    #[test]
    fn the_file_overrides_the_environment_and_leaving_a_name_out_goes_back() {
        static S: Live = Live::new("VICTORY_LIVE_TEST_UNSET", true);
        assert!(S.on(), "the default with the variable unset");
        apply(parse("VICTORY_LIVE_TEST_UNSET=0"));
        assert!(!S.on());
        apply(parse("VICTORY_LIVE_TEST_UNSET=1"));
        assert!(S.on());
        apply(parse("VICTORY_LIVE_TEST_UNSET=0"));
        assert!(!S.on());
        apply(parse(""));
        assert!(S.on(), "back to the environment's value");
    }
}
