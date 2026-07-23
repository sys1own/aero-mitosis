//! Lock-free work-stealing thread pool built on `crossbeam-deque::Injector`.
//!
//! The pool distributes tasks across worker threads that steal from a single
//! central deque, keeping scheduling overhead low while saturating available
//! parallelism.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use crossbeam_deque::{Injector, Steal};

/// A task is any `Send` closure that can be executed once.
pub type Task = Box<dyn FnOnce() + Send + 'static>;

/// Lock-free work-stealing thread pool.
pub struct WorkStealingPool {
    injector: Arc<Injector<Task>>,
    running: Arc<AtomicBool>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl WorkStealingPool {
    /// Create a pool with `num_threads` workers.
    ///
    /// Thread counts are clamped to a sensible range; `0` becomes `1` and
    /// unreasonably large values are capped to avoid allocation failures.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::atomic::{AtomicUsize, Ordering};
    /// use std::sync::Arc;
    /// use autonomic_ci_core::scheduler::work_stealing::WorkStealingPool;
    ///
    /// let mut pool = WorkStealingPool::new(2);
    /// let counter = Arc::new(AtomicUsize::new(0));
    /// for _ in 0..4 {
    ///     let c = Arc::clone(&counter);
    ///     pool.submit(Box::new(move || { c.fetch_add(1, Ordering::Relaxed); }));
    /// }
    /// pool.shutdown();
    /// assert_eq!(counter.load(Ordering::Relaxed), 4);
    /// ```
    pub fn new(num_threads: usize) -> Self {
        let num_threads = num_threads.clamp(1, 256);
        let injector = Arc::new(Injector::new());
        let running = Arc::new(AtomicBool::new(true));
        let mut handles = Vec::with_capacity(num_threads);

        for _ in 0..num_threads {
            let injector = Arc::clone(&injector);
            let running = Arc::clone(&running);
            let handle = thread::spawn(move || worker_loop(injector, running));
            handles.push(handle);
        }

        Self {
            injector,
            running,
            handles,
        }
    }

    /// Create a pool sized to the machine's reported parallelism.
    pub fn default_threads() -> Self {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .max(1);
        Self::new(threads)
    }

    /// Submit a task to the pool.
    pub fn submit<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.injector.push(Box::new(f));
    }

    /// Number of worker threads in the pool.
    pub fn thread_count(&self) -> usize {
        self.handles.len()
    }

    /// Signal workers to finish after the queue is drained and wait for them.
    ///
    /// Tasks submitted after shutdown are dropped because the worker threads
    /// have already terminated.
    pub fn shutdown(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for WorkStealingPool {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

fn worker_loop(injector: Arc<Injector<Task>>, running: Arc<AtomicBool>) {
    loop {
        match injector.steal() {
            Steal::Success(task) => task(),
            Steal::Empty => {
                if !running.load(Ordering::Relaxed) {
                    break;
                }
                thread::yield_now();
            }
            Steal::Retry => thread::yield_now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn pool_runs_submitted_tasks() {
        let mut pool = WorkStealingPool::new(2);
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..10 {
            let c = Arc::clone(&counter);
            pool.submit(move || {
                c.fetch_add(1, Ordering::Relaxed);
            });
        }

        // Give workers a moment to drain the queue.
        while counter.load(Ordering::Relaxed) < 10 {
            thread::yield_now();
        }

        pool.shutdown();
        assert_eq!(counter.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn pool_survives_a_panicking_task_and_continues_work() {
        let mut pool = WorkStealingPool::new(4);
        let counter = Arc::new(AtomicUsize::new(0));

        pool.submit(|| panic!("intentional worker panic"));

        for _ in 0..10 {
            let c = Arc::clone(&counter);
            pool.submit(move || {
                c.fetch_add(1, Ordering::Relaxed);
            });
        }

        while counter.load(Ordering::Relaxed) < 10 {
            thread::yield_now();
        }

        pool.shutdown();
        assert_eq!(counter.load(Ordering::Relaxed), 10);
    }

    #[test]
    fn shutdown_drops_late_tasks_without_panic() {
        let mut pool = WorkStealingPool::new(2);
        let counter = Arc::new(AtomicUsize::new(0));

        pool.shutdown();

        let c = Arc::clone(&counter);
        pool.submit(move || {
            c.fetch_add(1, Ordering::Relaxed);
        });

        drop(pool);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn drop_drains_pending_tasks_gracefully() {
        let pool = WorkStealingPool::new(2);
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..100 {
            let c = Arc::clone(&counter);
            pool.submit(move || {
                c.fetch_add(1, Ordering::Relaxed);
            });
        }

        drop(pool);
        assert_eq!(counter.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn zero_threads_clamped_to_one() {
        let pool = WorkStealingPool::new(0);
        assert_eq!(pool.thread_count(), 1);
    }

    #[test]
    fn huge_thread_count_is_clamped_without_panic() {
        let pool = WorkStealingPool::new(usize::MAX);
        assert!(pool.thread_count() > 0);
        assert!(pool.thread_count() <= 256);
    }

    #[test]
    fn contention_with_many_tasks() {
        let mut pool = WorkStealingPool::new(4);
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..10_000 {
            let c = Arc::clone(&counter);
            pool.submit(move || {
                c.fetch_add(1, Ordering::Relaxed);
            });
        }

        while counter.load(Ordering::Relaxed) < 10_000 {
            thread::yield_now();
        }

        pool.shutdown();
        assert_eq!(counter.load(Ordering::Relaxed), 10_000);
    }
}
