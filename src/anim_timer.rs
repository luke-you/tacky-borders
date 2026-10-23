use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};

use crate::border_registry::discard_panic;
use crate::post_message_w;
use crate::utils::WM_APP_ANIMATE;

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnimationTimerExit {
    Stopped,
    InvalidInterval,
    InvalidFrameRate(i32),
    SpawnFailed(String),
    SchedulerStopped(String),
    PostFailed(String),
    Panicked,
}

impl std::fmt::Display for AnimationTimerExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for AnimationTimerExit {}

type TimerStatus = Arc<Mutex<Option<AnimationTimerExit>>>;
type SchedulerRegistrations = Arc<Mutex<HashMap<u64, TimerStatus>>>;

enum SchedulerCommand {
    Register {
        id: u64,
        hwnd: isize,
        interval: Duration,
        status: TimerStatus,
    },
    Unregister {
        id: u64,
        acknowledged: Sender<()>,
    },
}

struct SchedulerClient {
    commands: Sender<SchedulerCommand>,
    alive: Arc<AtomicBool>,
    registrations: SchedulerRegistrations,
}

struct SharedScheduler {
    client: SchedulerClient,
    _worker: JoinHandle<()>,
}

static SHARED_SCHEDULER: OnceLock<Mutex<Option<SharedScheduler>>> = OnceLock::new();
static NEXT_REGISTRATION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
enum TimerBackend {
    #[allow(dead_code)]
    Thread {
        stop: Option<Sender<()>>,
        worker: Option<JoinHandle<AnimationTimerExit>>,
    },
    Shared {
        id: u64,
        commands: Sender<SchedulerCommand>,
        status: TimerStatus,
    },
}

#[derive(Debug)]
pub struct AnimationTimer {
    backend: TimerBackend,
    outcome: Option<AnimationTimerExit>,
}

impl AnimationTimer {
    #[cfg(test)]
    pub(crate) fn fail_next_spawn_for_test() {
        FAIL_NEXT_SPAWN.set(true);
    }

    pub fn new(hwnd: HWND, interval: Duration) -> Result<Self, AnimationTimerExit> {
        if interval.is_zero() || Instant::now().checked_add(interval).is_none() {
            return Err(AnimationTimerExit::InvalidInterval);
        }
        #[cfg(test)]
        if FAIL_NEXT_SPAWN.replace(false) {
            return Err(AnimationTimerExit::SpawnFailed(
                "injected timer spawn failure".into(),
            ));
        }

        let client = shared_scheduler_client()?;
        if !client.alive.load(Ordering::Acquire) {
            return Err(AnimationTimerExit::SchedulerStopped(
                "shared animation scheduler is no longer running".into(),
            ));
        }
        let id = NEXT_REGISTRATION_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| AnimationTimerExit::SpawnFailed("registration id exhausted".into()))?;
        let status = Arc::new(Mutex::new(None));
        client
            .registrations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id, status.clone());
        let result = client.commands.send(SchedulerCommand::Register {
            id,
            hwnd: hwnd.0 as isize,
            interval,
            status: status.clone(),
        });
        if let Err(error) = result {
            client
                .registrations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            set_timer_status(
                &status,
                AnimationTimerExit::SchedulerStopped(error.to_string()),
            );
            return Err(AnimationTimerExit::SchedulerStopped(error.to_string()));
        }

        Ok(Self {
            backend: TimerBackend::Shared {
                id,
                commands: client.commands,
                status,
            },
            outcome: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn spawn(
        interval: Duration,
        post: impl FnMut() -> Result<(), String> + Send + 'static,
    ) -> Result<Self, AnimationTimerExit> {
        if interval.is_zero() {
            return Err(AnimationTimerExit::InvalidInterval);
        }
        if FAIL_NEXT_SPAWN.replace(false) {
            return Err(AnimationTimerExit::SpawnFailed(
                "injected timer spawn failure".into(),
            ));
        }
        let (stop, receiver) = mpsc::channel();
        match thread::Builder::new()
            .name("border-animation".into())
            .spawn(move || Self::run(receiver, interval, post))
        {
            Ok(worker) => Ok(Self {
                backend: TimerBackend::Thread {
                    stop: Some(stop),
                    worker: Some(worker),
                },
                outcome: None,
            }),
            Err(error) => Err(AnimationTimerExit::SpawnFailed(error.to_string())),
        }
    }

    #[cfg(test)]
    fn run(
        stop: Receiver<()>,
        interval: Duration,
        mut post: impl FnMut() -> Result<(), String>,
    ) -> AnimationTimerExit {
        loop {
            match stop.recv_timeout(interval) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return AnimationTimerExit::Stopped,
                Err(RecvTimeoutError::Timeout) => {
                    if !matches!(stop.try_recv(), Err(mpsc::TryRecvError::Empty)) {
                        return AnimationTimerExit::Stopped;
                    }
                    if let Err(error) = post() {
                        return AnimationTimerExit::PostFailed(error);
                    }
                }
            }
        }
    }

    pub fn is_finished(&self) -> bool {
        if self.outcome.is_some() {
            return true;
        }
        match &self.backend {
            TimerBackend::Thread { worker, .. } => {
                worker.as_ref().is_some_and(JoinHandle::is_finished)
            }
            TimerBackend::Shared { status, .. } => status
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_some(),
        }
    }

    pub fn poll_completion(&mut self) -> Option<&AnimationTimerExit> {
        match &self.backend {
            TimerBackend::Thread { worker, .. } => {
                if worker.as_ref().is_some_and(JoinHandle::is_finished) {
                    self.finish_thread();
                }
            }
            TimerBackend::Shared { status, .. } => {
                if self.outcome.is_none() {
                    self.outcome = status
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone();
                }
            }
        }
        self.outcome.as_ref()
    }

    pub fn stop_and_join(&mut self) -> &AnimationTimerExit {
        if self.outcome.is_none() {
            match &mut self.backend {
                TimerBackend::Thread { stop, .. } => {
                    if let Some(stop) = stop.take() {
                        let _ = stop.send(());
                    }
                    self.finish_thread();
                }
                TimerBackend::Shared {
                    id,
                    commands,
                    status,
                } => {
                    let (acknowledge, acknowledged) = mpsc::channel();
                    let unregister = commands.send(SchedulerCommand::Unregister {
                        id: *id,
                        acknowledged: acknowledge,
                    });
                    if unregister.is_ok() && acknowledged.recv().is_ok() {
                        set_timer_status(status, AnimationTimerExit::Stopped);
                    }
                    self.outcome = status
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone()
                        .or(Some(AnimationTimerExit::Panicked));
                }
            }
        } else if let TimerBackend::Shared {
            id,
            commands,
            status,
        } = &mut self.backend
        {
            let (acknowledge, acknowledged) = mpsc::channel();
            if commands
                .send(SchedulerCommand::Unregister {
                    id: *id,
                    acknowledged: acknowledge,
                })
                .is_ok()
                && acknowledged.recv().is_ok()
            {
                set_timer_status(status, AnimationTimerExit::Stopped);
            }
        }
        self.outcome.get_or_insert(AnimationTimerExit::Stopped)
    }

    fn finish_thread(&mut self) {
        let TimerBackend::Thread { worker, .. } = &mut self.backend else {
            return;
        };
        if let Some(worker) = worker.take() {
            self.outcome = Some(match worker.join() {
                Ok(outcome) => outcome,
                Err(payload) => {
                    discard_panic(payload);
                    AnimationTimerExit::Panicked
                }
            });
        }
    }
}

impl Drop for AnimationTimer {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn shared_scheduler_client() -> Result<SchedulerClient, AnimationTimerExit> {
    let scheduler = SHARED_SCHEDULER.get_or_init(|| Mutex::new(None));
    let mut scheduler = scheduler
        .lock()
        .map_err(|error| AnimationTimerExit::SchedulerStopped(error.to_string()))?;
    if let Some(shared) = scheduler.as_ref()
        && shared.client.alive.load(Ordering::Acquire)
    {
        return Ok(SchedulerClient {
            commands: shared.client.commands.clone(),
            alive: shared.client.alive.clone(),
            registrations: shared.client.registrations.clone(),
        });
    }
    if let Some(shared) = scheduler.take()
        && let Err(payload) = shared._worker.join()
    {
        discard_panic(payload);
    }

    let (commands, receiver) = mpsc::channel();
    let alive = Arc::new(AtomicBool::new(true));
    let registrations = Arc::new(Mutex::new(HashMap::new()));
    let worker_alive = alive.clone();
    let worker_registrations = registrations.clone();
    let worker = thread::Builder::new()
        .name("border-animation-scheduler".into())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                run_scheduler(receiver, worker_registrations.clone())
            }));
            worker_alive.store(false, Ordering::Release);
            let failure = if result.is_err() {
                AnimationTimerExit::Panicked
            } else {
                AnimationTimerExit::SchedulerStopped("scheduler channel closed".into())
            };
            let registrations = worker_registrations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            for status in registrations.values() {
                set_timer_status(status, failure.clone());
            }
        })
        .map_err(|error| AnimationTimerExit::SpawnFailed(error.to_string()))?;
    let client = SchedulerClient {
        commands,
        alive,
        registrations,
    };
    *scheduler = Some(SharedScheduler {
        client: SchedulerClient {
            commands: client.commands.clone(),
            alive: client.alive.clone(),
            registrations: client.registrations.clone(),
        },
        _worker: worker,
    });
    Ok(client)
}

struct ScheduledTimer {
    hwnd: isize,
    interval: Duration,
    next_due: Instant,
    status: TimerStatus,
}

fn run_scheduler(receiver: Receiver<SchedulerCommand>, statuses: SchedulerRegistrations) {
    let mut timers = HashMap::<u64, ScheduledTimer>::new();
    let mut deadlines = BinaryHeap::<Reverse<(Instant, u64)>>::new();

    loop {
        if let Ok(command) = receiver.try_recv() {
            process_scheduler_command(command, &mut timers, &mut deadlines, &statuses);
            continue;
        }

        while let Some(Reverse((deadline, id))) = deadlines.peek().copied() {
            if timers
                .get(&id)
                .is_some_and(|timer| timer.next_due == deadline)
            {
                break;
            }
            deadlines.pop();
        }

        let command = match deadlines.peek().copied() {
            Some(Reverse((deadline, _))) => {
                receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            }
            None => match receiver.recv() {
                Ok(command) => {
                    process_scheduler_command(command, &mut timers, &mut deadlines, &statuses);
                    continue;
                }
                Err(_) => return,
            },
        };
        match command {
            Ok(command) => {
                process_scheduler_command(command, &mut timers, &mut deadlines, &statuses)
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {
                let Some(Reverse((deadline, id))) = deadlines.pop() else {
                    continue;
                };
                let Some(timer) = timers.get(&id) else {
                    continue;
                };
                if timer.next_due != deadline {
                    continue;
                }

                let hwnd = timer.hwnd;
                let interval = timer.interval;
                let status = timer.status.clone();
                if let Err(error) =
                    post_message_w(Some(HWND(hwnd as _)), WM_APP_ANIMATE, WPARAM(0), LPARAM(0))
                {
                    set_timer_status(
                        &status,
                        AnimationTimerExit::PostFailed(format!("{error:#}")),
                    );
                    timers.remove(&id);
                    statuses
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&id);
                    continue;
                }

                let Some(next_due) = Instant::now().checked_add(interval) else {
                    set_timer_status(&status, AnimationTimerExit::InvalidInterval);
                    timers.remove(&id);
                    statuses
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&id);
                    continue;
                };
                if let Some(timer) = timers.get_mut(&id) {
                    timer.next_due = next_due;
                    deadlines.push(Reverse((next_due, id)));
                }
            }
        }
    }
}

fn process_scheduler_command(
    command: SchedulerCommand,
    timers: &mut HashMap<u64, ScheduledTimer>,
    deadlines: &mut BinaryHeap<Reverse<(Instant, u64)>>,
    statuses: &SchedulerRegistrations,
) {
    match command {
        SchedulerCommand::Register {
            id,
            hwnd,
            interval,
            status,
        } => {
            let Some(next_due) = Instant::now().checked_add(interval) else {
                set_timer_status(&status, AnimationTimerExit::InvalidInterval);
                statuses
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                return;
            };
            timers.insert(
                id,
                ScheduledTimer {
                    hwnd,
                    interval,
                    next_due,
                    status,
                },
            );
            deadlines.push(Reverse((next_due, id)));
        }
        SchedulerCommand::Unregister { id, acknowledged } => {
            timers.remove(&id);
            if let Some(status) = statuses
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id)
            {
                set_timer_status(&status, AnimationTimerExit::Stopped);
            }
            let _ = acknowledged.send(());
        }
    }
}

fn set_timer_status(status: &TimerStatus, outcome: AnimationTimerExit) {
    let mut status = status.lock().unwrap_or_else(|error| error.into_inner());
    if status.is_none() {
        *status = Some(outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn await_completion(timer: &AnimationTimer) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !timer.is_finished() {
            assert!(Instant::now() < deadline, "timer did not complete");
            thread::yield_now();
        }
    }

    #[test]
    fn long_interval_is_interruptible_and_join_is_idempotent() {
        let mut timer =
            AnimationTimer::spawn(Duration::MAX, || panic!("unexpected frame")).unwrap();
        assert_eq!(timer.stop_and_join(), &AnimationTimerExit::Stopped);
        assert!(matches!(
            timer.backend,
            TimerBackend::Thread { worker: None, .. }
        ));
        assert_eq!(timer.stop_and_join(), &AnimationTimerExit::Stopped);
    }

    #[test]
    fn zero_interval_is_rejected_without_spawning() {
        assert!(matches!(
            AnimationTimer::spawn(Duration::ZERO, || panic!("unexpected frame")),
            Err(AnimationTimerExit::InvalidInterval)
        ));
    }

    #[test]
    fn shared_timer_rejects_zero_interval_without_starting_scheduler() {
        assert!(matches!(
            AnimationTimer::new(HWND::default(), Duration::ZERO),
            Err(AnimationTimerExit::InvalidInterval)
        ));
    }

    fn create_native_window() -> HWND {
        use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, WINDOW_EX_STYLE, WS_POPUP};
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                windows::core::w!("STATIC"),
                windows::core::w!("animation timer test"),
                WS_POPUP,
                0,
                0,
                10,
                10,
                None,
                None,
                None,
                None,
            )
            .unwrap()
        }
    }

    fn take_animation_message(hwnd: HWND) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{MSG, PM_REMOVE, PeekMessageW};
        let mut message = MSG::default();
        unsafe {
            PeekMessageW(
                &mut message,
                Some(hwnd),
                WM_APP_ANIMATE,
                WM_APP_ANIMATE,
                PM_REMOVE,
            )
        }
        .as_bool()
    }

    #[test]
    fn post_failure_is_observed_and_joined() {
        let mut timer = AnimationTimer::spawn(Duration::from_millis(1), || {
            Err("injected post failure".into())
        })
        .unwrap();
        await_completion(&timer);
        assert_eq!(
            timer.poll_completion(),
            Some(&AnimationTimerExit::PostFailed(
                "injected post failure".into()
            ))
        );
        assert!(matches!(
            timer.backend,
            TimerBackend::Thread { worker: None, .. }
        ));
    }

    #[test]
    fn shared_timer_posts_and_unregisters_cleanly() {
        use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;

        let hwnd = create_native_window();
        let mut timer = AnimationTimer::new(hwnd, Duration::from_millis(10)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !take_animation_message(hwnd) {
            assert!(
                Instant::now() < deadline,
                "shared timer did not post a frame"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(timer.stop_and_join(), &AnimationTimerExit::Stopped);
        while take_animation_message(hwnd) {}
        thread::sleep(Duration::from_millis(50));
        assert!(
            !take_animation_message(hwnd),
            "frame posted after unregister"
        );
        unsafe { DestroyWindow(hwnd).unwrap() };
    }

    #[test]
    fn shared_post_failure_is_observed_and_unregistered() {
        use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;

        let hwnd = create_native_window();
        let mut timer = AnimationTimer::new(hwnd, Duration::from_millis(100)).unwrap();
        unsafe { DestroyWindow(hwnd).unwrap() };
        await_completion(&timer);
        assert!(matches!(
            timer.poll_completion(),
            Some(AnimationTimerExit::PostFailed(_))
        ));
        assert!(matches!(
            timer.stop_and_join(),
            AnimationTimerExit::PostFailed(_)
        ));
    }

    #[test]
    fn panic_is_contained_in_join_outcome() {
        let mut timer =
            AnimationTimer::spawn(Duration::from_millis(1), || panic!("injected timer panic"))
                .unwrap();
        await_completion(&timer);
        assert_eq!(timer.poll_completion(), Some(&AnimationTimerExit::Panicked));
    }

    #[test]
    fn no_post_after_join_and_replacement_retires_previous_timer() {
        let posts = Arc::new(AtomicUsize::new(0));
        let worker_posts = posts.clone();
        let (posted, observed) = mpsc::channel();
        let mut timer = AnimationTimer::spawn(Duration::from_millis(1), move || {
            worker_posts.fetch_add(1, Ordering::SeqCst);
            let _ = posted.send(());
            Ok(())
        })
        .unwrap();
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(timer.stop_and_join(), &AnimationTimerExit::Stopped);
        let count = posts.load(Ordering::SeqCst);
        timer = AnimationTimer::spawn(Duration::MAX, || panic!("unexpected replacement frame"))
            .unwrap();
        assert_eq!(timer.stop_and_join(), &AnimationTimerExit::Stopped);
        assert_eq!(posts.load(Ordering::SeqCst), count);
    }
}
