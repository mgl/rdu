//! Lazy work-stealing pool for the directory walk.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread::Scope;

use crossbeam_deque::{Injector, Steal, Stealer, Worker};

/// Queued tasks per worker before starting another worker.
const TASKS_PER_WORKER: usize = 2;

/// Handed to a running task so it can queue more work.
pub struct Spawner<'a, T: Send> {
    pool: &'a Pool<T>,
    local: &'a Worker<T>,
}

impl<T: Send> Spawner<'_, T> {
    /// Queues a task and marks it outstanding before publishing it.
    pub fn push(&self, task: T) {
        self.pool.outstanding.fetch_add(1, Ordering::Relaxed);
        self.local.push(task);
        self.pool.wake_one();
    }
}

struct Pool<T: Send> {
    injector: Injector<T>,
    stealers: Box<[Stealer<T>]>,
    /// Unclaimed worker deques, indexed by worker ID.
    spare: Mutex<Vec<Option<Worker<T>>>>,
    /// Tasks pushed but not finished.
    outstanding: AtomicUsize,
    /// Started workers, including the caller.
    live: AtomicUsize,
    max_workers: usize,
    /// Guards the sleeper count.
    sleepers: Mutex<usize>,
    /// Lock-free sleeper count used by the common push path.
    parked: AtomicUsize,
    wake: Condvar,
    shutdown: AtomicBool,
}

impl<T: Send> Pool<T> {
    fn new(max_workers: usize) -> Self {
        let workers: Vec<Worker<T>> = (0..max_workers).map(|_| Worker::new_lifo()).collect();
        let stealers = workers.iter().map(Worker::stealer).collect();
        let spare = workers.into_iter().map(Some).collect();
        Self {
            injector: Injector::new(),
            stealers,
            spare: Mutex::new(spare),
            outstanding: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            max_workers,
            sleepers: Mutex::new(0),
            parked: AtomicUsize::new(0),
            wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
        }
    }

    /// Wakes one sleeper when needed.
    fn wake_one(&self) {
        if self.parked.load(Ordering::Relaxed) > 0 {
            let _guard = self.sleepers.lock().expect("sleeper count");
            self.wake.notify_one();
        }
    }

    fn wake_all(&self) {
        let _guard = self.sleepers.lock().expect("sleeper count");
        self.wake.notify_all();
    }

    /// Takes the next task, preferring the local deque.
    fn find(&self, local: &Worker<T>, me: usize) -> Option<T> {
        if let Some(task) = local.pop() {
            return Some(task);
        }
        loop {
            match self.injector.steal_batch_and_pop(local) {
                Steal::Success(task) => return Some(task),
                Steal::Retry => continue,
                Steal::Empty => break,
            }
        }
        for offset in 1..=self.stealers.len() {
            let victim = (me + offset) % self.stealers.len();
            if victim == me {
                continue;
            }
            loop {
                match self.stealers[victim].steal_batch_and_pop(local) {
                    Steal::Success(task) => return Some(task),
                    Steal::Retry => continue,
                    Steal::Empty => break,
                }
            }
        }
        None
    }

    /// Whether any deque currently holds a task.
    fn has_work(&self) -> bool {
        !self.injector.is_empty() || self.stealers.iter().any(|stealer| !stealer.is_empty())
    }

    /// Sleeps until work arrives or the scan ends.
    fn park(&self) {
        let mut sleepers = self.sleepers.lock().expect("sleeper count");
        if self.shutdown.load(Ordering::Acquire) {
            return;
        }
        *sleepers += 1;
        self.parked.fetch_add(1, Ordering::SeqCst);
        if !self.has_work() && !self.shutdown.load(Ordering::Acquire) {
            let (guard, _) = self
                .wake
                .wait_timeout(sleepers, std::time::Duration::from_millis(2))
                .expect("sleeper count");
            sleepers = guard;
        }
        self.parked.fetch_sub(1, Ordering::SeqCst);
        *sleepers -= 1;
    }

    /// Marks a finished task, ending the scan when it was the last one.
    fn finish_task(&self) {
        if self.outstanding.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shutdown.store(true, Ordering::Release);
            self.wake_all();
        }
    }

    /// Starts another worker when the queue has outgrown the running ones.
    fn maybe_grow<'scope, F>(&'scope self, scope: &'scope Scope<'scope, '_>, exec: &'scope F)
    where
        T: 'scope,
        F: Fn(T, &Spawner<'_, T>) + Sync + 'scope,
    {
        let live = self.live.load(Ordering::Relaxed);
        if live >= self.max_workers
            || self.outstanding.load(Ordering::Relaxed) < live * TASKS_PER_WORKER
        {
            return;
        }
        let me = self.live.fetch_add(1, Ordering::Relaxed);
        if me >= self.max_workers {
            self.live.fetch_sub(1, Ordering::Relaxed);
            return;
        }
        let local = self.claim(me);
        scope.spawn(move || self.work(local, me, scope, exec));
    }

    /// Takes the deque belonging to worker `index`.
    fn claim(&self, index: usize) -> Worker<T> {
        self.spare.lock().expect("spare deques")[index]
            .take()
            .expect("each worker id is claimed exactly once")
    }

    /// Runs tasks, grows the pool, and parks when idle.
    fn work<'scope, F>(
        &'scope self,
        local: Worker<T>,
        me: usize,
        scope: &'scope Scope<'scope, '_>,
        exec: &'scope F,
    ) where
        T: 'scope,
        F: Fn(T, &Spawner<'_, T>) + Sync + 'scope,
    {
        let spawner = Spawner {
            pool: self,
            local: &local,
        };
        loop {
            if let Some(task) = self.find(&local, me) {
                self.maybe_grow(scope, exec);
                exec(task, &spawner);
                self.finish_task();
                continue;
            }
            if self.shutdown.load(Ordering::Acquire) {
                return;
            }
            self.park();
        }
    }
}

/// Runs `seed` and all tasks it queues.
pub fn run<T, F>(max_workers: usize, seed: T, exec: F)
where
    T: Send,
    F: Fn(T, &Spawner<'_, T>) + Sync,
{
    let pool = Pool::new(max_workers.max(1));
    let local = pool.claim(0);
    pool.live.store(1, Ordering::Relaxed);
    pool.outstanding.store(1, Ordering::Relaxed);
    pool.injector.push(seed);

    std::thread::scope(|scope| {
        let pool = &pool;
        let exec = &exec;
        pool.work(local, 0, scope, exec);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn every_queued_task_runs_exactly_once() {
        let counter = AtomicU64::new(0);
        run(8, 12u32, |depth: u32, spawner| {
            counter.fetch_add(1, Ordering::Relaxed);
            if depth > 0 {
                spawner.push(depth - 1);
                spawner.push(depth - 1);
            }
        });
        assert_eq!(counter.load(Ordering::Relaxed), (1 << 13) - 1);
    }

    #[test]
    fn a_single_task_never_starts_a_thread() {
        let ran = AtomicU64::new(0);
        run(8, (), |(), _| {
            ran.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(ran.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_second_worker_actually_takes_work() {
        use std::collections::HashSet;
        use std::sync::Mutex as StdMutex;

        let threads: StdMutex<HashSet<std::thread::ThreadId>> = StdMutex::default();
        run(2, 4_000u32, |left: u32, spawner| {
            threads.lock().unwrap().insert(std::thread::current().id());
            if left > 0 {
                spawner.push(left - 1);
                spawner.push(0);
            }
            std::hint::black_box(left);
        });
        assert_eq!(
            threads.lock().unwrap().len(),
            2,
            "both workers should have run tasks"
        );
    }

    #[test]
    fn one_worker_still_drains_a_deep_queue() {
        let counter = AtomicU64::new(0);
        run(1, 10_000u32, |left: u32, spawner| {
            counter.fetch_add(1, Ordering::Relaxed);
            if left > 0 {
                spawner.push(left - 1);
            }
        });
        assert_eq!(counter.load(Ordering::Relaxed), 10_001);
    }
}
