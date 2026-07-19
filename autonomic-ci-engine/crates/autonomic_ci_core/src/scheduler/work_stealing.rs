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
    pub fn new(num_threads: usize) -> Self {
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
    pub fn shutdown(mut self) {
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
        let pool = WorkStealingPool::new(2);
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
}
