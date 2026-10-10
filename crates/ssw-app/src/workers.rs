//! The worker pool (architecture "Threading").
//!
//! Jobs (IO, parsing, image decoding, rendering, indexing, search, export) run on pool threads
//! and never touch UI state. Every job belongs to a [`JobKey`] (the thing its result is about:
//! a document, a request) and carries a generation (the document version or a request counter).
//! Its result is delivered on the UI thread by [`Workers::poll`], after the [`Waker`] told the
//! message loop to call it. A result is dropped, and its callback never runs, when
//!
//! - a newer generation was submitted for the same key ([`Workers::supersede`] marks one without
//!   submitting a job, e.g. when the document changed again), or
//! - the window that owns the job was cancelled ([`Workers::cancel_window`], on window close).
//!
//! Running jobs see both through their [`CancelToken`] and can stop early. A panic inside a job
//! is caught and delivered as [`JobError::Panicked`]; it never reaches the UI thread.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// Wakes the UI thread's message loop after a worker result is ready; the app implements it
/// with `PostMessageW(hwnd, WM_APP + n, ..)`. Called from pool threads.
pub trait Waker: Send + Sync {
    /// Asks the UI thread to call [`Workers::poll`].
    fn wake(&self);
}

/// A [`Waker`] that exists before the window does: wakes that arrive before
/// [`DeferredWaker::connect`] are remembered and reported by `connect`.
#[derive(Default)]
pub struct DeferredWaker {
    target: Mutex<Option<Arc<dyn Waker>>>,
    missed: AtomicBool,
}

impl std::fmt::Debug for DeferredWaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeferredWaker")
            .field("missed", &self.missed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl DeferredWaker {
    /// A waker with no target yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sends every later wake to `target`. Returns whether wakes were missed before: the
    /// caller should then poll once.
    pub fn connect(&self, target: Arc<dyn Waker>) -> bool {
        *self
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(target);
        self.missed.swap(false, Ordering::AcqRel)
    }
}

impl Waker for DeferredWaker {
    fn wake(&self) {
        let target = self
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match target {
            Some(target) => target.wake(),
            None => self.missed.store(true, Ordering::Release),
        }
    }
}

/// What a job does, for logging and statistics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobKind {
    /// File reads and writes.
    Io,
    /// Parsing large documents.
    Parse,
    /// Image decoding.
    Decode,
    /// Math, Mermaid and page rendering.
    Render,
    /// Workspace indexing.
    Index,
    /// Search in files.
    Search,
    /// Export and import conversions.
    Export,
}

/// Identifies a window for [`Workers::cancel_window`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u64);

/// What a job's result is about; newer generations of the same key supersede older ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobKey(pub u64);

/// Identifies one submitted job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

/// Describes a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobSpec {
    /// What the job does.
    pub kind: JobKind,
    /// The window that owns the job; closing it cancels the job.
    pub window: Option<WindowId>,
    /// What the result is about.
    pub key: JobKey,
    /// Document version or request counter the job was started for.
    pub generation: u64,
}

/// Why a job produced no value.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum JobError {
    /// The job panicked; the message is the panic payload.
    #[error("job panicked: {0}")]
    Panicked(String),
    /// The job was cancelled before it started. Never handed to a callback: such a result is
    /// always stale.
    #[error("job was cancelled")]
    Cancelled,
}

/// Why a job was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SubmitError {
    /// The owning window was already cancelled.
    #[error("the window of the job was closed")]
    WindowClosed,
    /// The pool is shutting down.
    #[error("the worker pool is shutting down")]
    ShuttingDown,
}

/// Lets a running job notice that its result will be dropped.
#[derive(Clone, Debug)]
pub struct CancelToken {
    window: Option<Arc<AtomicBool>>,
    latest: Arc<AtomicU64>,
    generation: u64,
}

impl CancelToken {
    /// True when the window was closed or a newer generation of the key exists.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.window
            .as_ref()
            .is_some_and(|closed| closed.load(Ordering::Acquire))
            || self.latest.load(Ordering::Acquire) > self.generation
    }
}

/// Counters, for tests and diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkerStats {
    /// Callbacks that ran with a value or an error.
    pub delivered: u64,
    /// Results dropped because they were stale or their window was closed.
    pub dropped: u64,
    /// Delivered results that were panics.
    pub panics: u64,
}

type Outcome = Result<Box<dyn Any + Send>, JobError>;

struct Task {
    id: JobId,
    run: Box<dyn FnOnce() -> Outcome + Send>,
}

struct Queue {
    tasks: Mutex<(VecDeque<Task>, bool)>,
    ready: Condvar,
}

struct Pending {
    spec: JobSpec,
    deliver: Box<dyn FnOnce(Outcome)>,
}

/// The pool. Lives on the UI thread; every method takes `&self` so callbacks may submit more
/// jobs.
pub struct Workers {
    queue: Arc<Queue>,
    threads: Vec<JoinHandle<()>>,
    results: Receiver<(JobId, Outcome)>,
    pending: RefCell<HashMap<JobId, Pending>>,
    latest: RefCell<HashMap<JobKey, Arc<AtomicU64>>>,
    windows: RefCell<HashMap<WindowId, Arc<AtomicBool>>>,
    closed: RefCell<HashSet<WindowId>>,
    next_id: Cell<u64>,
    stats: Cell<WorkerStats>,
}

impl std::fmt::Debug for Workers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workers")
            .field("threads", &self.threads.len())
            .field("pending", &self.pending.borrow().len())
            .field("stats", &self.stats.get())
            .finish()
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "non-text panic payload".to_owned()
    }
}

fn worker_loop(queue: &Queue, results: &Sender<(JobId, Outcome)>, waker: &dyn Waker) {
    loop {
        let task = {
            let mut guard = queue
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                if let Some(task) = guard.0.pop_front() {
                    break Some(task);
                }
                if guard.1 {
                    break None;
                }
                guard = queue
                    .ready
                    .wait(guard)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        let Some(task) = task else { return };
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(task.run))
            .unwrap_or_else(|payload| Err(JobError::Panicked(panic_message(&*payload))));
        // The receiver is gone only while the pool shuts down.
        let _ = results.send((task.id, outcome));
        waker.wake();
    }
}

impl Workers {
    /// Starts `threads` pool threads (at least one).
    #[must_use]
    pub fn new(threads: usize, waker: Arc<dyn Waker>) -> Self {
        let queue = Arc::new(Queue {
            tasks: Mutex::new((VecDeque::new(), false)),
            ready: Condvar::new(),
        });
        let (sender, results) = channel();
        let threads = (0..threads.max(1))
            .map(|index| {
                let queue = Arc::clone(&queue);
                let sender = sender.clone();
                let waker = Arc::clone(&waker);
                std::thread::Builder::new()
                    .name(format!("ssw-worker-{index}"))
                    .spawn(move || worker_loop(&queue, &sender, waker.as_ref()))
            })
            .filter_map(Result::ok)
            .collect();
        Self {
            queue,
            threads,
            results,
            pending: RefCell::new(HashMap::new()),
            latest: RefCell::new(HashMap::new()),
            windows: RefCell::new(HashMap::new()),
            closed: RefCell::new(HashSet::new()),
            next_id: Cell::new(1),
            stats: Cell::new(WorkerStats::default()),
        }
    }

    /// A sensible pool size: the available parallelism minus the UI thread, between 2 and 8.
    #[must_use]
    pub fn default_thread_count() -> usize {
        std::thread::available_parallelism()
            .map_or(2, std::num::NonZeroUsize::get)
            .saturating_sub(1)
            .clamp(2, 8)
    }

    /// The number of pool threads that started.
    #[must_use]
    pub fn thread_count(&self) -> usize {
        self.threads.len()
    }

    /// The counters.
    #[must_use]
    pub fn stats(&self) -> WorkerStats {
        self.stats.get()
    }

    /// Jobs submitted whose result was not delivered or dropped yet.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.borrow().len()
    }

    fn bump(&self, change: impl FnOnce(&mut WorkerStats)) {
        let mut stats = self.stats.get();
        change(&mut stats);
        self.stats.set(stats);
    }

    fn latest_for(&self, key: JobKey) -> Arc<AtomicU64> {
        Arc::clone(
            self.latest
                .borrow_mut()
                .entry(key)
                .or_insert_with(|| Arc::new(AtomicU64::new(0))),
        )
    }

    /// Declares that results of `key` older than `generation` are obsolete (the document
    /// changed again): running jobs see [`CancelToken::is_cancelled`] and their results are
    /// dropped.
    pub fn supersede(&self, key: JobKey, generation: u64) {
        self.latest_for(key).fetch_max(generation, Ordering::AcqRel);
    }

    /// Runs `job` on a pool thread and calls `on_done` on the UI thread (from [`Workers::poll`])
    /// with its value, unless the result is stale or the window was cancelled.
    ///
    /// # Errors
    /// [`SubmitError::WindowClosed`] for a cancelled window, [`SubmitError::ShuttingDown`] when
    /// the pool is being dropped.
    pub fn submit<T, F, D>(&self, spec: JobSpec, job: F, on_done: D) -> Result<JobId, SubmitError>
    where
        T: Send + 'static,
        F: FnOnce(&CancelToken) -> T + Send + 'static,
        D: FnOnce(Result<T, JobError>) + 'static,
    {
        if spec
            .window
            .is_some_and(|window| self.closed.borrow().contains(&window))
        {
            return Err(SubmitError::WindowClosed);
        }
        let latest = self.latest_for(spec.key);
        latest.fetch_max(spec.generation, Ordering::AcqRel);
        let window_flag = spec.window.map(|window| {
            Arc::clone(
                self.windows
                    .borrow_mut()
                    .entry(window)
                    .or_insert_with(|| Arc::new(AtomicBool::new(false))),
            )
        });
        let token = CancelToken {
            window: window_flag,
            latest,
            generation: spec.generation,
        };
        let id = JobId(self.next_id.get());
        self.next_id.set(id.0 + 1);
        let task = Task {
            id,
            run: Box::new(move || {
                if token.is_cancelled() {
                    // Never started: the value would be dropped anyway.
                    return Err(JobError::Cancelled);
                }
                Ok(Box::new(job(&token)) as Box<dyn Any + Send>)
            }),
        };
        let deliver = Box::new(move |outcome: Outcome| match outcome {
            Ok(value) => match value.downcast::<T>() {
                Ok(value) => on_done(Ok(*value)),
                // The type is fixed by `submit`; a mismatch cannot happen.
                Err(_) => on_done(Err(JobError::Panicked("result type mismatch".to_owned()))),
            },
            Err(error) => on_done(Err(error)),
        });
        {
            let mut guard = self
                .queue
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.1 {
                return Err(SubmitError::ShuttingDown);
            }
            guard.0.push_back(task);
        }
        self.pending
            .borrow_mut()
            .insert(id, Pending { spec, deliver });
        self.queue.ready.notify_one();
        Ok(id)
    }

    /// Cancels every job of `window`: queued jobs are dropped when they start, running jobs see
    /// their token cancelled, and no callback of the window runs any more. Later submissions for
    /// the window are refused.
    pub fn cancel_window(&self, window: WindowId) {
        self.windows
            .borrow_mut()
            .entry(window)
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .store(true, Ordering::Release);
        self.closed.borrow_mut().insert(window);
        let doomed: Vec<JobId> = self
            .pending
            .borrow()
            .iter()
            .filter(|(_, pending)| pending.spec.window == Some(window))
            .map(|(id, _)| *id)
            .collect();
        for id in doomed {
            // The worker still reports its result; with no pending entry it is discarded.
            self.pending.borrow_mut().remove(&id);
            self.bump(|stats| stats.dropped += 1);
        }
    }

    /// Delivers every finished result on the calling (UI) thread. Returns how many callbacks
    /// ran.
    pub fn poll(&self) -> usize {
        let mut delivered = 0;
        while let Ok((id, outcome)) = self.results.try_recv() {
            let Some(pending) = self.pending.borrow_mut().remove(&id) else {
                // Cancelled with its window: already counted.
                continue;
            };
            let newest = self.latest_for(pending.spec.key).load(Ordering::Acquire);
            if newest > pending.spec.generation || matches!(outcome, Err(JobError::Cancelled)) {
                self.bump(|stats| stats.dropped += 1);
                continue;
            }
            if matches!(outcome, Err(JobError::Panicked(_))) {
                self.bump(|stats| stats.panics += 1);
            }
            self.bump(|stats| stats.delivered += 1);
            (pending.deliver)(outcome);
            delivered += 1;
        }
        delivered
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        {
            let mut guard = self
                .queue
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.0.clear();
            guard.1 = true;
        }
        self.queue.ready.notify_all();
        for thread in self.threads.drain(..) {
            // A pool thread only ends abnormally when `catch_unwind` itself failed.
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    use super::*;

    #[derive(Debug, Default)]
    struct CountingWaker(AtomicUsize);

    impl Waker for CountingWaker {
        fn wake(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn spec(key: u64, generation: u64) -> JobSpec {
        JobSpec {
            kind: JobKind::Io,
            window: None,
            key: JobKey(key),
            generation,
        }
    }

    /// Polls until `done` is true; fails after five seconds.
    fn pump(workers: &Workers, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "workers did not finish in time");
            workers.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn a_result_is_delivered_on_the_polling_thread_and_wakes_the_loop() {
        let waker = Arc::new(CountingWaker::default());
        let workers = Workers::new(2, waker.clone());
        assert_eq!(workers.thread_count(), 2);
        let caller = std::thread::current().id();
        let got = Rc::new(RefCell::new(None));
        let sink = Rc::clone(&got);
        workers
            .submit(
                spec(1, 1),
                |_| (std::thread::current().id(), 6 * 7),
                move |result| *sink.borrow_mut() = Some((result, std::thread::current().id())),
            )
            .unwrap();
        assert_eq!(workers.pending(), 1);
        pump(&workers, || got.borrow().is_some());
        let (result, delivered_on) = got.borrow_mut().take().unwrap();
        let (ran_on, value) = result.unwrap();
        assert_eq!(value, 42);
        assert_ne!(ran_on, caller, "the job must run on a pool thread");
        assert_eq!(
            delivered_on, caller,
            "the callback must run on the UI thread"
        );
        assert!(waker.0.load(Ordering::SeqCst) >= 1);
        assert_eq!(workers.pending(), 0);
        assert_eq!(
            workers.stats(),
            WorkerStats {
                delivered: 1,
                dropped: 0,
                panics: 0
            }
        );
    }

    #[test]
    fn a_newer_generation_drops_the_older_result() {
        let workers = Workers::new(1, Arc::new(CountingWaker::default()));
        let (release, gate) = channel::<()>();
        let (started, running) = channel::<()>();
        let delivered = Rc::new(RefCell::new(Vec::new()));
        let first = Rc::clone(&delivered);
        // Generation 1 is blocked until generation 2 has been submitted.
        workers
            .submit(
                spec(7, 1),
                move |token| {
                    started.send(()).unwrap();
                    gate.recv_timeout(Duration::from_secs(5)).unwrap();
                    (1, token.is_cancelled())
                },
                move |result| first.borrow_mut().push(result.unwrap()),
            )
            .unwrap();
        running.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = Rc::clone(&delivered);
        workers
            .submit(
                spec(7, 2),
                |token| (2, token.is_cancelled()),
                move |result| second.borrow_mut().push(result.unwrap()),
            )
            .unwrap();
        release.send(()).unwrap();
        pump(&workers, || workers.pending() == 0);
        // Only generation 2 arrived, and generation 1 saw the cancellation while it ran.
        assert_eq!(*delivered.borrow(), [(2, false)]);
        assert_eq!(workers.stats().dropped, 1);
        assert_eq!(workers.stats().delivered, 1);
    }

    #[test]
    fn supersede_marks_results_obsolete_without_a_new_job() {
        let workers = Workers::new(1, Arc::new(CountingWaker::default()));
        let ran = Rc::new(Cell::new(false));
        let flag = Rc::clone(&ran);
        workers
            .submit(spec(3, 5), |_| (), move |_| flag.set(true))
            .unwrap();
        workers.supersede(JobKey(3), 6);
        pump(&workers, || workers.pending() == 0);
        assert!(!ran.get());
        assert_eq!(workers.stats().dropped, 1);
        // An older supersede never lowers the marker.
        workers.supersede(JobKey(3), 2);
        let flag = Rc::clone(&ran);
        workers
            .submit(spec(3, 6), |_| (), move |_| flag.set(true))
            .unwrap();
        pump(&workers, || workers.pending() == 0);
        assert!(ran.get());
    }

    #[test]
    fn a_panic_is_isolated_to_its_job() {
        let workers = Workers::new(1, Arc::new(CountingWaker::default()));
        let outcomes = Rc::new(RefCell::new(Vec::new()));
        let literal = Rc::clone(&outcomes);
        workers
            .submit(
                spec(1, 1),
                |_| -> u32 { panic!("boom") },
                move |result| literal.borrow_mut().push(result),
            )
            .unwrap();
        let formatted = Rc::clone(&outcomes);
        workers
            .submit(
                spec(2, 1),
                |_| -> u32 { panic!("{}", String::from("formatted")) },
                move |result| formatted.borrow_mut().push(result),
            )
            .unwrap();
        let opaque = Rc::clone(&outcomes);
        workers
            .submit(
                spec(3, 1),
                |_| -> u32 { std::panic::panic_any(7_u8) },
                move |result| opaque.borrow_mut().push(result),
            )
            .unwrap();
        // The same single thread still serves the next job.
        let after = Rc::clone(&outcomes);
        workers
            .submit(
                spec(4, 1),
                |_| 9_u32,
                move |result| after.borrow_mut().push(result),
            )
            .unwrap();
        pump(&workers, || outcomes.borrow().len() == 4);
        assert_eq!(
            *outcomes.borrow(),
            [
                Err(JobError::Panicked("boom".to_owned())),
                Err(JobError::Panicked("formatted".to_owned())),
                Err(JobError::Panicked("non-text panic payload".to_owned())),
                Ok(9),
            ]
        );
        assert_eq!(workers.stats().panics, 3);
    }

    #[test]
    fn cancelling_a_window_drops_its_jobs_and_refuses_new_ones() {
        let workers = Workers::new(1, Arc::new(CountingWaker::default()));
        let window = WindowId(11);
        let other = WindowId(12);
        let (release, gate) = channel::<()>();
        let (started, running) = channel::<()>();
        let saw_cancel = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&saw_cancel);
        let ran = Rc::new(RefCell::new(Vec::new()));
        let mut owned = spec(1, 1);
        owned.window = Some(window);
        let sink = Rc::clone(&ran);
        workers
            .submit(
                owned,
                move |token| {
                    // Blocks until the window was cancelled, then reports what it saw.
                    started.send(()).unwrap();
                    let _ = gate.recv_timeout(Duration::from_secs(5));
                    observed.store(token.is_cancelled(), Ordering::SeqCst);
                },
                move |_| sink.borrow_mut().push("window job"),
            )
            .unwrap();
        running.recv_timeout(Duration::from_secs(5)).unwrap();
        // Queued behind the blocked job: cancelled before it can start.
        let mut queued = spec(2, 1);
        queued.window = Some(window);
        let sink = Rc::clone(&ran);
        workers
            .submit(
                queued,
                |_| (),
                move |_| sink.borrow_mut().push("queued job"),
            )
            .unwrap();
        let mut unrelated = spec(3, 1);
        unrelated.window = Some(other);
        let sink = Rc::clone(&ran);
        workers
            .submit(
                unrelated,
                |_| (),
                move |_| sink.borrow_mut().push("other window"),
            )
            .unwrap();

        workers.cancel_window(window);
        assert_eq!(workers.pending(), 1);
        assert_eq!(
            workers.submit(owned, |_| (), |_| ()),
            Err(SubmitError::WindowClosed)
        );
        release.send(()).unwrap();
        pump(&workers, || workers.pending() == 0);
        assert_eq!(*ran.borrow(), ["other window"]);
        assert!(saw_cancel.load(Ordering::SeqCst));
        assert_eq!(workers.stats().dropped, 2);
    }

    #[test]
    fn callbacks_may_submit_more_jobs() {
        let workers = Rc::new(Workers::new(1, Arc::new(CountingWaker::default())));
        let total = Rc::new(Cell::new(0));
        let inner_workers = Rc::clone(&workers);
        let inner_total = Rc::clone(&total);
        workers
            .submit(
                spec(1, 1),
                |_| 1,
                move |first| {
                    let value = first.unwrap();
                    let total = Rc::clone(&inner_total);
                    inner_workers
                        .submit(
                            spec(1, 2),
                            move |_| value + 10,
                            move |second| {
                                total.set(second.unwrap());
                            },
                        )
                        .unwrap();
                },
            )
            .unwrap();
        pump(&workers, || total.get() == 11);
    }

    #[test]
    fn a_deferred_waker_reports_wakes_missed_before_it_was_connected() {
        let deferred = Arc::new(DeferredWaker::new());
        assert!(format!("{deferred:?}").contains("DeferredWaker"));
        // Workers wake the deferred waker before any window exists.
        let workers = Workers::new(1, deferred.clone());
        workers.submit(spec(1, 1), |_| (), |_| ()).unwrap();
        // The worker wakes after it posted the result: wait for the wake itself.
        pump(&workers, || {
            workers.pending() == 0 && deferred.missed.load(Ordering::SeqCst)
        });
        let target = Arc::new(CountingWaker::default());
        assert!(
            deferred.connect(target.clone()),
            "the early wake was missed"
        );
        assert!(!deferred.connect(target.clone()), "reported only once");
        workers.submit(spec(2, 1), |_| (), |_| ()).unwrap();
        pump(&workers, || {
            workers.pending() == 0 && target.0.load(Ordering::SeqCst) >= 1
        });
    }

    #[test]
    fn the_default_pool_size_is_bounded() {
        let count = Workers::default_thread_count();
        assert!((2..=8).contains(&count));
    }

    #[test]
    fn dropping_the_pool_joins_its_threads_and_discards_queued_jobs() {
        let ran = Arc::new(AtomicUsize::new(0));
        let (release, gate) = channel::<()>();
        let (started, running) = channel::<()>();
        let workers = Workers::new(1, Arc::new(CountingWaker::default()));
        let counter = Arc::clone(&ran);
        workers
            .submit(
                spec(1, 1),
                move |_| {
                    started.send(()).unwrap();
                    let _ = gate.recv_timeout(Duration::from_secs(5));
                    counter.fetch_add(1, Ordering::SeqCst);
                },
                |_| (),
            )
            .unwrap();
        let counter = Arc::clone(&ran);
        workers
            .submit(
                spec(2, 1),
                move |_| {
                    counter.fetch_add(100, Ordering::SeqCst);
                },
                |_| (),
            )
            .unwrap();
        // The first job is running and the second queued behind it. A helper releases the first
        // job a moment after the pool starts dropping, so the second job is already discarded.
        running.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(format!("{workers:?}").contains("Workers"));
        let helper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let _ = release.send(());
        });
        drop(workers);
        helper.join().unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }
}
