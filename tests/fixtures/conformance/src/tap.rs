//! Test Anything Protocol output.

use std::sync::Mutex;

#[derive(Default)]
pub struct Tap {
    lines: Vec<String>,
    n: usize,
    failed: usize,
}

impl Tap {
    /// One check: `ok` or `not ok`, with what was seen when it failed.
    pub fn check(&mut self, ok: bool, name: &str, detail: impl FnOnce() -> String) {
        self.n += 1;
        if ok {
            self.lines.push(format!("ok {} - {name}", self.n));
        } else {
            self.failed += 1;
            self.lines.push(format!("not ok {} - {name}", self.n));
            for line in detail().lines() {
                self.lines.push(format!("  # {line}"));
            }
        }
    }

    pub fn eq<T: PartialEq + std::fmt::Debug>(&mut self, seen: T, want: T, name: &str) {
        let ok = seen == want;
        self.check(ok, name, || format!("seen {seen:?}, want {want:?}"));
    }

    pub fn finish(self) -> &'static str {
        static REPORT: Mutex<String> = Mutex::new(String::new());
        let mut report = REPORT.lock().unwrap_or_else(|e| e.into_inner());
        *report = format!(
            "TAP version 13\n1..{}\n{}\n# failed {}\n",
            self.n,
            self.lines.join("\n"),
            self.failed
        );
        // SAFETY: the string lives in a static and is replaced only by the
        // next run, which the harness does not start while reading this one.
        unsafe { &*(report.as_str() as *const str) }
    }
}
