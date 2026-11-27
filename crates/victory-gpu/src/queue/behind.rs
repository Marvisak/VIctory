//! Submissions and presents prepared and sent by a thread of ours, in the
//! order the title made them (`VICTORY_ASYNC_SUBMIT=1`, or `VICTORY_THIN=1`
//! which needs the lane). The title's submit and present calls only put work
//! in the line; `victory-submit` takes the oldest work that is free to run.
//! Waits for fences and memory values hold their own queue, a present goes
//! behind everything given before it, and nothing is sent before the lists
//! written down by the translation workers have been translated.
//!
//! The switch is off by default: with neither variable set, no thread is
//! started and queue calls behave as they did before.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use super::This;

/// One thing in the line.
pub(super) trait Work: Send {
    type Ask;
    fn ask(&self) -> Option<Self::Ask>;
    fn met(ask: &Self::Ask) -> bool;
    /// Whether it is a present: everything before it goes first, met or not.
    fn shows(&self) -> bool;
    /// Whether it is a submission.
    fn runs(&self) -> bool;
    /// Does it. `forced`: its wait was not met at a present.
    fn go(self, forced: bool);
}

struct Entry<W> {
    number: u64,
    queue: usize,
    what: W,
}

struct State<W> {
    items: VecDeque<Entry<W>>,
    next: u64,
    running: Option<usize>,
    asked: u64,
    served: u64,
    presents: usize,
    runs: usize,
}

/// The line, in the order of the title's calls whatever queue they came on.
pub(super) struct Lane<W: Work> {
    state: Mutex<State<W>>,
    work: Condvar,
    moved: Condvar,
}

impl<W: Work> Lane<W> {
    pub(super) const fn new() -> Self {
        Lane {
            state: Mutex::new(State {
                items: VecDeque::new(),
                next: 0,
                running: None,
                asked: 0,
                served: 0,
                presents: 0,
                runs: 0,
            }),
            work: Condvar::new(),
            moved: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<W>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn put(state: &mut State<W>, queue: usize, what: W) {
        state.presents += usize::from(what.shows());
        state.runs += usize::from(what.runs());
        let number = state.next;
        state.next += 1;
        state.items.push_back(Entry { number, queue, what });
    }

    pub(super) fn push(&self, queue: usize, what: impl IntoIterator<Item = W>) {
        let mut state = self.lock();
        for what in what {
            Self::put(&mut state, queue, what);
        }
        state.asked += 1;
        drop(state);
        self.work.notify_one();
    }

    /// Adds `what` if `queue` has anything in the line (or `always`), else
    /// does `now`, with nothing of the queue able to come in between.
    pub(super) fn push_behind(&self, queue: usize, always: bool, what: W, now: impl FnOnce()) -> bool {
        let mut state = self.lock();
        let queued = always || state.running == Some(queue) || state.items.iter().any(|e| e.queue == queue);
        if queued {
            Self::put(&mut state, queue, what);
        } else {
            now();
        }
        state.asked += 1;
        drop(state);
        self.work.notify_one();
        queued
    }

    pub(super) fn wake(&self) {
        let mut state = self.lock();
        if state.items.is_empty() {
            return;
        }
        state.asked += 1;
        drop(state);
        self.work.notify_one();
    }

    /// Waits until all free work put in the line before this call is done.
    pub(super) fn drain(&self) {
        let mut state = self.lock();
        if state.items.is_empty() && state.running.is_none() {
            return;
        }
        state.asked += 1;
        let ticket = state.asked;
        self.work.notify_one();
        while state.served < ticket {
            state = self.moved.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    pub(super) fn wait_presents(&self, most: usize) {
        let mut state = self.lock();
        while state.presents > most {
            state = self.moved.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    pub(super) fn wait_runs(&self, most: usize) {
        let mut state = self.lock();
        while state.runs > most && state.served < state.asked {
            state = self.moved.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn serve(&self) -> ! {
        loop {
            let (asked, limit) = {
                let mut state = self.lock();
                while state.asked == state.served {
                    if state.items.is_empty() {
                        state = self.work.wait(state).unwrap_or_else(|e| e.into_inner());
                    } else {
                        // A wait may be met by the CPU without a call of ours.
                        let (back, out) = self
                            .work
                            .wait_timeout(state, Duration::from_millis(1))
                            .unwrap_or_else(|e| e.into_inner());
                        state = back;
                        if out.timed_out() {
                            break;
                        }
                    }
                }
                (state.asked, state.next)
            };
            self.look(limit);
            let mut state = self.lock();
            state.served = state.served.max(asked);
            drop(state);
            self.moved.notify_all();
        }
    }

    pub(super) fn look(&self, limit: u64) {
        while self.step(limit) {}
    }

    /// Takes the oldest entry that is the first of its queue and free to run.
    fn step(&self, limit: u64) -> bool {
        let mut heads: Vec<(u64, usize, Option<W::Ask>, bool)> = Vec::new();
        {
            let state = self.lock();
            for e in state.items.iter().take_while(|e| e.number < limit) {
                if !heads.iter().any(|h| h.1 == e.queue) {
                    heads.push((e.number, e.queue, e.what.ask(), e.what.shows()));
                }
            }
        }
        let Some(&(first, ..)) = heads.first() else { return false };
        let ready = heads.iter().find(|(number, _, ask, shows)| match ask {
            Some(ask) => W::met(ask),
            // A present comes behind everything given before it; without
            // one, an entry with no wait runs only when it is the oldest.
            None => !shows || *number == first,
        });
        let (number, forced) = match ready {
            Some(head) => (head.0, false),
            None => {
                let state = self.lock();
                if !state.items.iter().any(|e| e.number < limit && e.what.shows()) {
                    return false;
                }
                (first, true)
            }
        };
        let entry = {
            let mut state = self.lock();
            let Some(at) = state.items.iter().position(|e| e.number == number) else {
                return false;
            };
            let Some(entry) = state.items.remove(at) else { return false };
            state.running = Some(entry.queue);
            entry
        };
        let (shows, runs) = (entry.what.shows(), entry.what.runs());
        entry.what.go(forced);
        let mut state = self.lock();
        state.running = None;
        state.presents -= usize::from(shows);
        state.runs -= usize::from(runs);
        drop(state);
        self.moved.notify_all();
        true
    }
}

/// What the title's queues put in the line.
enum Job {
    Wait(usize, u64),
    Value(crate::hold::ValueWait),
    Run(usize, Vec<std::sync::Arc<crate::host::HostList>>),
    Signal(usize, u64),
    Present(Vec<(windows::Win32::Graphics::Direct3D12::ID3D12Resource, u32)>, super::Shown),
}

// SAFETY: D3D12 objects are free-threaded; a job is used by whoever put it
// in the line, then by our thread alone.
unsafe impl Send for Job {}

enum Ask {
    Wait(usize, u64),
    Value(crate::hold::ValueWait),
}

impl Work for Job {
    type Ask = Ask;

    fn ask(&self) -> Option<Ask> {
        match self {
            &Job::Wait(fence, value) => Some(Ask::Wait(fence, value)),
            Job::Value(wait) => Some(Ask::Value(*wait)),
            _ => None,
        }
    }

    fn met(ask: &Ask) -> bool {
        match ask {
            &Ask::Wait(fence, value) => crate::fence::value(fence as This) >= value,
            Ask::Value(wait) => super::value_met(wait),
        }
    }

    fn shows(&self) -> bool {
        matches!(self, Job::Present(..))
    }

    fn runs(&self) -> bool {
        matches!(self, Job::Run(..))
    }

    fn go(self, _forced: bool) {
        let began = Instant::now();
        match self {
            Job::Wait(..) | Job::Value(..) => {}
            Job::Run(queue, lists) => {
                let refs: Vec<_> = lists.iter().map(|l| &**l).collect();
                if let Some(host) = crate::host::host() {
                    host.execute(&refs);
                }
                drop(refs);
                for list in lists {
                    crate::list::recycle(list);
                }
                let _ = queue;
                QUEUED.fetch_sub(1, Relaxed);
            }
            Job::Signal(fence, value) => {
                crate::fence::set(fence as This, value);
                crate::fence::let_go(fence as This);
            }
            Job::Present(texture, shown) => {
                super::show(&texture, shown);
                QUEUED.fetch_sub(1, Relaxed);
            }
        }
        STATS[BUSY].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    }
}

static LANE: Lane<Job> = Lane::new();
/// Submissions and presents the host has not begun, also counted by a
/// resource's retirement.
static QUEUED: AtomicU64 = AtomicU64::new(0);
static STATS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
const RUNS: usize = 0;
const PRESENTS: usize = 1;
const DEPTH: usize = 2;
const DEEPEST: usize = 3;
const ROOM: usize = 4;
const AT_PRESENT: usize = 5;
const DRAINING: usize = 6;
const DRAINS: usize = 7;
const BUSY: usize = 8;
const FORCED: usize = 9;

fn number(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn runs_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| number("VICTORY_SUBMIT_DEPTH").unwrap_or(64).max(1))
}

fn frames_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| number("VICTORY_SUBMIT_FRAMES").unwrap_or(1))
}

/// How many parts of a submission, and lists in them, the lane gives to the
/// host as one (`VICTORY_JOIN_PARTS`, `VICTORY_JOIN_LISTS`; both off by
/// default). Parts with waits in between are never joined.
fn joins() -> (usize, usize) {
    static MOST: OnceLock<(usize, usize)> = OnceLock::new();
    *MOST.get_or_init(|| {
        (
            number("VICTORY_JOIN_PARTS").unwrap_or(1).max(1),
            number("VICTORY_JOIN_LISTS").unwrap_or(0),
        )
    })
}

thread_local! {
    static OURS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the lane is serving submissions. `VICTORY_THIN=1` turns it on too:
/// translations hand their lists to it when they are done.
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let wanted = std::env::var_os("VICTORY_ASYNC_SUBMIT").is_some_and(|v| v == "1")
            || crate::list::thin::wanted();
        if !wanted {
            return false;
        }
        let worker = || {
            crate::cores::keep_on_fast_cores("victory-submit");
            OURS.set(true);
            let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| LANE.serve()));
            if ended.is_err() {
                crate::host::fail(format_args!("the thread that prepares and sends submissions panicked"));
            }
        };
        match std::thread::Builder::new().name("victory-submit".into()).spawn(worker) {
            Ok(_) => {
                tracing::info!(
                    frames = frames_most(),
                    runs = runs_most(),
                    "submissions are prepared and sent by our own thread (VICTORY_ASYNC_SUBMIT=1)"
                );
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "no thread to prepare submissions: the threads that make them do");
                false
            }
        }
    })
}

pub(super) fn queued() -> u64 {
    QUEUED.load(Relaxed)
}

pub(super) fn wake() {
    LANE.wake();
}

/// Before something that must see the submissions made so far sent. Not on
/// our own thread, which is inside one of them.
pub(super) fn drain() {
    if OURS.get() {
        return;
    }
    let began = Instant::now();
    LANE.drain();
    STATS[DRAINING].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    STATS[DRAINS].fetch_add(1, Relaxed);
}

pub(super) fn no_drain_here() {
    OURS.set(true);
}

/// Parts of `lists`, split at their waits. With thin lists the title's calls
/// were translated on our threads first: `Recording::wait` is the hand-over.
fn parts(lists: &[This]) -> crate::list::Parts {
    if crate::list::thin::on() {
        let recordings = crate::list::thin::submitted(lists);
        let backs: Vec<This> = recordings
            .iter()
            .map(|r| {
                r.wait();
                r.back()
            })
            .filter(|b| !b.is_null())
            .collect();
        crate::list::detach_parts(&backs)
    } else {
        crate::list::detach_parts(lists)
    }
}

/// Joins consecutive runs of the same queue that have no wait between them.
fn coalesce(jobs: &mut Vec<Job>, parts: usize, lists: usize) {
    if parts <= 1 {
        return;
    }
    let mut joined: Vec<Job> = Vec::with_capacity(jobs.len());
    for job in jobs.drain(..) {
        let merge = match (&mut joined.last_mut(), &job) {
            (Some(Job::Run(queue, ahead)), Job::Run(next_queue, next)) => {
                *queue == *next_queue
                    && ahead.len() < parts
                    && (lists == 0 || ahead.len() + next.len() <= lists)
            }
            _ => false,
        };
        if let (true, Some(Job::Run(_, ahead))) = (merge, joined.last_mut()) {
            match job {
                Job::Run(_, next) => ahead.extend(next),
                _ => unreachable!(),
            }
        } else {
            joined.push(job);
        }
    }
    *jobs = joined;
}

pub(super) fn submit(this: This, lists: &[This]) {
    let queue = this as usize;
    let mut jobs: Vec<Job> = Vec::new();
    for (waits, taken) in parts(lists) {
        jobs.extend(waits.into_iter().map(Job::Value));
        if !taken.is_empty() {
            jobs.push(Job::Run(queue, taken));
        }
    }
    let (join_parts, join_lists) = joins();
    coalesce(&mut jobs, join_parts, join_lists);
    let runs = jobs.iter().filter(|j| j.runs()).count() as u64;
    let before = QUEUED.fetch_add(runs, Relaxed);
    STATS[RUNS].fetch_add(runs, Relaxed);
    STATS[DEPTH].fetch_add(before, Relaxed);
    STATS[DEEPEST].fetch_max(before + runs, Relaxed);
    LANE.push(queue, jobs);
    if before + runs > runs_most() as u64 {
        let began = Instant::now();
        LANE.wait_runs(runs_most());
        STATS[ROOM].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    }
}

pub(super) fn signal(this: This, fence: This, value: u64) {
    let at = fence as usize;
    let queued = LANE.push_behind(this as usize, false, Job::Signal(at, value), || {
        crate::fence::set(fence, value);
    });
    if queued {
        crate::fence::keep(fence);
    }
}

pub(super) fn wait(this: This, fence: This, value: u64) {
    let always = crate::fence::value(fence) < value;
    LANE.push_behind(this as usize, always, Job::Wait(fence as usize, value), || {});
}

pub(super) fn present(
    this: This,
    texture: Vec<(windows::Win32::Graphics::Direct3D12::ID3D12Resource, u32)>,
    shown: super::Shown,
) {
    QUEUED.fetch_add(1, Relaxed);
    STATS[PRESENTS].fetch_add(1, Relaxed);
    LANE.push(this as usize, [Job::Present(texture, shown)]);
    let began = Instant::now();
    LANE.wait_presents(frames_most());
    STATS[AT_PRESENT].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
}

pub(super) fn line(frames: f64) {
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Relaxed) as f64).collect();
    let ms = |i: usize| s[i] / 1000.0 / frames;
    tracing::info!(
        "submissions prepared by our thread per frame (VICTORY_ASYNC_SUBMIT): {:.1} in {:.1} presents, {:.1} in the \
         line when one was added (at most {:.0}); that thread worked {:.1} ms; threads of the title waited {:.1} ms \
         for room in the line, {:.1} ms at presents, {:.1} ms in {:.1} drains; waits not met {:.2}",
        s[RUNS] / frames,
        s[PRESENTS] / frames,
        s[DEPTH] / s[RUNS].max(1.0),
        s[DEEPEST],
        ms(BUSY),
        ms(ROOM),
        ms(AT_PRESENT),
        ms(DRAINING),
        s[DRAINS] / frames,
        s[FORCED] / frames
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    type Log = Arc<Mutex<Vec<String>>>;

    enum T {
        Wait(usize, u64),
        Run(&'static str, Log),
        Slow(&'static str, Log, Arc<(Mutex<bool>, Condvar)>),
        Signal(usize, u64, Log),
        Present(&'static str, Log),
    }

    impl Work for T {
        type Ask = (usize, u64);
        fn ask(&self) -> Option<(usize, u64)> {
            match self {
                &T::Wait(fence, value) => Some((fence, value)),
                _ => None,
            }
        }
        fn met(ask: &(usize, u64)) -> bool {
            crate::fence::value(ask.0 as This) >= ask.1
        }
        fn shows(&self) -> bool {
            matches!(self, T::Present(..))
        }
        fn runs(&self) -> bool {
            matches!(self, T::Run(..) | T::Slow(..))
        }
        fn go(self, forced: bool) {
            match self {
                T::Wait(..) => {
                    if forced {
                        STATS[FORCED].fetch_add(1, Relaxed);
                    }
                }
                T::Run(name, log) | T::Present(name, log) => log.lock().unwrap().push(name.into()),
                T::Slow(name, log, gate) => {
                    let mut open = gate.0.lock().unwrap();
                    while !*open {
                        open = gate.1.wait(open).unwrap();
                    }
                    log.lock().unwrap().push(name.into());
                }
                T::Signal(fence, value, log) => {
                    log.lock().unwrap().push(format!("signal {value}"));
                    crate::fence::set(fence as This, value);
                }
            }
        }
    }

    fn fence() -> usize {
        let mut f = std::ptr::null();
        assert_eq!(
            crate::fence::create(std::ptr::null(), 0, 0, &crate::fence::FENCE_IIDS[0], &mut f),
            super::super::S_OK
        );
        f as usize
    }

    fn look(lane: &Lane<T>) {
        let limit = lane.lock().next;
        lane.look(limit);
    }

    /// A queue's order is kept, a wait holds only what is behind it on its
    /// own queue, and the oldest work goes first.
    #[test]
    fn queues_keep_their_order_across_waits() {
        let lane = Lane::new();
        let log: Log = Default::default();
        let f = fence();
        let run = |name| T::Run(name, log.clone());
        lane.push(2, [T::Wait(f, 1), run("compute")]);
        lane.push(1, [run("draw 1")]);
        assert!(lane.push_behind(1, false, T::Signal(f, 1, log.clone()), || panic!("queue not empty")));
        lane.push(1, [run("draw 2")]);
        look(&lane);
        assert_eq!(*log.lock().unwrap(), ["draw 1", "signal 1", "compute", "draw 2"]);
        assert_eq!(crate::fence::value(f as This), 1);
        let mut done = false;
        assert!(!lane.push_behind(1, false, T::Signal(f, 2, log.clone()), || done = true));
        assert!(done);
    }

    /// A present goes behind everything before it, met or not, and a drain
    /// comes back only when the free work has been done.
    #[test]
    fn presents_and_drains_keep_the_frame_order() {
        let lane: &'static Lane<T> = Box::leak(Box::new(Lane::new()));
        std::thread::spawn(|| lane.serve());
        let log: Log = Default::default();
        let f = fence();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        lane.push(2, [T::Wait(f, 9), T::Run("held", log.clone())]);
        lane.push(1, [T::Slow("slow", log.clone(), gate.clone())]);
        lane.push(1, [T::Present("present 1", log.clone())]);
        lane.wait_presents(1);
        assert!(log.lock().unwrap().is_empty());
        while lane.lock().running.is_none() {
            std::thread::yield_now();
        }
        lane.push(1, [T::Run("second", log.clone())]);
        lane.push(1, [T::Present("present 2", log.clone())]);
        let opener = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                *gate.0.lock().unwrap() = true;
                gate.1.notify_all();
            })
        };
        lane.wait_presents(1);
        lane.drain();
        assert_eq!(*log.lock().unwrap(), ["slow", "held", "present 1", "second", "present 2"]);
        assert_eq!(STATS[FORCED].load(Relaxed), 1);
        opener.join().unwrap();
    }
}
