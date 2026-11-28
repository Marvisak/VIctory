use std::collections::{HashSet, VecDeque};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, OnceLock};

use crate::host::HostList;

pub(crate) type Work = Vec<(Arc<crate::texture::Staging>, Arc<crate::resource::HostTexture>)>;

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_WORKER").is_some_and(|v| v == "1"))
}

struct Job {
    work: Work,
    done: mpsc::SyncSender<Ready>,
}

// SAFETY: D3D12 objects and the textures held in `work` are free-threaded;
// each job is owned by one worker at a time.
unsafe impl Send for Job {}

pub(crate) struct Ready {
    pub(crate) list: HostList,
    work: Work,
}

// SAFETY: the closed list and the textures in `work` are free-threaded; each
// ready upload is owned by one submission at a time.
unsafe impl Send for Ready {}

struct QueueState<T> {
    jobs: VecDeque<T>,
    stop: bool,
    drain: bool,
}

#[allow(dead_code)]
pub(crate) struct Queue<T> {
    state: Arc<(Mutex<QueueState<T>>, Condvar)>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[allow(dead_code)]
impl<T: Send + 'static> Queue<T> {
    pub(crate) fn new(work: impl Fn(T) + Send + 'static) -> Self {
        let state = Arc::new((Mutex::new(QueueState { jobs: VecDeque::new(), stop: false, drain: false }), Condvar::new()));
        let running = state.clone();
        let thread = std::thread::Builder::new()
            .name("victory-texture-worker".into())
            .spawn(move || loop {
                let next = {
                    let (lock, wake) = &*running;
                    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while state.jobs.is_empty() && !state.stop {
                        state = wake.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                    if state.stop && !state.drain {
                        state.jobs.clear();
                        return;
                    }
                    match state.jobs.pop_front() {
                        Some(job) => job,
                        None => return,
                    }
                };
                work(next);
            })
            .expect("texture worker thread");
        Queue { state, thread: Mutex::new(Some(thread)) }
    }

    pub(crate) fn push(&self, job: T) {
        let (lock, wake) = &*self.state;
        lock.lock().unwrap_or_else(|e| e.into_inner()).jobs.push_back(job);
        wake.notify_one();
    }

    pub(crate) fn stopping(&self) -> bool {
        let (lock, _) = &*self.state;
        lock.lock().unwrap_or_else(|e| e.into_inner()).stop
    }

    pub(crate) fn shutdown(&self, drain: bool) {
        {
            let (lock, wake) = &*self.state;
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            state.stop = true;
            state.drain = drain;
            wake.notify_all();
        }
        if let Some(thread) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = thread.join();
        }
    }
}

static WORKER: OnceLock<Queue<Job>> = OnceLock::new();
static PENDING: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
static SPARE: LazyLock<Mutex<Vec<HostList>>> = LazyLock::new(|| Mutex::new(Vec::new()));

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn prepare(work: Work) -> Ready {
    let list = lock(&SPARE).pop().or_else(|| crate::host::host().and_then(|h| h.new_list().ok()));
    let list = list.expect("a list for async texture uploads");
    list.set_ours();
    let mut readable = Vec::new();
    for ((staging, copy), upload) in work.iter().zip(crate::texture::refresh_all(&work)) {
        if let Some(upload) = upload
            && let Some(barrier) = crate::texture::record_upload(&list, copy, staging, &upload)
        {
            readable.push(barrier);
        }
    }
    if !readable.is_empty() {
        // SAFETY: a list we opened and close below; the barriers name live
        // textures held by `work`.
        unsafe { list.list.ResourceBarrier(&readable) };
    }
    list.close();
    Ready { list, work }
}

/// Prepares the upload on the worker and waits for it before the submission
/// runs, so a draw cannot sample a texture whose copy is not in a list yet.
pub(crate) fn submit(work: Work) -> Vec<Ready> {
    if work.is_empty() {
        return Vec::new();
    }
    let (done, received) = mpsc::sync_channel(1);
    let queue = WORKER.get_or_init(|| {
        Queue::new(|job: Job| {
            let ready = prepare(job.work);
            let _ = job.done.send(ready);
        })
    });
    let keys: Vec<_> = work.iter().map(|(staging, _)| Arc::as_ptr(staging) as usize).collect();
    let mut pending = lock(&PENDING);
    for key in &keys {
        pending.insert(*key);
    }
    drop(pending);
    queue.push(Job { work, done });
    match received.recv() {
        Ok(ready) => vec![ready],
        Err(_) => {
            let mut pending = lock(&PENDING);
            for key in keys {
                pending.remove(&key);
            }
            Vec::new()
        }
    }
}

/// After the submission that ran the upload has finished: the staging it
/// used is free and its list can record the next job.
pub(crate) fn recycle(ready: Ready) {
    let Ready { list, work } = ready;
    let mut pending = lock(&PENDING);
    for (staging, _) in &work {
        pending.remove(&(Arc::as_ptr(staging) as usize));
    }
    drop(pending);
    let mut spare = lock(&SPARE);
    if spare.len() < 16 {
        list.reset();
        spare.push(list);
    }
}

pub(crate) fn pending(staging: usize) -> bool {
    lock(&PENDING).contains(&staging)
}

#[cfg(test)]
pub(crate) fn ready_for_test(list: HostList, work: Work) -> Ready {
    Ready { list, work }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_jobs_are_drained_before_shutdown() {
        let (send, receive) = std::sync::mpsc::channel();
        let queue = Queue::new(move |n: u32| {
            let _ = send.send(n);
        });
        for n in 0..8 {
            queue.push(n);
        }
        queue.shutdown(true);
        assert_eq!(receive.iter().collect::<Vec<_>>(), (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn shutdown_without_drain_stops_the_worker_safely() {
        let (entered_send, entered) = std::sync::mpsc::channel();
        let (release_send, release) = std::sync::mpsc::channel();
        let (send, receive) = std::sync::mpsc::channel();
        let queue = Arc::new(Queue::new(move |n: u32| {
            if n == 0 {
                let _ = entered_send.send(());
                let _ = release.recv();
            } else {
                let _ = send.send(n);
            }
        }));
        queue.push(0);
        entered.recv().expect("the worker started the first job");
        queue.push(1);
        let stopped = queue.clone();
        let stopping = std::thread::spawn(move || {
            stopped.shutdown(false);
        });
        while !queue.stopping() {
            std::thread::yield_now();
        }
        let _ = release_send.send(());
        stopping.join().expect("shutdown does not panic");
        assert!(receive.try_recv().is_err());
    }

    #[test]
    fn staging_stays_alive_and_pending_until_the_upload_is_recycled() {
        if crate::host::host().is_none() {
            return; // no host GPU
        }
        let resource = crate::bindless::tests::resource(3, 4, 1, 28, 0x100);
        let Some(copy) = crate::resource::host_copy(resource) else {
            return;
        };
        let Some(staging) = crate::texture::staging(resource, &copy.texture) else {
            return;
        };
        let key = Arc::as_ptr(&staging) as usize;
        let before = Arc::strong_count(&staging);
        let ready = submit(vec![(staging.clone(), copy)]);
        assert!(!ready.is_empty(), "the prepared upload is returned before submission");
        assert!(pending(key), "the staging stays pending while its list is in flight");
        assert!(Arc::strong_count(&staging) > before, "the ready upload holds the staging");
        recycle(ready.into_iter().next().unwrap());
        assert!(!pending(key), "the staging is free after the list ran");
        assert_eq!(Arc::strong_count(&staging), before);
    }
}
