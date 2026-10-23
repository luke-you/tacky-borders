use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::border_registry::{
    BorderInstanceId, BorderLaunchSlot, BorderOperationId, BorderPhase, BorderRegistry,
    BorderStopReason, BorderWorkerExit, JoinedBorderWorker, JoiningBorderWorker, RegistryError,
    UnattachedWorker, discard_panic,
};
use crate::border_runtime::{BorderWorkerContext, run_border_worker};
use crate::border_updates::{BorderUpdateMailbox, MailboxClosure};

pub const MAX_COORDINATOR_BATCH: usize = 64;
const MAX_TERMINAL_OUTCOMES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinatorError {
    Deferred,
    Registry(RegistryError),
    Capacity,
}

impl std::fmt::Display for CoordinatorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for CoordinatorError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Reload,
    Stop,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationOutcome {
    Reloaded {
        operation: BorderOperationId,
        old_generation_retired: bool,
        admitted_instances: Vec<BorderInstanceId>,
        skipped_invalid: usize,
        skipped_ineligible: usize,
        readiness: BorderReadiness,
    },
    Stopped {
        operation: BorderOperationId,
    },
    InterruptedByShutdown {
        operation: BorderOperationId,
    },
    Failed {
        operation: BorderOperationId,
        reason: &'static str,
        unresolved: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderReadiness {
    NotAwaited,
}

impl OperationOutcome {
    pub fn operation(&self) -> BorderOperationId {
        match self {
            Self::Reloaded { operation, .. }
            | Self::Stopped { operation }
            | Self::InterruptedByShutdown { operation }
            | Self::Failed { operation, .. } => *operation,
        }
    }
}

struct ActiveOperation {
    id: BorderOperationId,
    kind: OperationKind,
    deadline: Instant,
    signal_failures: u64,
    awaiting_baseline: bool,
    failure: Option<(&'static str, usize)>,
}

struct LaunchRecord {
    slot: BorderLaunchSlot,
    updates: Arc<BorderUpdateMailbox>,
    no_worker: Option<bool>,
    spawn_error: Option<String>,
}

struct JoinTicket {
    worker: Option<JoiningBorderWorker>,
    joined: Option<JoinedBorderWorker>,
}

pub struct BorderCoordinator {
    pub registry: Arc<Mutex<BorderRegistry>>,
    launches: HashMap<BorderInstanceId, LaunchRecord>,
    tickets: Vec<JoinTicket>,
    orphans: Vec<UnattachedWorker>,
    outcomes: VecDeque<(BorderInstanceId, BorderWorkerExit)>,
    pub dropped_outcomes: u64,
    pub signal_failures: u64,
    recovery_pending: bool,
    closure_cursor: usize,
    ticket_cursor: usize,
    recovery_cursor: usize,
    operation: Option<ActiveOperation>,
    operation_results: VecDeque<OperationOutcome>,
    shutdown_submission: Option<Arc<AtomicBool>>,
    #[cfg(test)]
    panic_at: Option<CoordinatorStage>,
    #[cfg(test)]
    pub(crate) worker_probe: Option<Arc<dyn Fn(crate::border_runtime::WorkerStage) + Send + Sync>>,
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum CoordinatorStage {
    BeforeAttachment,
    AfterCollection,
    AfterJoin,
}

pub(crate) fn try_registry(
    registry: &Mutex<BorderRegistry>,
) -> Option<MutexGuard<'_, BorderRegistry>> {
    match registry.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::WouldBlock) => None,
        Err(TryLockError::Poisoned(poison)) => {
            let mut guard = poison.into_inner();
            guard.quarantine();
            Some(guard)
        }
    }
}

impl BorderCoordinator {
    pub fn new(registry: Arc<Mutex<BorderRegistry>>) -> Self {
        Self {
            registry,
            launches: HashMap::new(),
            tickets: Vec::new(),
            orphans: Vec::new(),
            outcomes: VecDeque::with_capacity(MAX_TERMINAL_OUTCOMES),
            dropped_outcomes: 0,
            signal_failures: 0,
            recovery_pending: false,
            closure_cursor: 0,
            ticket_cursor: 0,
            recovery_cursor: 0,
            operation: None,
            operation_results: VecDeque::with_capacity(MAX_COORDINATOR_BATCH),
            shutdown_submission: None,
            #[cfg(test)]
            panic_at: None,
            #[cfg(test)]
            worker_probe: None,
        }
    }

    pub fn service_turn(&mut self) {
        if let Err(payload) = catch_unwind(AssertUnwindSafe(|| self.service_unchecked())) {
            self.recovery_pending = true;
            for launch in self.launches.values() {
                launch.slot.control.record_stop();
            }
            discard_panic(payload);
        }
        self.poll_operation(Instant::now());
    }

    pub fn begin_operation(
        &mut self,
        kind: OperationKind,
        deadline: Instant,
    ) -> Result<BorderOperationId, CoordinatorError> {
        let mut registry = try_registry(&self.registry).ok_or(CoordinatorError::Deferred)?;
        if kind == OperationKind::Shutdown {
            registry.shutdown_requested = true;
            registry.admission = crate::border_registry::BorderAdmission::ShuttingDown;
        }
        if self.operation.is_some() || self.operation_results.len() == MAX_COORDINATOR_BATCH {
            if kind == OperationKind::Shutdown {
                for entry in registry.entries.values() {
                    entry.control.record_stop();
                }
                for launch in self.launches.values() {
                    launch.slot.control.record_stop();
                }
            }
            return Err(CoordinatorError::Registry(RegistryError::Busy));
        }
        let id = registry
            .begin_operation(kind == OperationKind::Shutdown)
            .map_err(CoordinatorError::Registry)?;
        self.operation = Some(ActiveOperation {
            id,
            kind,
            deadline,
            signal_failures: self.signal_failures,
            awaiting_baseline: false,
            failure: None,
        });
        let reason = match kind {
            OperationKind::Reload => BorderStopReason::Reload,
            OperationKind::Stop => BorderStopReason::Explicit,
            OperationKind::Shutdown => BorderStopReason::Shutdown,
        };
        for entry in registry.entries.values_mut() {
            entry.control.record_stop();
            entry
                .stop
                .get_or_insert(crate::border_registry::BorderStopRequest {
                    reason,
                    requested_at: Instant::now(),
                });
            if entry.phase != BorderPhase::Joining {
                entry.phase = BorderPhase::StopRequested;
            }
        }
        for launch in self.launches.values() {
            launch.slot.control.record_stop();
        }
        Ok(id)
    }

    fn poll_operation(&mut self, now: Instant) {
        let Some(operation) = self.operation.as_mut() else {
            return;
        };
        if now >= operation.deadline && operation.failure.is_none() {
            operation.failure = Some((
                "operation deadline expired",
                self.launches.len() + self.tickets.len() + self.orphans.len(),
            ));
        }
        let Some(mut registry) = try_registry(&self.registry) else {
            return;
        };
        if registry.quarantined && operation.failure.is_none() {
            operation.failure = Some(("registry quarantined", registry.entries.len()));
        }
        let settled = registry.entries.is_empty()
            && self.launches.is_empty()
            && self.tickets.is_empty()
            && self.orphans.is_empty();
        if settled && self.signal_failures != operation.signal_failures {
            operation.failure = Some(("required stop signaling failed", 0));
        }
        if let Some((reason, unresolved)) = operation.failure {
            let id = operation.id;
            if registry.finish_operation(id, false).is_ok() {
                self.operation_results.push_back(OperationOutcome::Failed {
                    operation: id,
                    reason,
                    unresolved,
                });
                self.operation = None;
            }
        } else if settled {
            if operation.kind == OperationKind::Reload
                && !registry.shutdown_requested
                && !registry.quarantined
            {
                operation.awaiting_baseline = true;
            } else {
                let id = operation.id;
                let interrupted =
                    operation.kind == OperationKind::Reload && registry.shutdown_requested;
                if registry.finish_operation(id, false).is_ok() {
                    self.operation_results.push_back(if interrupted {
                        OperationOutcome::InterruptedByShutdown { operation: id }
                    } else {
                        OperationOutcome::Stopped { operation: id }
                    });
                    self.operation = None;
                }
            }
        }
    }

    pub fn awaiting_baseline(&self) -> Option<BorderOperationId> {
        self.operation
            .as_ref()
            .filter(|operation| operation.awaiting_baseline)
            .map(|operation| operation.id)
    }

    pub fn fail_baseline(&mut self, id: BorderOperationId) -> Result<(), CoordinatorError> {
        let operation = self
            .operation
            .as_mut()
            .filter(|operation| operation.id == id && operation.awaiting_baseline)
            .ok_or(CoordinatorError::Registry(RegistryError::Stale))?;
        operation.failure = Some(("baseline preparation failed", 0));
        self.poll_operation(Instant::now());
        Ok(())
    }

    pub fn take_operation_outcome(&mut self) -> Option<OperationOutcome> {
        self.operation_results.pop_front()
    }

    pub fn admit_baseline(
        &mut self,
        id: BorderOperationId,
        mut windows: Vec<crate::border_registry::BaselineWindow>,
        mut skipped_invalid: usize,
        skipped_ineligible: usize,
    ) -> Result<(), CoordinatorError> {
        let operation = self
            .operation
            .as_ref()
            .filter(|operation| operation.id == id && operation.awaiting_baseline)
            .ok_or(CoordinatorError::Registry(RegistryError::Stale))?;
        let deadline = operation.deadline;
        if Instant::now() >= operation.deadline {
            self.poll_operation(Instant::now());
            return Err(CoordinatorError::Registry(RegistryError::Closed));
        }
        windows.retain(|window| {
            let current = window.fingerprint.is_current();
            if !current {
                skipped_invalid += 1;
            }
            current
        });
        let eligible = windows
            .iter()
            .filter(|window| window.updates.is_some())
            .count();
        let mut registry = try_registry(&self.registry).ok_or(CoordinatorError::Deferred)?;
        let admitted = registry
            .admit_baseline_unless_shutdown(
                id,
                windows,
                Some(deadline),
                self.shutdown_submission.as_deref(),
            )
            .map_err(CoordinatorError::Registry)?;
        self.operation = None;
        self.operation_results
            .push_back(OperationOutcome::Reloaded {
                operation: id,
                old_generation_retired: true,
                skipped_invalid,
                skipped_ineligible: skipped_ineligible + eligible.saturating_sub(admitted.len()),
                admitted_instances: admitted,
                readiness: BorderReadiness::NotAwaited,
            });
        Ok(())
    }

    fn service_unchecked(&mut self) {
        if self.recovery_pending {
            let Some(mut registry) = try_registry(&self.registry) else {
                return;
            };
            registry.quarantine();
            self.recovery_pending = false;
            for launch in self.launches.values_mut() {
                if !launch.slot.owns_worker()
                    && registry
                        .entries
                        .get(&launch.slot.id)
                        .is_some_and(|entry| entry.launch_pending)
                {
                    launch.no_worker = Some(false);
                }
            }
        }
        let ready = {
            let Some(registry) = try_registry(&self.registry) else {
                return;
            };
            registry
                .entries
                .values()
                .filter(|entry| {
                    entry.phase == BorderPhase::Reserved
                        || (entry.phase == BorderPhase::StopRequested && entry.launch_pending)
                })
                .take(MAX_COORDINATOR_BATCH)
                .map(|entry| (entry.id, entry.tracking.fingerprint))
                .collect::<Vec<_>>()
        };
        for (id, fingerprint) in ready {
            if !self.launches.contains_key(&id)
                && matches!(
                    self.launch_one(id, fingerprint.is_current(), |context| {
                        thread::Builder::new()
                            .name("border-worker".into())
                            .spawn(move || run_border_worker(context))
                    }),
                    Err(CoordinatorError::Capacity)
                )
            {
                self.recovery_pending = true;
                break;
            }
        }
        #[cfg(test)]
        self.inject_panic(CoordinatorStage::BeforeAttachment);
        self.attach_retained();
        self.close_stopped();
        self.collect_finished();
        #[cfg(test)]
        self.inject_panic(CoordinatorStage::AfterCollection);
        for ticket in &mut self.tickets {
            if let Some(worker) = ticket.worker.take() {
                ticket.joined = Some(worker.join());
            }
        }
        #[cfg(test)]
        self.inject_panic(CoordinatorStage::AfterJoin);
        self.finalize_ready();
    }

    #[cfg(test)]
    fn inject_panic(&mut self, stage: CoordinatorStage) {
        if self.panic_at == Some(stage) {
            self.panic_at = None;
            panic!("injected coordinator turn interruption");
        }
    }

    fn launch_one(
        &mut self,
        id: BorderInstanceId,
        current: bool,
        spawn: impl FnOnce(BorderWorkerContext) -> std::io::Result<JoinHandle<BorderWorkerExit>>,
    ) -> Result<(), CoordinatorError> {
        self.launches
            .try_reserve(1)
            .map_err(|_| CoordinatorError::Capacity)?;
        self.orphans
            .try_reserve(1)
            .map_err(|_| CoordinatorError::Capacity)?;
        let (context, cancelled) = {
            let mut registry = try_registry(&self.registry).ok_or(CoordinatorError::Deferred)?;
            let entry = registry
                .entries
                .get(&id)
                .ok_or(CoordinatorError::Registry(RegistryError::Stale))?;
            if self.launches.contains_key(&id) || !entry.launch_pending {
                return Err(CoordinatorError::Registry(RegistryError::InvalidOwnership));
            }
            let tracking = entry.tracking;
            let control = entry.control.clone();
            let updates = entry.updates.clone();
            let initial = registry
                .initial_observations
                .contains(&tracking.observation);
            let started_at = Instant::now();
            let cancelled = !current || registry.start(id, started_at).is_err();
            if cancelled {
                registry.request_stop(id, BorderStopReason::Destroyed, started_at);
            }
            self.launches.insert(
                id,
                LaunchRecord {
                    slot: BorderLaunchSlot::new(id, control.clone()),
                    updates: updates.clone(),
                    no_worker: cancelled.then_some(false),
                    spawn_error: None,
                },
            );
            (
                BorderWorkerContext {
                    id,
                    tracking,
                    control,
                    updates,
                    registry: self.registry.clone(),
                    started_at,
                    initial,
                    #[cfg(test)]
                    probe: self.worker_probe.clone(),
                },
                cancelled,
            )
        };
        if cancelled {
            return Ok(());
        }
        if context.control.is_stopped() {
            self.launches.get_mut(&id).unwrap().no_worker = Some(false);
            return Ok(());
        }
        let launch = self.launches.get_mut(&id).unwrap();
        match spawn(context) {
            Ok(worker) => {
                if let Err(unattached) = launch.slot.retain(worker) {
                    self.orphans.push(unattached);
                    launch.slot.control.record_stop();
                    self.recovery_pending = true;
                }
            }
            Err(error) => {
                launch.no_worker = Some(true);
                launch.slot.control.record_stop();
                launch.spawn_error = Some(format!("spawn failed: {error}"));
            }
        }
        Ok(())
    }

    fn attach_retained(&mut self) {
        let Some(mut registry) = try_registry(&self.registry) else {
            return;
        };
        for launch in self
            .launches
            .values_mut()
            .filter(|launch| launch.slot.owns_worker())
            .take(MAX_COORDINATOR_BATCH)
        {
            if registry
                .entries
                .get(&launch.slot.id)
                .is_some_and(|entry| entry.launch_pending)
            {
                let _ = launch.slot.attach(&mut registry);
            } else {
                launch.slot.control.record_stop();
                if let Some(entry) = registry.entries.get(&launch.slot.id)
                    && !entry.launch_pending
                    && entry.phase != BorderPhase::StopRequested
                {
                    registry.quarantine();
                }
            }
        }
    }

    fn close_stopped(&mut self) {
        let now = Instant::now();
        let count = self.launches.len().min(MAX_COORDINATOR_BATCH);
        let start = self.closure_cursor.min(self.launches.len());
        for launch in self
            .launches
            .values()
            .skip(start)
            .chain(self.launches.values().take(start))
            .take(count)
        {
            if !launch.slot.control.is_stopped() && launch.no_worker.is_none() {
                if let Some(mut registry) = try_registry(&self.registry) {
                    registry.reset_healthy_history(launch.slot.id, now);
                }
                continue;
            }
            if launch.slot.control.signal_stop().is_err() {
                self.signal_failures = self.signal_failures.saturating_add(1);
            }
            if let MailboxClosure::Closed {
                reclaimed,
                poisoned,
            } = launch.updates.try_close()
            {
                drop(reclaimed);
                if poisoned {
                    self.recovery_pending = true;
                }
            }
        }
        self.closure_cursor = if self.launches.is_empty() {
            0
        } else {
            (start + count) % self.launches.len()
        };
    }

    fn collect_finished(&mut self) {
        if self.tickets.try_reserve(MAX_COORDINATOR_BATCH * 3).is_err() {
            self.recovery_pending = true;
            return;
        }
        let Some(mut registry) = try_registry(&self.registry) else {
            return;
        };
        for worker in registry.take_finished_workers(MAX_COORDINATOR_BATCH) {
            self.tickets.push(JoinTicket {
                worker: Some(worker),
                joined: None,
            });
        }
        let count = self.launches.len().min(MAX_COORDINATOR_BATCH);
        let start = self.recovery_cursor.min(self.launches.len());
        let ids = self
            .launches
            .keys()
            .skip(start)
            .chain(self.launches.keys().take(start))
            .take(count)
            .copied()
            .collect::<Vec<_>>();
        for id in ids {
            let launch = self.launches.get_mut(&id).unwrap();
            if let Some(worker) = launch
                .slot
                .take_finished(&mut registry)
                .or_else(|| launch.slot.take_finished_orphan(&registry))
            {
                self.tickets.push(JoinTicket {
                    worker: Some(worker),
                    joined: None,
                });
            }
        }
        self.recovery_cursor = if self.launches.is_empty() {
            0
        } else {
            (start + count) % self.launches.len()
        };
        let mut index = 0;
        let mut taken = 0;
        while index < self.orphans.len() && taken < MAX_COORDINATOR_BATCH {
            if self.orphans[index].worker.is_finished() {
                self.tickets.push(JoinTicket {
                    worker: Some(self.orphans.swap_remove(index).into_joining()),
                    joined: None,
                });
                taken += 1;
            } else {
                index += 1;
            }
        }
    }

    fn finalize_ready(&mut self) {
        let mut index = self.ticket_cursor.min(self.tickets.len().saturating_sub(1));
        let mut attempted = 0;
        let count = self.tickets.len().min(MAX_COORDINATOR_BATCH);
        while !self.tickets.is_empty() && attempted < count {
            index %= self.tickets.len();
            attempted += 1;
            let id = self.tickets[index].joined.as_ref().unwrap().id;
            if let Some(launch) = self.launches.get(&id) {
                launch.slot.control.record_stop();
                let _ = launch.updates.try_close();
                if !launch.updates.is_closed() {
                    index += 1;
                    continue;
                }
            }
            let Some(mut registry) = try_registry(&self.registry) else {
                break;
            };
            let joined = self.tickets[index].joined.take().unwrap();
            let result = if registry.entries.contains_key(&id) {
                registry.finalize_join(joined, Instant::now())
            } else {
                registry.quarantine();
                Ok(joined.outcome)
            };
            drop(registry);
            match result {
                Ok(outcome) => {
                    self.tickets.swap_remove(index);
                    if !self
                        .launches
                        .get(&id)
                        .is_some_and(|launch| launch.slot.owns_worker())
                    {
                        self.launches.remove(&id);
                    }
                    self.record_outcome(id, outcome);
                }
                Err(joined) => {
                    self.tickets[index].joined = Some(joined);
                    index += 1;
                }
            }
        }
        self.ticket_cursor = if self.tickets.is_empty() {
            0
        } else {
            index % self.tickets.len()
        };
        let absent = self
            .launches
            .values()
            .filter(|launch| launch.no_worker.is_some() && launch.updates.is_closed())
            .take(MAX_COORDINATOR_BATCH)
            .map(|launch| (launch.slot.id, launch.no_worker.unwrap()))
            .collect::<Vec<_>>();
        for (id, failed) in absent {
            let Some(mut registry) = try_registry(&self.registry) else {
                break;
            };
            let settled = if registry.entries.contains_key(&id) {
                registry
                    .settle_no_worker(id, failed, Instant::now())
                    .is_ok()
            } else {
                registry.quarantine();
                true
            };
            drop(registry);
            if settled && let Some(launch) = self.launches.remove(&id) {
                let outcome = if failed {
                    BorderWorkerExit {
                        primary: crate::border_registry::BorderWorkerResult::InitializationFailed(
                            launch.spawn_error.unwrap_or_else(|| "spawn failed".into()),
                        ),
                        cleanup_failures: Vec::new(),
                    }
                } else {
                    BorderWorkerExit::completed()
                };
                self.record_outcome(id, outcome);
            }
        }
    }

    fn record_outcome(&mut self, id: BorderInstanceId, outcome: BorderWorkerExit) {
        if self.outcomes.len() == MAX_TERMINAL_OUTCOMES {
            self.outcomes.pop_front();
            self.dropped_outcomes = self.dropped_outcomes.saturating_add(1);
        }
        self.outcomes.push_back((id, outcome));
    }

    pub fn take_outcome(&mut self) -> Option<(BorderInstanceId, BorderWorkerExit)> {
        self.outcomes.pop_front()
    }

    pub fn is_settled(&self) -> bool {
        self.launches.is_empty()
            && self.tickets.is_empty()
            && self.orphans.is_empty()
            && try_registry(&self.registry).is_some_and(|registry| registry.entries.is_empty())
    }
}

pub struct PreparedBaseline {
    pub windows: Vec<crate::border_registry::BaselineWindow>,
    pub skipped_invalid: usize,
    pub skipped_ineligible: usize,
}

enum BaselineFailure {
    Deferred,
    Failed,
}

fn prepare_native_baseline() -> Result<PreparedBaseline, BaselineFailure> {
    let config = match crate::APP_STATE.config.try_read() {
        Ok(config) => config.clone(),
        Err(TryLockError::WouldBlock) => return Err(BaselineFailure::Deferred),
        Err(TryLockError::Poisoned(_)) => return Err(BaselineFailure::Failed),
    };
    let captured = crate::enumerate_tracking_windows().map_err(|_| BaselineFailure::Failed)?;
    let mut baseline = PreparedBaseline {
        windows: Vec::new(),
        skipped_invalid: 0,
        skipped_ineligible: 0,
    };
    baseline
        .windows
        .try_reserve(captured.len())
        .map_err(|_| BaselineFailure::Failed)?;
    let own_process = unsafe { windows::Win32::System::Threading::GetCurrentProcessId() };
    for hwnd in captured {
        let Some(fingerprint) = crate::border_registry::TrackingWindowFingerprint::capture(hwnd)
        else {
            baseline.skipped_invalid += 1;
            continue;
        };
        if fingerprint.process_id == own_process || !crate::utils::is_window_top_level(hwnd) {
            continue;
        }
        let rule = crate::utils::get_window_rule_from_config(hwnd, &config);
        let eligible = crate::utils::is_window_visible(hwnd)
            && !crate::utils::is_window_cloaked(hwnd)
            && rule.enabled != Some(crate::config::EnableMode::Bool(false))
            && (rule.enabled == Some(crate::config::EnableMode::Bool(true))
                || !crate::utils::has_filtered_style(hwnd));
        let updates = if eligible {
            let control = Arc::new(
                crate::border_registry::BorderControl::new()
                    .map_err(|_| BaselineFailure::Failed)?,
            );
            Some(Arc::new(
                BorderUpdateMailbox::new(control).map_err(|_| BaselineFailure::Failed)?,
            ))
        } else {
            baseline.skipped_ineligible += 1;
            None
        };
        baseline
            .windows
            .push(crate::border_registry::BaselineWindow {
                fingerprint,
                updates,
            });
    }
    Ok(baseline)
}

struct LifecycleRequest {
    kind: OperationKind,
    deadline: Instant,
    reply: SyncSender<Result<OperationOutcome, CoordinatorError>>,
}

struct CoordinatorSignals {
    wake: crate::border_registry::BorderEvent,
    completion: crate::border_registry::BorderEvent,
    shutdown_submission: Arc<AtomicBool>,
    terminal: AtomicBool,
    failed: AtomicBool,
    #[cfg(test)]
    inject_wait_failure: AtomicBool,
    #[cfg(test)]
    inject_completion_failure: AtomicBool,
}

impl CoordinatorSignals {
    fn wait_for_work_failed(&self) -> bool {
        let handle = self.wake.handle();
        #[cfg(test)]
        let handle = if self.inject_wait_failure.load(Ordering::Acquire) {
            windows::Win32::Foundation::HANDLE::default()
        } else {
            handle
        };
        let wait = unsafe { windows::Win32::System::Threading::WaitForSingleObject(handle, 100) };
        wait == windows::Win32::Foundation::WAIT_FAILED
    }

    fn signal_completion(&self) -> windows::core::Result<()> {
        #[cfg(test)]
        if self.inject_completion_failure.load(Ordering::Acquire) {
            return unsafe {
                windows::Win32::System::Threading::SetEvent(
                    windows::Win32::Foundation::HANDLE::default(),
                )
            };
        }
        self.completion.signal()
    }
}

pub struct LifecycleReceipt {
    receiver: Receiver<Result<OperationOutcome, CoordinatorError>>,
    deadline: Instant,
}

impl LifecycleReceipt {
    pub fn wait(self) -> anyhow::Result<OperationOutcome> {
        self.receiver
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
            .map_err(|error| anyhow::anyhow!("lifecycle result wait: {error}"))?
            .map_err(anyhow::Error::from)
    }

    pub fn try_result(
        &self,
    ) -> Result<Option<Result<OperationOutcome, CoordinatorError>>, TryRecvError> {
        match self.receiver.try_recv() {
            Ok(result) => Ok(Some(result)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

pub struct BorderLifecycleCoordinator {
    registry: Arc<Mutex<BorderRegistry>>,
    requests: SyncSender<LifecycleRequest>,
    signals: Arc<CoordinatorSignals>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl BorderLifecycleCoordinator {
    pub fn start(registry: Arc<Mutex<BorderRegistry>>) -> anyhow::Result<Arc<Self>> {
        Self::start_with_baseline(registry, prepare_native_baseline)
    }

    fn start_with_baseline(
        registry: Arc<Mutex<BorderRegistry>>,
        baseline: impl FnMut() -> Result<PreparedBaseline, BaselineFailure> + Send + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        Self::start_coordinator(BorderCoordinator::new(registry), baseline)
    }

    #[cfg(test)]
    pub(crate) fn start_with_empty_baseline_for_test(
        registry: Arc<Mutex<BorderRegistry>>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::start_with_baseline(registry, || {
            Ok(PreparedBaseline {
                windows: Vec::new(),
                skipped_invalid: 0,
                skipped_ineligible: 0,
            })
        })
    }

    #[cfg(test)]
    pub(crate) fn start_with_worker_probe(
        registry: Arc<Mutex<BorderRegistry>>,
        probe: Arc<dyn Fn(crate::border_runtime::WorkerStage) + Send + Sync>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut coordinator = BorderCoordinator::new(registry);
        coordinator.worker_probe = Some(probe);
        Self::start_coordinator(coordinator, prepare_native_baseline)
    }

    fn start_coordinator(
        coordinator: BorderCoordinator,
        baseline: impl FnMut() -> Result<PreparedBaseline, BaselineFailure> + Send + 'static,
    ) -> anyhow::Result<Arc<Self>> {
        let registry = coordinator.registry.clone();
        let signals = Arc::new(CoordinatorSignals {
            wake: crate::border_registry::BorderEvent::new(false)?,
            completion: crate::border_registry::BorderEvent::new(false)?,
            shutdown_submission: Arc::new(AtomicBool::new(false)),
            terminal: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            #[cfg(test)]
            inject_wait_failure: AtomicBool::new(false),
            #[cfg(test)]
            inject_completion_failure: AtomicBool::new(false),
        });
        let (requests, receiver) = mpsc::sync_channel(MAX_COORDINATOR_BATCH);
        let worker_signals = signals.clone();
        let owner = Arc::new(Self {
            registry,
            requests,
            signals,
            worker: Mutex::new(None),
        });
        let mut retained = owner
            .worker
            .lock()
            .map_err(|_| anyhow::anyhow!("coordinator startup state poisoned"))?;
        *retained = Some(
            thread::Builder::new()
                .name("border-coordinator".into())
                .spawn(move || {
                    run_coordinator_host(coordinator, receiver, worker_signals, baseline)
                })?,
        );
        drop(retained);
        Ok(owner)
    }

    pub fn request(
        &self,
        kind: OperationKind,
        timeout: Duration,
    ) -> anyhow::Result<LifecycleReceipt> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| anyhow::anyhow!("lifecycle deadline overflow"))?;
        if kind == OperationKind::Shutdown {
            self.request_shutdown();
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        self.requests
            .try_send(LifecycleRequest {
                kind,
                deadline,
                reply,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => anyhow::anyhow!("lifecycle request queue full"),
                TrySendError::Disconnected(_) => anyhow::anyhow!("lifecycle coordinator exited"),
            })?;
        let _ = self.signals.wake.signal();
        Ok(LifecycleReceipt { receiver, deadline })
    }

    pub fn request_shutdown(&self) {
        self.signals
            .shutdown_submission
            .store(true, Ordering::Release);
        if let Some(mut registry) = try_registry(&self.registry) {
            registry.shutdown_requested = true;
            registry.admission = crate::border_registry::BorderAdmission::ShuttingDown;
            for entry in registry.entries.values() {
                entry.control.record_stop();
            }
        }
        let _ = self.signals.wake.signal();
    }

    pub fn wake(&self) {
        let _ = self.signals.wake.signal();
    }
    pub fn completion_event(&self) -> windows::Win32::Foundation::HANDLE {
        self.signals.completion.handle()
    }
    #[cfg(test)]
    pub(crate) fn fail_completion_signal_for_test(&self) {
        self.signals
            .inject_completion_failure
            .store(true, Ordering::Release);
    }
    pub fn is_terminal(&self) -> bool {
        self.signals.terminal.load(Ordering::Acquire)
    }

    pub fn has_joined(&self) -> bool {
        self.worker.try_lock().is_ok_and(|worker| worker.is_none())
    }

    pub fn join_terminal(&self) -> anyhow::Result<()> {
        if !self.is_terminal() {
            return Err(anyhow::anyhow!("coordinator still owns live work"));
        }
        let mut worker = self
            .worker
            .lock()
            .map_err(|_| anyhow::anyhow!("coordinator join state poisoned"))?;
        if worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return Err(anyhow::anyhow!(
                "coordinator completion is not yet joinable"
            ));
        }
        if let Some(worker) = worker.take()
            && let Err(payload) = worker.join()
        {
            discard_panic(payload);
            return Err(anyhow::anyhow!("coordinator thread panicked"));
        }
        if self.signals.failed.load(Ordering::Acquire) {
            return Err(anyhow::anyhow!(
                "coordinator failed while retaining/settling ownership"
            ));
        }
        Ok(())
    }
}

fn run_coordinator_host(
    mut coordinator: BorderCoordinator,
    requests: Receiver<LifecycleRequest>,
    signals: Arc<CoordinatorSignals>,
    mut prepare: impl FnMut() -> Result<PreparedBaseline, BaselineFailure>,
) {
    coordinator.shutdown_submission = Some(signals.shutdown_submission.clone());
    let mut replies = VecDeque::with_capacity(MAX_COORDINATOR_BATCH);
    let mut staged_request = None;
    let mut wait_failed = false;
    loop {
        let turn = catch_unwind(AssertUnwindSafe(|| {
            if signals.shutdown_submission.swap(false, Ordering::AcqRel) {
                let deadline = Instant::now()
                    .checked_add(Duration::from_secs(10))
                    .unwrap_or_else(Instant::now);
                if coordinator.begin_operation(OperationKind::Shutdown, deadline)
                    == Err(CoordinatorError::Deferred)
                {
                    signals.shutdown_submission.store(true, Ordering::Release);
                }
            }
            for _ in 0..MAX_COORDINATOR_BATCH {
                let request = match staged_request
                    .take()
                    .map_or_else(|| requests.try_recv(), Ok)
                {
                    Ok(request) => request,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        signals.shutdown_submission.store(true, Ordering::Release);
                        break;
                    }
                };
                if replies.len() == MAX_COORDINATOR_BATCH {
                    let _ = request.reply.try_send(Err(CoordinatorError::Capacity));
                    continue;
                }
                match coordinator.begin_operation(request.kind, request.deadline) {
                    Ok(id) => replies.push_back((id, request.reply)),
                    Err(CoordinatorError::Deferred) => {
                        staged_request = Some(request);
                        break;
                    }
                    Err(error) => {
                        let _ = request.reply.try_send(Err(error));
                    }
                }
            }
            coordinator.service_turn();
            if let Some(id) = coordinator.awaiting_baseline() {
                match prepare() {
                    Ok(baseline) => {
                        match coordinator.admit_baseline(
                            id,
                            baseline.windows,
                            baseline.skipped_invalid,
                            baseline.skipped_ineligible,
                        ) {
                            Ok(()) | Err(CoordinatorError::Deferred) => {}
                            Err(CoordinatorError::Registry(RegistryError::Closed)) => {
                                coordinator.service_turn();
                                if coordinator.awaiting_baseline() == Some(id) {
                                    let _ = coordinator.fail_baseline(id);
                                }
                            }
                            Err(_) => {
                                let _ = coordinator.fail_baseline(id);
                            }
                        }
                    }
                    Err(BaselineFailure::Deferred) => {}
                    Err(BaselineFailure::Failed) => {
                        let _ = coordinator.fail_baseline(id);
                    }
                }
            }
            while let Some(outcome) = coordinator.take_operation_outcome() {
                deliver_operation_outcome(&mut replies, outcome);
                let _ = signals.signal_completion();
            }
        }));
        if let Err(payload) = turn {
            signals.failed.store(true, Ordering::Release);
            coordinator.recovery_pending = true;
            for launch in coordinator.launches.values() {
                launch.slot.control.record_stop();
            }
            discard_panic(payload);
        }
        if coordinator.is_settled()
            && try_registry(&coordinator.registry).is_some_and(|registry| {
                registry.shutdown_requested && registry.active_operation.is_none()
            })
        {
            signals.terminal.store(true, Ordering::Release);
            let _ = signals.signal_completion();
            return;
        }
        if wait_failed && staged_request.is_some() {
            thread::park_timeout(Duration::from_millis(100));
        } else if wait_failed {
            match requests.recv_timeout(Duration::from_millis(100)) {
                Ok(request) => staged_request = Some(request),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    signals.shutdown_submission.store(true, Ordering::Release);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        } else if signals.wait_for_work_failed() {
            wait_failed = true;
            signals.failed.store(true, Ordering::Release);
            signals.shutdown_submission.store(true, Ordering::Release);
            coordinator.recovery_pending = true;
        }
    }
}

fn deliver_operation_outcome(
    replies: &mut VecDeque<(
        BorderOperationId,
        SyncSender<Result<OperationOutcome, CoordinatorError>>,
    )>,
    outcome: OperationOutcome,
) {
    if let Some(position) = replies
        .iter()
        .position(|(id, _)| *id == outcome.operation())
        && let Some((_, sender)) = replies.remove(position)
    {
        let _ = sender.try_send(Ok(outcome));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::border_registry::{
        BorderAdmission, BorderControl, ReserveResult, TrackingWindowFingerprint,
    };
    use std::time::Duration;

    fn reserve(registry: &mut BorderRegistry, hwnd: isize) -> BorderInstanceId {
        let identity = registry
            .observe(TrackingWindowFingerprint {
                hwnd,
                process_id: 2,
                thread_id: 3,
            })
            .unwrap();
        let updates =
            Arc::new(BorderUpdateMailbox::new(Arc::new(BorderControl::new().unwrap())).unwrap());
        match registry.reserve(identity, updates, Instant::now()).unwrap() {
            ReserveResult::Reserved(id) => id,
            _ => panic!(),
        }
    }

    fn fixture() -> (BorderCoordinator, BorderInstanceId) {
        let mut registry = BorderRegistry::default();
        registry.admission = BorderAdmission::Open;
        let id = reserve(&mut registry, 1);
        (BorderCoordinator::new(Arc::new(Mutex::new(registry))), id)
    }

    fn settle(coordinator: &mut BorderCoordinator) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !coordinator.is_settled() {
            assert!(
                Instant::now() < deadline,
                "coordinator did not retain/reap ownership"
            );
            coordinator.service_turn();
            thread::yield_now();
        }
    }

    struct HostedFixture(Arc<BorderLifecycleCoordinator>);

    impl Drop for HostedFixture {
        fn drop(&mut self) {
            self.0.request_shutdown();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !self.0.is_terminal()
                || self
                    .0
                    .worker
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|worker| !worker.is_finished())
            {
                assert!(Instant::now() < deadline, "test coordinator did not settle");
                thread::yield_now();
            }
            let result = self.0.join_terminal();
            if self.0.signals.inject_wait_failure.load(Ordering::Acquire) {
                assert!(self.0.has_joined(), "failed host join abandoned its handle");
            } else {
                result.unwrap();
            }
        }
    }

    fn empty_baseline() -> Result<PreparedBaseline, BaselineFailure> {
        Ok(PreparedBaseline {
            windows: Vec::new(),
            skipped_invalid: 0,
            skipped_ineligible: 0,
        })
    }

    #[test]
    fn operation_receipts_are_correlated_when_results_are_backlogged() {
        let (mut coordinator, _) = fixture();
        let first = coordinator
            .begin_operation(OperationKind::Stop, Instant::now() + Duration::from_secs(2))
            .unwrap();
        settle(&mut coordinator);
        let second = coordinator
            .begin_operation(OperationKind::Stop, Instant::now() + Duration::from_secs(2))
            .unwrap();
        settle(&mut coordinator);
        coordinator.service_turn();
        let (first_sender, first_receiver) = mpsc::sync_channel(1);
        let (second_sender, second_receiver) = mpsc::sync_channel(1);
        let mut replies = VecDeque::from([(second, second_sender), (first, first_sender)]);
        while let Some(outcome) = coordinator.take_operation_outcome() {
            deliver_operation_outcome(&mut replies, outcome);
        }
        assert_eq!(
            first_receiver.try_recv().unwrap(),
            Ok(OperationOutcome::Stopped { operation: first })
        );
        assert_eq!(
            second_receiver.try_recv().unwrap(),
            Ok(OperationOutcome::Stopped { operation: second })
        );
        assert!(replies.is_empty());
    }

    #[test]
    fn hosted_request_survives_registry_contention_without_resetting_deadline() {
        let registry = Arc::new(Mutex::new(BorderRegistry::default()));
        let host = HostedFixture(
            BorderLifecycleCoordinator::start_with_baseline(registry.clone(), empty_baseline)
                .unwrap(),
        );
        let guard = registry.lock().unwrap();
        let receipt = host
            .0
            .request(OperationKind::Reload, Duration::from_secs(2))
            .unwrap();
        let original_deadline = receipt.deadline;
        let check_until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < check_until {
            assert!(
                matches!(receipt.receiver.try_recv(), Err(TryRecvError::Empty)),
                "registry contention must retain the request instead of rejecting it"
            );
            thread::yield_now();
        }
        assert_eq!(receipt.deadline, original_deadline);
        drop(guard);
        host.0.wake();
        assert!(
            matches!(receipt.wait().unwrap(), OperationOutcome::Reloaded {
            admitted_instances, readiness: BorderReadiness::NotAwaited, ..
        } if admitted_instances.is_empty())
        );
        assert_eq!(registry.lock().unwrap().admission, BorderAdmission::Open);
    }

    #[test]
    fn hosted_wait_failure_preserves_staged_requests_until_safe_shutdown() {
        let registry = Arc::new(Mutex::new(BorderRegistry::default()));
        let host = HostedFixture(
            BorderLifecycleCoordinator::start_with_baseline(registry.clone(), empty_baseline)
                .unwrap(),
        );
        let guard = registry.lock().unwrap();
        let first = host
            .0
            .request(OperationKind::Reload, Duration::from_secs(2))
            .unwrap();
        let original_deadline = first.deadline;
        host.0
            .signals
            .inject_wait_failure
            .store(true, Ordering::Release);
        host.0.wake();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !host.0.signals.failed.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "native wait failure was not observed"
            );
            thread::yield_now();
        }
        let second = host
            .0
            .request(OperationKind::Stop, Duration::from_secs(2))
            .unwrap();
        let check_until = Instant::now() + Duration::from_millis(250);
        while Instant::now() < check_until {
            assert!(
                matches!(first.receiver.try_recv(), Err(TryRecvError::Empty)),
                "staged request was discarded during failed-wait fallback"
            );
            assert!(
                matches!(second.receiver.try_recv(), Err(TryRecvError::Empty)),
                "queued request was consumed while the first remained deferred"
            );
            thread::yield_now();
        }
        assert_eq!(first.deadline, original_deadline);
        drop(guard);
        assert!(
            first
                .receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        assert!(
            second
                .receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        while !host.0.is_terminal()
            || host
                .0
                .worker
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
        {
            assert!(
                Instant::now() < deadline,
                "failed-wait shutdown did not settle"
            );
            thread::yield_now();
        }
        assert!(host.0.join_terminal().is_err());
        assert!(host.0.has_joined());
        let registry = registry.lock().unwrap();
        assert!(registry.shutdown_requested);
        assert_eq!(registry.admission, BorderAdmission::ShuttingDown);
        assert!(registry.entries.is_empty());
    }

    #[test]
    fn failed_completion_signal_preserves_receipt_and_terminal_join() {
        let registry = Arc::new(Mutex::new(BorderRegistry::default()));
        let host = HostedFixture(
            BorderLifecycleCoordinator::start_with_baseline(registry.clone(), empty_baseline)
                .unwrap(),
        );
        host.0
            .signals
            .inject_completion_failure
            .store(true, Ordering::Release);
        let receipt = host
            .0
            .request(OperationKind::Reload, Duration::from_secs(2))
            .unwrap();
        assert!(matches!(
            receipt.wait().unwrap(),
            OperationOutcome::Reloaded { .. }
        ));
        assert_eq!(
            unsafe {
                windows::Win32::System::Threading::WaitForSingleObject(host.0.completion_event(), 0)
            },
            windows::Win32::Foundation::WAIT_TIMEOUT
        );
        host.0.request_shutdown();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !host.0.is_terminal()
            || host
                .0
                .worker
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
        {
            assert!(
                Instant::now() < deadline,
                "shutdown depended on failed completion signal"
            );
            thread::yield_now();
        }
        host.0.join_terminal().unwrap();
        assert!(host.0.has_joined());
        assert_eq!(
            unsafe {
                windows::Win32::System::Threading::WaitForSingleObject(host.0.completion_event(), 0)
            },
            windows::Win32::Foundation::WAIT_TIMEOUT
        );
        assert!(registry.lock().unwrap().shutdown_requested);
    }

    #[test]
    fn hosted_reload_and_terminal_shutdown_join_without_desktop_admission() {
        let registry = Arc::new(Mutex::new(BorderRegistry::default()));
        let host = HostedFixture(
            BorderLifecycleCoordinator::start_with_baseline(registry.clone(), empty_baseline)
                .unwrap(),
        );
        let receipt = host
            .0
            .request(OperationKind::Reload, Duration::from_secs(2))
            .unwrap();
        assert!(
            matches!(receipt.wait().unwrap(), OperationOutcome::Reloaded { admitted_instances, readiness: BorderReadiness::NotAwaited, .. } if admitted_instances.is_empty())
        );
        assert_eq!(registry.lock().unwrap().admission, BorderAdmission::Open);
    }

    #[test]
    fn shutdown_submission_bypasses_full_host_request_queue() {
        let registry = Arc::new(Mutex::new(BorderRegistry::default()));
        let (preparing, prepared) = mpsc::channel();
        let (resume, paused) = mpsc::channel();
        let host = HostedFixture(
            BorderLifecycleCoordinator::start_with_baseline(registry.clone(), move || {
                let _ = preparing.send(());
                let _ = paused.recv_timeout(Duration::from_secs(2));
                empty_baseline()
            })
            .unwrap(),
        );
        let reload = host
            .0
            .request(OperationKind::Reload, Duration::from_secs(2))
            .unwrap();
        prepared.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut receipts = Vec::new();
        for _ in 0..MAX_COORDINATOR_BATCH {
            receipts.push(
                host.0
                    .request(OperationKind::Stop, Duration::from_secs(2))
                    .unwrap(),
            );
        }
        assert!(
            host.0
                .request(OperationKind::Reload, Duration::from_secs(2))
                .is_err()
        );
        host.0.request_shutdown();
        assert_eq!(
            registry.lock().unwrap().admission,
            BorderAdmission::ShuttingDown
        );
        resume.send(()).unwrap();
        assert!(matches!(
            reload.wait().unwrap(),
            OperationOutcome::InterruptedByShutdown { .. }
        ));
        assert!(registry.lock().unwrap().entries.is_empty());
        drop(receipts);
    }

    #[test]
    fn spawn_failure_is_settled_without_a_join() {
        let (mut coordinator, id) = fixture();
        coordinator
            .launch_one(id, true, |_| {
                Err(std::io::Error::other("injected spawn failure"))
            })
            .unwrap();
        assert_eq!(coordinator.launches[&id].no_worker, Some(true));
        settle(&mut coordinator);
        assert!(
            coordinator
                .registry
                .lock()
                .unwrap()
                .observations
                .values()
                .any(|observation| observation.consecutive_failures == 1)
        );
    }

    #[test]
    fn fast_completion_is_retained_attached_and_joined() {
        let (mut coordinator, id) = fixture();
        coordinator
            .launch_one(id, true, |_| {
                thread::Builder::new().spawn(BorderWorkerExit::completed)
            })
            .unwrap();
        assert!(coordinator.launches[&id].slot.owns_worker());
        settle(&mut coordinator);
        assert_eq!(coordinator.take_outcome().unwrap().0, id);
    }

    #[test]
    fn held_mailbox_preserves_joined_ticket_after_worker_exits() {
        let (mut coordinator, id) = fixture();
        let updates = coordinator.registry.lock().unwrap().entries[&id]
            .updates
            .clone();
        coordinator
            .launch_one(id, true, |_| {
                thread::Builder::new().spawn(BorderWorkerExit::completed)
            })
            .unwrap();
        updates.while_locked(|| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while coordinator.tickets.is_empty() {
                assert!(Instant::now() < deadline);
                coordinator.service_turn();
                thread::yield_now();
            }
            assert!(coordinator.tickets[0].joined.is_some());
            assert!(!coordinator.is_settled());
            assert!(coordinator.take_outcome().is_none());
        });
        settle(&mut coordinator);
        assert_eq!(coordinator.take_outcome().unwrap().0, id);
    }

    #[test]
    fn pending_shutdown_submission_wins_atomic_reload_commit() {
        let (mut coordinator, _) = fixture();
        let operation = coordinator
            .begin_operation(
                OperationKind::Reload,
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
        settle(&mut coordinator);
        coordinator.shutdown_submission = Some(Arc::new(AtomicBool::new(true)));
        assert_eq!(
            coordinator.admit_baseline(operation, Vec::new(), 0, 0),
            Err(CoordinatorError::Registry(RegistryError::Closed))
        );
        coordinator.service_turn();
        assert_eq!(
            coordinator.take_operation_outcome(),
            Some(OperationOutcome::InterruptedByShutdown { operation })
        );
        assert_eq!(
            coordinator.registry.lock().unwrap().admission,
            BorderAdmission::ShuttingDown
        );
    }

    #[test]
    fn empty_reload_reports_admission_without_readiness() {
        let (mut coordinator, _) = fixture();
        let operation = coordinator
            .begin_operation(
                OperationKind::Reload,
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
        settle(&mut coordinator);
        coordinator
            .admit_baseline(operation, Vec::new(), 0, 0)
            .unwrap();
        assert!(
            matches!(coordinator.take_operation_outcome(), Some(OperationOutcome::Reloaded { operation: actual, old_generation_retired: true, admitted_instances, readiness: BorderReadiness::NotAwaited, .. }) if actual == operation && admitted_instances.is_empty())
        );
        assert_eq!(
            coordinator.registry.lock().unwrap().admission,
            BorderAdmission::Open
        );
    }

    #[test]
    fn concurrent_reload_is_busy_and_shutdown_prevents_baseline_reopen() {
        let (mut coordinator, _) = fixture();
        let deadline = Instant::now() + Duration::from_secs(2);
        let id = coordinator
            .begin_operation(OperationKind::Reload, deadline)
            .unwrap();
        assert_eq!(
            coordinator.begin_operation(OperationKind::Reload, deadline),
            Err(CoordinatorError::Registry(RegistryError::Busy))
        );
        assert_eq!(
            coordinator.begin_operation(OperationKind::Shutdown, deadline),
            Err(CoordinatorError::Registry(RegistryError::Busy))
        );
        settle(&mut coordinator);
        assert!(coordinator.awaiting_baseline().is_none());
        assert_eq!(
            coordinator.take_operation_outcome(),
            Some(OperationOutcome::InterruptedByShutdown { operation: id })
        );
        assert_eq!(
            coordinator.registry.lock().unwrap().admission,
            BorderAdmission::ShuttingDown
        );
    }

    #[test]
    fn operation_timeout_preserves_live_worker_and_admission_pause() {
        let (mut coordinator, id) = fixture();
        let (release, waiting) = std::sync::mpsc::channel::<()>();
        coordinator
            .launch_one(id, true, |_| {
                thread::Builder::new().spawn(move || {
                    let _ = waiting.recv();
                    BorderWorkerExit::completed()
                })
            })
            .unwrap();
        let operation = coordinator
            .begin_operation(OperationKind::Reload, Instant::now())
            .unwrap();
        coordinator.service_turn();
        let outcome = coordinator.take_operation_outcome();
        let retained = !coordinator.is_settled();
        let admission = coordinator.registry.lock().unwrap().admission;
        drop(release);
        settle(&mut coordinator);
        assert!(
            matches!(outcome, Some(OperationOutcome::Failed { operation: actual, reason: "operation deadline expired", .. }) if actual == operation)
        );
        assert!(retained);
        assert_eq!(admission, BorderAdmission::Paused);
        assert_eq!(
            coordinator.registry.lock().unwrap().admission,
            BorderAdmission::Paused
        );
    }

    #[test]
    fn baseline_failure_keeps_admission_closed_without_replacements() {
        let (mut coordinator, _) = fixture();
        let operation = coordinator
            .begin_operation(
                OperationKind::Reload,
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
        settle(&mut coordinator);
        assert_eq!(coordinator.awaiting_baseline(), Some(operation));
        coordinator.fail_baseline(operation).unwrap();
        assert!(matches!(
            coordinator.take_operation_outcome(),
            Some(OperationOutcome::Failed {
                reason: "baseline preparation failed",
                ..
            })
        ));
        assert_eq!(
            coordinator.registry.lock().unwrap().admission,
            BorderAdmission::Paused
        );
        assert!(coordinator.is_settled());
    }

    #[test]
    fn coordinator_turn_panics_preserve_launch_and_join_ownership() {
        for stage in [
            CoordinatorStage::BeforeAttachment,
            CoordinatorStage::AfterCollection,
            CoordinatorStage::AfterJoin,
        ] {
            let (mut coordinator, id) = fixture();
            coordinator
                .launch_one(id, true, |_| {
                    thread::Builder::new().spawn(BorderWorkerExit::completed)
                })
                .unwrap();
            coordinator.panic_at = Some(stage);
            coordinator.service_turn();
            assert!(coordinator.recovery_pending);
            assert!(coordinator.launches.contains_key(&id));
            assert!(!coordinator.is_settled());
            settle(&mut coordinator);
            assert!(coordinator.registry.lock().unwrap().quarantined);
            assert_eq!(coordinator.take_outcome().unwrap().0, id);
        }
    }

    #[test]
    fn missing_entry_does_not_abandon_recovery_handle() {
        let (mut coordinator, id) = fixture();
        coordinator
            .launch_one(id, true, |_| {
                thread::Builder::new().spawn(BorderWorkerExit::completed)
            })
            .unwrap();
        coordinator.registry.lock().unwrap().entries.remove(&id);
        settle(&mut coordinator);
        assert!(coordinator.registry.lock().unwrap().quarantined);
        assert_eq!(coordinator.take_outcome().unwrap().0, id);
    }

    #[test]
    fn unfinished_recovery_slots_do_not_starve_finished_ones() {
        let (mut coordinator, first) = fixture();
        let mut ids = vec![first];
        {
            let mut registry = coordinator.registry.lock().unwrap();
            for hwnd in 2..=130 {
                ids.push(reserve(&mut registry, hwnd));
            }
        }
        let mut releases = HashMap::new();
        for id in ids {
            let (release, wait) = std::sync::mpsc::channel();
            coordinator
                .launch_one(id, true, |_| {
                    thread::Builder::new().spawn(move || {
                        let _ = wait.recv();
                        BorderWorkerExit::completed()
                    })
                })
                .unwrap();
            coordinator
                .registry
                .lock()
                .unwrap()
                .recover_launch(id)
                .unwrap();
            releases.insert(id, release);
        }
        let blocked = coordinator
            .launches
            .keys()
            .take(MAX_COORDINATOR_BATCH)
            .copied()
            .collect::<std::collections::HashSet<_>>();
        for (id, release) in &releases {
            if !blocked.contains(id) {
                let _ = release.send(());
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while coordinator.outcomes.len() < 66 && Instant::now() < deadline {
            coordinator.service_turn();
            thread::yield_now();
        }
        let progressed = coordinator.outcomes.len() >= 66;
        drop(releases);
        settle(&mut coordinator);
        assert!(
            progressed,
            "unfinished recovery slots starved other completed workers"
        );
        assert_eq!(coordinator.outcomes.len(), 130);
    }

    #[test]
    fn more_than_sixty_four_workers_retire_incrementally() {
        let (mut coordinator, first) = fixture();
        let mut ids = vec![first];
        {
            let mut registry = coordinator.registry.lock().unwrap();
            for hwnd in 2..=130 {
                ids.push(reserve(&mut registry, hwnd));
            }
        }
        for id in ids {
            coordinator
                .launch_one(id, true, |_| {
                    thread::Builder::new().spawn(BorderWorkerExit::completed)
                })
                .unwrap();
        }
        settle(&mut coordinator);
        assert_eq!(coordinator.outcomes.len(), 130);
    }
}
