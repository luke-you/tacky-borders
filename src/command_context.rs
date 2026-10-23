use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(5);
const PENDING: u8 = 0;
const COMMITTED: u8 = 1;
const CANCELLED: u8 = 2;

#[derive(Clone, Debug)]
pub struct CommandContext {
    deadline: Option<Instant>,
    stop: Arc<AtomicBool>,
    phase: Arc<AtomicU8>,
}

impl Default for CommandContext {
    fn default() -> Self {
        Self {
            deadline: None,
            stop: Arc::new(AtomicBool::new(false)),
            phase: Arc::new(AtomicU8::new(PENDING)),
        }
    }
}

impl CommandContext {
    pub fn new(deadline: Instant, stop: Arc<AtomicBool>) -> Self {
        Self {
            deadline: Some(deadline),
            stop,
            phase: Arc::new(AtomicU8::new(PENDING)),
        }
    }

    pub fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.phase.load(Ordering::Acquire) != CANCELLED,
            "ipc command cancelled before commit"
        );
        anyhow::ensure!(!self.stop.load(Ordering::Acquire), "ipc server is stopping");
        anyhow::ensure!(
            self.deadline
                .is_none_or(|deadline| Instant::now() < deadline),
            "ipc connection deadline exceeded during command execution"
        );
        Ok(())
    }

    pub fn is_committed(&self) -> bool {
        self.phase.load(Ordering::Acquire) == COMMITTED
    }

    pub fn begin_commit(&self) -> anyhow::Result<()> {
        self.check()?;
        self.phase
            .compare_exchange(PENDING, COMMITTED, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("ipc command cancelled before commit"))?;
        Ok(())
    }

    // The owner and waiter arbitrate cancellation against commit atomically.
    pub fn cancel_before_commit(&self) -> bool {
        self.phase
            .compare_exchange(PENDING, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn pause(&self) -> anyhow::Result<()> {
        self.check()?;
        let delay = self
            .deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
            .unwrap_or(POLL_INTERVAL)
            .min(POLL_INTERVAL);
        std::thread::park_timeout(delay);
        self.check()
    }

    pub fn wait(&self, duration: Duration) -> anyhow::Result<()> {
        let until = Instant::now()
            .checked_add(duration)
            .ok_or_else(|| anyhow::anyhow!("command wait overflow"))?;
        while Instant::now() < until {
            self.pause()?;
        }
        self.check()
    }

    fn acquire<T>(
        &self,
        mut attempt: impl FnMut() -> Result<T, TryLockError<T>>,
        name: &str,
    ) -> anyhow::Result<T> {
        loop {
            self.check()?;
            match attempt() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(_)) => anyhow::bail!("{name} poisoned"),
                Err(TryLockError::WouldBlock) => self.pause()?,
            }
        }
    }

    pub fn lock<'a, T>(&self, lock: &'a Mutex<T>, name: &str) -> anyhow::Result<MutexGuard<'a, T>> {
        self.acquire(|| lock.try_lock(), name)
    }

    pub fn read<'a, T>(
        &self,
        lock: &'a RwLock<T>,
        name: &str,
    ) -> anyhow::Result<RwLockReadGuard<'a, T>> {
        self.acquire(|| lock.try_read(), name)
    }

    pub fn write<'a, T>(
        &self,
        lock: &'a RwLock<T>,
        name: &str,
    ) -> anyhow::Result<RwLockWriteGuard<'a, T>> {
        self.acquire(|| lock.try_write(), name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short_context() -> CommandContext {
        CommandContext::new(
            Instant::now() + Duration::from_millis(40),
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[test]
    fn contended_mutex_and_rwlock_admission_expire() {
        let mutex = Mutex::new(0);
        let rwlock = RwLock::new(0);
        let _mutex_guard = mutex.lock().unwrap();
        let _write_guard = rwlock.write().unwrap();
        let started = Instant::now();
        assert!(
            short_context()
                .lock(&mutex, "test mutex")
                .unwrap_err()
                .to_string()
                .contains("deadline")
        );
        assert!(
            short_context()
                .read(&rwlock, "test read")
                .unwrap_err()
                .to_string()
                .contains("deadline")
        );
        assert!(
            short_context()
                .write(&rwlock, "test write")
                .unwrap_err()
                .to_string()
                .contains("deadline")
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn stop_releases_contended_admission() {
        let mutex = Mutex::new(0);
        let _guard = mutex.lock().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let context = CommandContext::new(Instant::now() + Duration::from_secs(30), stop.clone());
        std::thread::scope(|scope| {
            let waiter =
                scope.spawn(|| context.lock(&mutex, "test mutex").unwrap_err().to_string());
            stop.store(true, Ordering::Release);
            assert!(waiter.join().unwrap().contains("stopping"));
        });
    }

    #[test]
    fn retry_delay_cannot_extend_command_lifetime() {
        let started = Instant::now();
        assert!(
            short_context()
                .wait(Duration::from_secs(30))
                .unwrap_err()
                .to_string()
                .contains("deadline")
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn poisoned_locks_are_distinct_from_deadlines() {
        let mutex = Mutex::new(0);
        let rwlock = RwLock::new(0);
        std::thread::scope(|scope| {
            assert!(
                scope
                    .spawn(|| {
                        let _guard = mutex.lock().unwrap();
                        panic!("poison test mutex");
                    })
                    .join()
                    .is_err()
            );
            assert!(
                scope
                    .spawn(|| {
                        let _guard = rwlock.write().unwrap();
                        panic!("poison test rwlock");
                    })
                    .join()
                    .is_err()
            );
        });
        let context = CommandContext::default();
        assert_eq!(
            context.lock(&mutex, "test mutex").unwrap_err().to_string(),
            "test mutex poisoned"
        );
        assert_eq!(
            context.read(&rwlock, "test read").unwrap_err().to_string(),
            "test read poisoned"
        );
        assert_eq!(
            context
                .write(&rwlock, "test write")
                .unwrap_err()
                .to_string(),
            "test write poisoned"
        );
    }

    #[test]
    fn queued_cancellation_and_expiry_prevent_commit() {
        let context = CommandContext::default();
        let owner = context.clone();
        assert!(context.cancel_before_commit());
        assert!(owner.begin_commit().is_err());
        assert!(!owner.is_committed());
        let expired = CommandContext::new(Instant::now(), Arc::new(AtomicBool::new(false)));
        assert!(expired.begin_commit().is_err());
    }

    #[test]
    fn committed_work_cannot_be_cancelled_by_waiter() {
        let context = short_context();
        context.begin_commit().unwrap();
        assert!(!context.cancel_before_commit());
        context.stop.store(true, Ordering::Release);
        assert!(context.check().is_err());
        assert!(context.is_committed());
    }

    #[test]
    fn cancellation_and_commit_have_exactly_one_winner() {
        for _ in 0..32 {
            let context = CommandContext::default();
            let barrier = std::sync::Barrier::new(2);
            std::thread::scope(|scope| {
                let owner = scope.spawn(|| {
                    barrier.wait();
                    context.begin_commit().is_ok()
                });
                barrier.wait();
                let cancelled = context.cancel_before_commit();
                let committed = owner.join().unwrap();
                assert_ne!(cancelled, committed);
                assert_eq!(context.is_committed(), committed);
            });
        }
    }
}
