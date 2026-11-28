//! More pipeline workers while little is drawn
//! (`VICTORY_SHADER_TURBO_PAUSE=1`; off when unset).
//!
//! The workers of [`super::make`] are half the logical processors, eight at
//! most, so that compiles leave the title its frame. A pause menu, a menu or
//! a loading screen draws next to nothing, and the processors the title then
//! leaves idle can compile: after [`QUIET_FRAMES`] presented frames of less
//! than [`BUSY_WORK`] draws and dispatches with pipelines waiting, or when
//! no frame was presented for [`STALL`] with more than [`STALL_BACKLOG`]
//! waiting, extra workers take jobs too. The first busy frame sends them
//! back: each finishes the pipeline it holds and parks.
//!
//! Here are the counts, the decisions and where workers wait, apart from the
//! threads that compile, so they are tested wherever the crate builds.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Draws and dispatches, run or dropped, from which a frame is gameplay.
pub(crate) const BUSY_WORK: u64 = 400;
/// Quiet frames in a row before turbo (one busy frame ends it).
pub(crate) const QUIET_FRAMES: u32 = 8;
/// No frame presented for this long is a loading stall...
pub(crate) const STALL: Duration = Duration::from_secs(1);
/// ...if more pipelines than this wait.
pub(crate) const STALL_BACKLOG: u64 = 32;
/// Turbo workers at most, in all.
const MOST: usize = 32;

/// Workers at all times: half the logical processors, 2 to 8.
pub(crate) fn base_workers(processors: Option<usize>) -> usize {
    processors.map_or(2, |n| (n / 2).clamp(2, 8))
}

/// Workers in all while turbo is on: all logical processors but two (left to
/// the system and the title's menu), or `asked`
/// (`VICTORY_SHADER_TURBO_THREADS`); never fewer than [`base_workers`] nor
/// more than [`MOST`].
pub(crate) fn turbo_workers(processors: Option<usize>, asked: Option<&str>) -> usize {
    let base = base_workers(processors);
    asked
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| processors.map_or(base, |n| n.saturating_sub(2)))
        .clamp(base, MOST)
}

/// A switch of turbo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    On,
    Off,
}

/// What the title draws, and whether turbo is on for it.
pub(crate) struct Load {
    on: bool,
    /// Presented frames in a row below [`BUSY_WORK`].
    quiet: u32,
    /// The draws and dispatches so far at the last presented frame.
    seen: u64,
    presented: Option<Instant>,
}

impl Load {
    pub(crate) const fn new() -> Self {
        Self {
            on: false,
            quiet: 0,
            seen: 0,
            presented: None,
        }
    }

    pub(crate) fn on(&self) -> bool {
        self.on
    }

    /// A frame was presented: the title has asked for `work_so_far` draws
    /// and dispatches since the start, and `waiting` pipelines are not made.
    pub(crate) fn frame(&mut self, work_so_far: u64, waiting: u64, now: Instant) -> Option<Change> {
        let work = work_so_far.saturating_sub(self.seen);
        self.seen = work_so_far;
        self.presented = Some(now);
        if work >= BUSY_WORK {
            self.quiet = 0;
            return std::mem::take(&mut self.on).then_some(Change::Off);
        }
        self.quiet = self.quiet.saturating_add(1);
        if self.on || self.quiet < QUIET_FRAMES || waiting == 0 {
            return None;
        }
        self.on = true;
        Some(Change::On)
    }

    /// Asked between frames, with `waiting` pipelines not made: a loading
    /// stall, where no frame says what is drawn, switches turbo on too.
    /// (Before the first frame the title is loading.)
    pub(crate) fn backlog(&mut self, waiting: u64, now: Instant) -> Option<Change> {
        let presenting = self
            .presented
            .is_some_and(|at| now.saturating_duration_since(at) < STALL);
        if self.on || waiting <= STALL_BACKLOG || presenting {
            return None;
        }
        self.on = true;
        Some(Change::On)
    }
}

/// Where workers wait for jobs. The extra ones wait apart from the base
/// ones, so that the notice of a job is never taken by a worker that may not
/// take the job.
pub(crate) struct Signals {
    base: Condvar,
    extra: Condvar,
}

impl Signals {
    pub(crate) const fn new() -> Self {
        Self {
            base: Condvar::new(),
            extra: Condvar::new(),
        }
    }

    /// A job was queued; `turbo`: the extra workers take jobs.
    pub(crate) fn queued(&self, turbo: bool) {
        self.base.notify_one();
        if turbo {
            self.extra.notify_one();
        }
    }

    /// Turbo was switched on: every extra worker looks at the queue. (Off
    /// needs no notice: they ask before each job.)
    pub(crate) fn engaged(&self) {
        self.extra.notify_all();
    }

    /// Waits until `take` gives a base or an `extra` worker a job. `take`
    /// gives an extra worker none while turbo is off. A base worker that has
    /// waited `idle` for nothing calls `on_idle`, without the queue.
    pub(crate) fn next<Q, J>(
        &self,
        queue: &Mutex<Q>,
        extra: bool,
        idle: Duration,
        mut take: impl FnMut(&mut Q) -> Option<J>,
        mut on_idle: impl FnMut(),
    ) -> J {
        let wake = if extra { &self.extra } else { &self.base };
        let mut held = queue.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(job) = take(&mut held) {
                return job;
            }
            if extra {
                held = wake.wait(held).unwrap_or_else(|e| e.into_inner());
                continue;
            }
            let (back, waited) = wake.wait_timeout(held, idle).unwrap_or_else(|e| e.into_inner());
            held = back;
            if waited.timed_out() {
                drop(held);
                on_idle();
                held = queue.lock().unwrap_or_else(|e| e.into_inner());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::mpsc;

    #[test]
    fn workers_for_the_processors_there_are() {
        // processors, base, turbo
        for (processors, base, turbo) in [
            (1, 2, 2),
            (2, 2, 2),
            (4, 2, 2),
            (6, 3, 4),
            (8, 4, 6),
            (12, 6, 10),
            (16, 8, 14),
            (24, 8, 22),
            (32, 8, 30),
            (64, 8, 32),
            (128, 8, 32),
        ] {
            assert_eq!(base_workers(Some(processors)), base, "{processors} processors");
            assert_eq!(turbo_workers(Some(processors), None), turbo, "{processors} processors");
        }
        assert_eq!(base_workers(None), 2);
        assert_eq!(turbo_workers(None, None), 2);
    }

    #[test]
    fn the_asked_thread_count_stays_between_base_and_most() {
        assert_eq!(turbo_workers(Some(24), Some("16")), 16);
        assert_eq!(turbo_workers(Some(24), Some(" 24 ")), 24);
        // Fewer than gameplay's workers: no turbo, never fewer workers.
        assert_eq!(turbo_workers(Some(24), Some("3")), 8);
        assert_eq!(turbo_workers(Some(24), Some("500")), 32);
        for ignored in ["0", "", "-4", "many"] {
            assert_eq!(turbo_workers(Some(24), Some(ignored)), 22, "{ignored:?}");
        }
    }

    /// Frames of a title, one every 50 ms, each of `work` draws.
    struct Title {
        load: Load,
        now: Instant,
        work: u64,
    }

    impl Title {
        fn new() -> Self {
            Self {
                load: Load::new(),
                now: Instant::now(),
                work: 0,
            }
        }

        fn frame(&mut self, work: u64, waiting: u64) -> Option<Change> {
            self.work += work;
            self.now += Duration::from_millis(50);
            self.load.frame(self.work, waiting, self.now)
        }
    }

    #[test]
    fn gameplay_pause_gameplay() {
        let mut title = Title::new();
        for _ in 0..20 {
            assert_eq!(title.frame(3_000, 200), None);
        }
        assert!(!title.load.on());
        // The pause menu: on at the eighth quiet frame, not before.
        for _ in 1..QUIET_FRAMES {
            assert_eq!(title.frame(40, 200), None);
        }
        assert_eq!(title.frame(40, 200), Some(Change::On));
        assert!(title.load.on());
        // On stays on, also while nothing waits for a moment.
        assert_eq!(title.frame(40, 150), None);
        assert_eq!(title.frame(40, 0), None);
        assert!(title.load.on());
        // The first busy frame ends it, once.
        assert_eq!(title.frame(BUSY_WORK, 100), Some(Change::Off));
        assert!(!title.load.on());
        assert_eq!(title.frame(3_000, 100), None);
        // And the count of quiet frames starts again.
        for _ in 1..QUIET_FRAMES {
            assert_eq!(title.frame(40, 100), None);
        }
        assert_eq!(title.frame(BUSY_WORK - 1, 100), Some(Change::On));
    }

    #[test]
    fn single_quiet_frames_in_gameplay_change_nothing() {
        let mut title = Title::new();
        for i in 0..200 {
            let work = if i % u64::from(QUIET_FRAMES) == 0 { 2_000 } else { 0 };
            assert_eq!(title.frame(work, 500), None, "frame {i}");
        }
    }

    #[test]
    fn quiet_frames_without_waiting_pipelines_change_nothing() {
        let mut title = Title::new();
        for _ in 0..100 {
            assert_eq!(title.frame(10, 0), None);
        }
        // The first pipeline then queued in the menu is enough.
        assert_eq!(title.frame(10, 1), Some(Change::On));
    }

    #[test]
    fn a_loading_stall_with_a_backlog_switches_on() {
        // Before the first frame: loading.
        let mut load = Load::new();
        let start = Instant::now();
        assert_eq!(load.backlog(STALL_BACKLOG, start), None);
        assert_eq!(load.backlog(STALL_BACKLOG + 1, start), Some(Change::On));
        assert_eq!(load.backlog(STALL_BACKLOG + 1, start), None);

        // Between frames of gameplay: nothing, whatever waits.
        let mut title = Title::new();
        assert_eq!(title.frame(3_000, 1_000), None);
        assert_eq!(title.load.backlog(1_000, title.now + STALL - Duration::from_millis(1)), None);
        // No frame for a while: a small backlog is left to the base workers.
        assert_eq!(title.load.backlog(STALL_BACKLOG, title.now + STALL), None);
        assert_eq!(title.load.backlog(1_000, title.now + STALL), Some(Change::On));
        // A quiet frame keeps it, the first busy one ends it.
        title.now += STALL;
        assert_eq!(title.frame(5, 900), None);
        assert!(title.load.on());
        assert_eq!(title.frame(3_000, 900), Some(Change::Off));
    }

    /// The threads' rule: an extra worker takes nothing while turbo is off,
    /// takes jobs once it is switched on, and parks again after it.
    #[test]
    fn an_extra_worker_takes_jobs_only_while_turbo_is_on() {
        type Queue = (VecDeque<u32>, bool);
        let queue: Arc<Mutex<Queue>> = Arc::default();
        let signals = Arc::new(Signals::new());
        let (taken, took) = mpsc::channel();
        {
            let (queue, signals) = (queue.clone(), signals.clone());
            // Left parked when the test ends.
            std::thread::spawn(move || {
                loop {
                    let take = |q: &mut Queue| if q.1 { q.0.pop_front() } else { None };
                    let job = signals.next(&queue, true, Duration::from_secs(60), take, || {});
                    if taken.send(job).is_err() {
                        break;
                    }
                }
            });
        }
        let push = |job| {
            let mut q = queue.lock().unwrap();
            q.0.push_back(job);
            signals.queued(q.1);
        };
        let turbo = |on| {
            queue.lock().unwrap().1 = on;
            if on {
                signals.engaged();
            }
        };
        let quiet = Duration::from_millis(100);
        let soon = Duration::from_secs(10);

        push(1);
        assert!(took.recv_timeout(quiet).is_err());
        turbo(true);
        assert_eq!(took.recv_timeout(soon), Ok(1));
        push(2);
        assert_eq!(took.recv_timeout(soon), Ok(2));
        turbo(false);
        push(3);
        assert!(took.recv_timeout(quiet).is_err());
        assert_eq!(queue.lock().unwrap().0.len(), 1);
        // A base worker takes what the extra one left.
        let take = |q: &mut Queue| q.0.pop_front();
        assert_eq!(signals.next(&queue, false, Duration::from_secs(60), take, || {}), 3);
    }

    #[test]
    fn an_idle_base_worker_says_so_without_the_queue() {
        let queue: Mutex<VecDeque<u32>> = Mutex::default();
        let job = Signals::new().next(&queue, false, Duration::from_millis(1), VecDeque::pop_front, || {
            queue.try_lock().expect("not held").push_back(7);
        });
        assert_eq!(job, 7);
    }
}
