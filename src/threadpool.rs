use std::panic::{self, AssertUnwindSafe};
use std::sync::{
    Arc, Condvar, LazyLock, Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;

use crate::error::{PzstdError, Result};

pub(crate) type Task = Box<dyn FnOnce() + Send>;

struct WorkerSlot {
    task: Mutex<Option<Task>>,
    notify: Condvar,
}

struct ThreadPool {
    slots: Arc<[WorkerSlot]>,
    size: usize,
}

impl ThreadPool {
    fn new(size: usize) -> Self {
        let slots: Arc<[WorkerSlot]> = (0..size)
            .map(|_| WorkerSlot {
                task: Mutex::new(None),
                notify: Condvar::new(),
            })
            .collect();

        for id in 0..size {
            let slots = Arc::clone(&slots);
            thread::Builder::new()
                .name("pzstd-worker".into())
                .spawn(move || {
                    let slot = &slots[id];
                    loop {
                        let task = {
                            let mut guard = slot.task.lock().unwrap();
                            loop {
                                if let Some(task) = guard.take() {
                                    break task;
                                }
                                guard = slot.notify.wait(guard).unwrap();
                            }
                        };
                        // catch_unwind keeps the worker alive if a task panics.
                        let _ = panic::catch_unwind(AssertUnwindSafe(task));
                    }
                })
                .expect("failed to spawn pzstd worker thread");
        }

        Self { slots, size }
    }
}

static POOL: LazyLock<ThreadPool> = LazyLock::new(|| {
    let n = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    ThreadPool::new(n)
});

/// Run `f(frame_index, frame)` in parallel across the global thread pool.
/// Frames are partitioned into contiguous chunks, one per worker.
/// Blocks until all chunks complete. Returns the first error encountered.
pub(crate) fn parallel_for_each<T: Sync>(
    items: &[T],
    f: impl Fn(usize, &T) -> Result<()> + Sync,
) -> Result<()> {
    let n_threads = POOL.size.min(items.len());
    let chunk_size = items.len().div_ceil(n_threads);
    let error: OnceLock<PzstdError> = OnceLock::new();

    // Completion barrier: each task decrements; last one signals the caller.
    let remaining = AtomicUsize::new(0);
    let done_lock = Mutex::new(false);
    let done_cvar = Condvar::new();

    let chunks: Vec<_> = items.chunks(chunk_size).collect();
    remaining.store(chunks.len(), Ordering::Release);

    for (worker_id, chunk) in chunks.into_iter().enumerate() {
        let base_idx = worker_id * chunk_size;
        let error = &error;
        let f = &f;
        let remaining = &remaining;
        let done_lock = &done_lock;
        let done_cvar = &done_cvar;

        // SAFETY: We block below (done_cvar.wait) until every task has
        // completed, so all borrowed data outlives the closures. This is
        // the same lifetime-erasure technique used by std::thread::scope.
        let task: Task = unsafe {
            std::mem::transmute::<Box<dyn FnOnce() + Send + '_>, Task>(Box::new(move || {
                for (i, item) in chunk.iter().enumerate() {
                    if error.get().is_some() {
                        break;
                    }
                    let idx = base_idx + i;
                    if let Err(e) = f(idx, item) {
                        let _ = error.set(e);
                    }
                }
                // Signal completion — last task wakes the caller.
                if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                    *done_lock.lock().unwrap() = true;
                    done_cvar.notify_one();
                }
            }))
        };

        let slot = &POOL.slots[worker_id];
        *slot.task.lock().unwrap() = Some(task);
        slot.notify.notify_one();
    }

    // Block until all tasks finish — guarantees soundness of lifetime erasure.
    let mut done = done_lock.lock().unwrap();
    while !*done {
        done = done_cvar.wait(done).unwrap();
    }

    match error.into_inner() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
