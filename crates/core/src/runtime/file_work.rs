use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use rayon::{ThreadPool, ThreadPoolBuilder};

pub struct FileWorkPool {
    pool: OnceLock<Option<ThreadPool>>,
    extra_threads: usize,
    credits: Arc<FileWorkCredits>,
    poisoned: Arc<AtomicBool>,
    poison_warning_emitted: Arc<AtomicBool>,
}

impl FileWorkPool {
    pub fn new(parallelism: usize) -> Self {
        let extra_threads = parallelism.saturating_sub(1);
        let poisoned = Arc::new(AtomicBool::new(false));
        let poison_warning_emitted = Arc::new(AtomicBool::new(false));
        Self {
            pool: OnceLock::new(),
            extra_threads,
            credits: Arc::new(FileWorkCredits {
                capacity: extra_threads,
                available: AtomicUsize::new(extra_threads),
            }),
            poisoned,
            poison_warning_emitted,
        }
    }

    #[must_use]
    pub fn extra_capacity(&self) -> usize {
        self.credits.capacity
    }

    #[must_use]
    pub fn try_credit(&self) -> Option<FileWorkCredit> {
        if self.poisoned.load(Ordering::Acquire) {
            if !self.poison_warning_emitted.swap(true, Ordering::AcqRel) {
                tracing::warn!(target: "agentshim", event = "file_work_pool_poisoned", outcome = "inline_fallback");
            }
            return None;
        }
        let mut available = self.credits.available.load(Ordering::Acquire);
        loop {
            if available == 0 {
                return None;
            }
            match self.credits.available.compare_exchange_weak(
                available,
                available - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(FileWorkCredit {
                        credits: Arc::clone(&self.credits),
                    });
                }
                Err(observed) => available = observed,
            }
        }
    }

    #[must_use]
    pub fn try_credits(&self, maximum: usize) -> Vec<FileWorkCredit> {
        let mut credits = Vec::with_capacity(maximum.min(self.extra_capacity()));
        while credits.len() < maximum {
            let Some(credit) = self.try_credit() else {
                break;
            };
            credits.push(credit);
        }
        credits
    }

    pub fn spawn<J>(&self, credit: FileWorkCredit, job: J) -> Result<(), (FileWorkCredit, J)>
    where
        J: FnOnce(FileWorkCredit) + Send + 'static,
    {
        let Some(pool) = self.pool() else {
            return Err((credit, job));
        };
        if self.poisoned.load(Ordering::Acquire) {
            return Err((credit, job));
        }
        let poisoned = Arc::clone(&self.poisoned);
        pool.spawn_fifo(move || {
            if catch_unwind(AssertUnwindSafe(|| job(credit))).is_err() {
                poisoned.store(true, Ordering::Release);
            }
        });
        Ok(())
    }

    fn pool(&self) -> Option<&ThreadPool> {
        self.pool
            .get_or_init(|| {
                if self.extra_threads == 0 {
                    return None;
                }
                let panic_poisoned = Arc::clone(&self.poisoned);
                let built = ThreadPoolBuilder::new()
                    .num_threads(self.extra_threads)
                    .thread_name(|index| format!("agentshim-file-{index}"))
                    .panic_handler(move |_| {
                        panic_poisoned.store(true, Ordering::Release);
                    })
                    .build();
                if built.is_err() {
                    self.poisoned.store(true, Ordering::Release);
                }
                built.ok()
            })
            .as_ref()
    }

    #[cfg(test)]
    pub fn available_credits(&self) -> usize {
        self.credits.available.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub fn is_initialized(&self) -> bool {
        self.pool.get().is_some()
    }

    #[cfg(test)]
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }
}

impl fmt::Debug for FileWorkPool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileWorkPool")
            .field("extra_capacity", &self.extra_capacity())
            .field("poisoned", &self.poisoned.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

struct FileWorkCredits {
    capacity: usize,
    available: AtomicUsize,
}

pub struct FileWorkCredit {
    credits: Arc<FileWorkCredits>,
}

impl Drop for FileWorkCredit {
    fn drop(&mut self) {
        let previous = self.credits.available.fetch_add(1, Ordering::Release);
        debug_assert!(previous < self.credits.capacity);
    }
}
