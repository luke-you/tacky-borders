use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindow};

use crate::border_updates::BorderUpdateMailbox;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TrackingWindowFingerprint {
    pub hwnd: isize,
    pub process_id: u32,
    pub thread_id: u32,
}

impl TrackingWindowFingerprint {
    pub fn capture(hwnd: HWND) -> Option<Self> {
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return None;
        }
        let mut process_id = 0;
        let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
        (thread_id != 0 && process_id != 0).then_some(Self {
            hwnd: hwnd.0 as isize,
            process_id,
            thread_id,
        })
    }

    pub fn is_current(self) -> bool {
        Self::capture(HWND(self.hwnd as _)) == Some(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TrackingObservationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TrackingWindowIdentity {
    pub fingerprint: TrackingWindowFingerprint,
    pub observation: TrackingObservationId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BorderInstanceId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BorderOperationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderAdmission {
    Open,
    Paused,
    ShuttingDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderPhase {
    Reserved,
    Starting,
    Running,
    StopRequested,
    Joining,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderHandleOwner {
    None,
    Launch,
    Entry,
    Recovery,
    JoinTicket,
}

impl BorderPhase {
    pub fn permits_owner(self, launch_pending: bool, owner: BorderHandleOwner) -> bool {
        matches!(
            (self, launch_pending, owner),
            (Self::Reserved, true, BorderHandleOwner::None)
                | (
                    Self::Starting | Self::Running,
                    true,
                    BorderHandleOwner::Launch
                )
                | (
                    Self::Starting | Self::Running,
                    false,
                    BorderHandleOwner::Entry
                )
                | (
                    Self::StopRequested,
                    true,
                    BorderHandleOwner::None | BorderHandleOwner::Launch
                )
                | (
                    Self::StopRequested,
                    false,
                    BorderHandleOwner::Entry | BorderHandleOwner::Recovery,
                )
                | (Self::Joining, false, BorderHandleOwner::JoinTicket)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderStopReason {
    Destroyed,
    Replaced,
    Reload,
    Shutdown,
    Explicit,
    Failed,
}

#[derive(Debug, Clone, Copy)]
pub struct BorderStopRequest {
    pub reason: BorderStopReason,
    pub requested_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredVisibility {
    Unknown = 0,
    Hidden = 1,
    Visible = 2,
}

#[derive(Debug)]
pub struct BorderEvent(HANDLE);

unsafe impl Send for BorderEvent {}
unsafe impl Sync for BorderEvent {}

impl BorderEvent {
    pub fn new(manual_reset: bool) -> windows::core::Result<Self> {
        unsafe { CreateEventW(None, manual_reset, false, None) }.map(Self)
    }

    pub fn handle(&self) -> HANDLE {
        self.0
    }

    pub fn signal(&self) -> windows::core::Result<()> {
        unsafe { SetEvent(self.0) }
    }
}

impl Drop for BorderEvent {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[derive(Debug)]
pub struct BorderControl {
    stop_requested: AtomicBool,
    callback_failed: AtomicBool,
    desired_visibility: AtomicU8,
    pending_work: AtomicU8,
    pub stop_event: BorderEvent,
}

impl BorderControl {
    pub fn new() -> windows::core::Result<Self> {
        Ok(Self {
            stop_requested: AtomicBool::new(false),
            callback_failed: AtomicBool::new(false),
            desired_visibility: AtomicU8::new(DesiredVisibility::Unknown as u8),
            pending_work: AtomicU8::new(0),
            stop_event: BorderEvent::new(true)?,
        })
    }

    pub fn is_stopped(&self) -> bool {
        self.stop_requested.load(Ordering::Acquire)
    }

    pub(crate) fn request_work(&self, bits: u8) {
        self.pending_work.fetch_or(bits, Ordering::AcqRel);
    }
    pub(crate) fn take_work(&self) -> u8 {
        self.pending_work.swap(0, Ordering::AcqRel)
    }

    pub fn record_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
    }

    pub fn signal_stop(&self) -> windows::core::Result<()> {
        self.signal_stop_with(|| self.stop_event.signal())
    }

    pub(crate) fn signal_stop_with(
        &self,
        signal: impl FnOnce() -> windows::core::Result<()>,
    ) -> windows::core::Result<()> {
        self.record_stop();
        signal()
    }

    pub fn record_callback_failure(&self) {
        self.callback_failed.store(true, Ordering::Release);
        self.record_stop();
    }

    pub fn callback_failed(&self) -> bool {
        self.callback_failed.load(Ordering::Acquire)
    }

    pub fn set_visibility(&self, visibility: DesiredVisibility) {
        self.desired_visibility
            .store(visibility as u8, Ordering::Release);
    }

    pub fn visibility(&self) -> DesiredVisibility {
        match self.desired_visibility.load(Ordering::Acquire) {
            1 => DesiredVisibility::Hidden,
            2 => DesiredVisibility::Visible,
            _ => DesiredVisibility::Unknown,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BorderStopAction {
    pub control: Arc<BorderControl>,
}

impl BorderStopAction {
    pub fn signal(&self) -> windows::core::Result<()> {
        self.control.signal_stop()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BorderWorkerResult {
    Completed,
    InitializationFailed(String),
    RuntimeFailed(String),
    AnimationFailed(String),
    CallbackFailed,
    Panicked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BorderWorkerExit {
    pub primary: BorderWorkerResult,
    pub cleanup_failures: Vec<String>,
}

impl BorderWorkerExit {
    pub fn completed() -> Self {
        Self {
            primary: BorderWorkerResult::Completed,
            cleanup_failures: Vec::new(),
        }
    }

    fn failed(&self) -> bool {
        self.primary != BorderWorkerResult::Completed || !self.cleanup_failures.is_empty()
    }
}

pub fn discard_panic(payload: Box<dyn std::any::Any + Send>) {
    if let Err(secondary) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        std::mem::forget(secondary);
    }
}

#[derive(Debug, Clone)]
pub struct TrackingObservation {
    pub identity: TrackingWindowIdentity,
    pub live: bool,
    pub border_desired: bool,
    pub consecutive_failures: u32,
    pub retry_at: Option<Instant>,
    pub healthy_since: Option<Instant>,
}

#[derive(Debug)]
pub struct BorderEntry {
    pub id: BorderInstanceId,
    pub tracking: TrackingWindowIdentity,
    pub phase: BorderPhase,
    pub border_hwnd: Option<isize>,
    pub worker_thread_id: Option<u32>,
    pub prepared_config_revision: Option<u64>,
    pub started_at: Option<Instant>,
    pub stop: Option<BorderStopRequest>,
    pub control: Arc<BorderControl>,
    pub updates: Arc<BorderUpdateMailbox>,
    pub launch_pending: bool,
    worker: Option<JoinHandle<BorderWorkerExit>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    Closed,
    Busy,
    Exhausted,
    Stale,
    InvalidOwnership,
    RetryNotDue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveResult {
    Reserved(BorderInstanceId),
    AlreadyTracked(BorderInstanceId),
}

#[derive(Debug)]
pub struct UnattachedWorker {
    pub id: BorderInstanceId,
    pub worker: JoinHandle<BorderWorkerExit>,
}

pub struct BaselineWindow {
    pub fingerprint: TrackingWindowFingerprint,
    pub updates: Option<Arc<BorderUpdateMailbox>>,
}

impl UnattachedWorker {
    pub(crate) fn into_joining(self) -> JoiningBorderWorker {
        JoiningBorderWorker {
            id: self.id,
            worker: self.worker,
        }
    }
}

#[derive(Debug)]
pub struct JoiningBorderWorker {
    pub id: BorderInstanceId,
    worker: JoinHandle<BorderWorkerExit>,
}

impl JoiningBorderWorker {
    pub fn join(self) -> JoinedBorderWorker {
        let outcome = match self.worker.join() {
            Ok(outcome) => outcome,
            Err(payload) => {
                discard_panic(payload);
                BorderWorkerExit {
                    primary: BorderWorkerResult::Panicked,
                    cleanup_failures: Vec::new(),
                }
            }
        };
        JoinedBorderWorker {
            id: self.id,
            outcome,
        }
    }
}

#[derive(Debug)]
pub struct JoinedBorderWorker {
    pub id: BorderInstanceId,
    pub outcome: BorderWorkerExit,
}

#[derive(Debug)]
pub struct BorderLaunchSlot {
    pub id: BorderInstanceId,
    pub control: Arc<BorderControl>,
    worker: Option<JoinHandle<BorderWorkerExit>>,
}

impl BorderLaunchSlot {
    pub fn new(id: BorderInstanceId, control: Arc<BorderControl>) -> Self {
        Self {
            id,
            control,
            worker: None,
        }
    }

    pub fn retain(&mut self, worker: JoinHandle<BorderWorkerExit>) -> Result<(), UnattachedWorker> {
        if self.worker.is_some() {
            return Err(UnattachedWorker {
                id: self.id,
                worker,
            });
        }
        self.worker = Some(worker);
        Ok(())
    }

    pub fn attach(&mut self, registry: &mut BorderRegistry) -> Result<(), RegistryError> {
        let worker = self.worker.take().ok_or(RegistryError::InvalidOwnership)?;
        match registry.attach_worker(self.id, worker) {
            Ok(()) => Ok(()),
            Err(unattached) => {
                self.worker = Some(unattached.worker);
                self.control.record_stop();
                registry.recover_launch(self.id)?;
                Err(RegistryError::InvalidOwnership)
            }
        }
    }

    pub fn take_finished(&mut self, registry: &mut BorderRegistry) -> Option<JoiningBorderWorker> {
        if !self.worker.as_ref()?.is_finished() {
            return None;
        }
        let entry = registry.entries.get_mut(&self.id)?;
        if entry.launch_pending
            || entry.worker.is_some()
            || entry.phase != BorderPhase::StopRequested
        {
            return None;
        }
        entry.phase = BorderPhase::Joining;
        Some(JoiningBorderWorker {
            id: self.id,
            worker: self.worker.take()?,
        })
    }

    pub fn owns_worker(&self) -> bool {
        self.worker.is_some()
    }

    pub(crate) fn take_finished_orphan(
        &mut self,
        registry: &BorderRegistry,
    ) -> Option<JoiningBorderWorker> {
        if registry.entries.contains_key(&self.id) || !self.worker.as_ref()?.is_finished() {
            return None;
        }
        Some(JoiningBorderWorker {
            id: self.id,
            worker: self.worker.take()?,
        })
    }
}

#[derive(Debug)]
pub struct BorderRegistry {
    next_id: u64,
    pub admission: BorderAdmission,
    pub quarantined: bool,
    pub shutdown_requested: bool,
    pub active_operation: Option<BorderOperationId>,
    pub initial_observations: HashSet<TrackingObservationId>,
    pub observations: HashMap<TrackingObservationId, TrackingObservation>,
    pub observation_by_hwnd: HashMap<isize, TrackingObservationId>,
    pub current_by_hwnd: HashMap<isize, BorderInstanceId>,
    pub entries: HashMap<BorderInstanceId, BorderEntry>,
}

impl Default for BorderRegistry {
    fn default() -> Self {
        Self {
            next_id: 0,
            admission: BorderAdmission::Paused,
            quarantined: false,
            shutdown_requested: false,
            active_operation: None,
            initial_observations: HashSet::new(),
            observations: HashMap::new(),
            observation_by_hwnd: HashMap::new(),
            current_by_hwnd: HashMap::new(),
            entries: HashMap::new(),
        }
    }
}

impl BorderRegistry {
    fn allocate(&mut self) -> Result<u64, RegistryError> {
        let Some(next) = self.next_id.checked_add(1) else {
            self.quarantine();
            return Err(RegistryError::Exhausted);
        };
        self.next_id = next;
        Ok(next)
    }

    pub fn observe(
        &mut self,
        fingerprint: TrackingWindowFingerprint,
    ) -> Result<TrackingWindowIdentity, RegistryError> {
        if self.quarantined || self.shutdown_requested {
            return Err(RegistryError::Closed);
        }
        if let Some(observation) = self
            .observation_by_hwnd
            .get(&fingerprint.hwnd)
            .and_then(|id| self.observations.get(id))
            && observation.live
            && observation.identity.fingerprint == fingerprint
        {
            return Ok(observation.identity);
        }
        if self.observations.try_reserve(1).is_err()
            || self.observation_by_hwnd.try_reserve(1).is_err()
        {
            self.quarantine();
            return Err(RegistryError::Exhausted);
        }
        let observation = TrackingObservationId(self.allocate()?);
        self.retire_observation(fingerprint.hwnd, None, Instant::now());
        let identity = TrackingWindowIdentity {
            fingerprint,
            observation,
        };
        self.observations.insert(
            observation,
            TrackingObservation {
                identity,
                live: true,
                border_desired: true,
                consecutive_failures: 0,
                retry_at: None,
                healthy_since: None,
            },
        );
        self.observation_by_hwnd
            .insert(fingerprint.hwnd, observation);
        Ok(identity)
    }

    pub fn retire_observation(
        &mut self,
        hwnd: isize,
        event_thread: Option<u32>,
        now: Instant,
    ) -> Vec<BorderStopAction> {
        let Some(observation_id) = self.observation_by_hwnd.get(&hwnd).copied() else {
            return Vec::new();
        };
        let Some(observation) = self.observations.get_mut(&observation_id) else {
            self.quarantine();
            return Vec::new();
        };
        if event_thread.is_some_and(|thread| {
            thread != 0 && thread != observation.identity.fingerprint.thread_id
        }) {
            return Vec::new();
        }
        observation.live = false;
        observation.border_desired = false;
        self.observation_by_hwnd.remove(&hwnd);
        let ids: Vec<_> = self
            .entries
            .values()
            .filter(|entry| entry.tracking.observation == observation_id)
            .map(|entry| entry.id)
            .collect();
        if ids.is_empty() {
            self.observations.remove(&observation_id);
            self.initial_observations.remove(&observation_id);
        }
        ids.into_iter()
            .filter_map(|id| self.request_stop(id, BorderStopReason::Destroyed, now))
            .collect()
    }

    pub fn reserve(
        &mut self,
        identity: TrackingWindowIdentity,
        updates: Arc<BorderUpdateMailbox>,
        now: Instant,
    ) -> Result<ReserveResult, RegistryError> {
        if self.admission != BorderAdmission::Open || self.quarantined || self.shutdown_requested {
            return Err(RegistryError::Closed);
        }
        let observation = self
            .observations
            .get(&identity.observation)
            .ok_or(RegistryError::Stale)?;
        if !observation.live
            || !observation.border_desired
            || observation.identity != identity
            || self.observation_by_hwnd.get(&identity.fingerprint.hwnd)
                != Some(&identity.observation)
        {
            return Err(RegistryError::Stale);
        }
        if let Some(entry) = self
            .entries
            .values()
            .find(|entry| entry.tracking == identity)
        {
            return Ok(ReserveResult::AlreadyTracked(entry.id));
        }
        if observation.retry_at.is_some_and(|deadline| now < deadline) {
            return Err(RegistryError::RetryNotDue);
        }
        if self.entries.try_reserve(1).is_err() || self.current_by_hwnd.try_reserve(1).is_err() {
            self.quarantine();
            return Err(RegistryError::Exhausted);
        }
        let id = BorderInstanceId(self.allocate()?);
        if let Some(old) = self
            .current_by_hwnd
            .get(&identity.fingerprint.hwnd)
            .copied()
        {
            self.request_stop(old, BorderStopReason::Replaced, now);
        }
        self.entries.insert(
            id,
            BorderEntry {
                id,
                tracking: identity,
                phase: BorderPhase::Reserved,
                border_hwnd: None,
                worker_thread_id: None,
                prepared_config_revision: None,
                started_at: None,
                stop: None,
                control: updates.control().clone(),
                updates,
                launch_pending: true,
                worker: None,
            },
        );
        self.current_by_hwnd.insert(identity.fingerprint.hwnd, id);
        Ok(ReserveResult::Reserved(id))
    }

    pub fn start(&mut self, id: BorderInstanceId, now: Instant) -> Result<(), RegistryError> {
        let entry = self.entries.get_mut(&id).ok_or(RegistryError::Stale)?;
        if entry.phase != BorderPhase::Reserved
            || !entry.launch_pending
            || entry.control.is_stopped()
            || self.admission != BorderAdmission::Open
            || self.quarantined
            || self.shutdown_requested
            || self.current_by_hwnd.get(&entry.tracking.fingerprint.hwnd) != Some(&id)
        {
            return Err(RegistryError::Closed);
        }
        entry.phase = BorderPhase::Starting;
        entry.started_at = Some(now);
        Ok(())
    }

    pub fn attach_worker(
        &mut self,
        id: BorderInstanceId,
        worker: JoinHandle<BorderWorkerExit>,
    ) -> Result<(), UnattachedWorker> {
        let Some(entry) = self.entries.get_mut(&id) else {
            return Err(UnattachedWorker { id, worker });
        };
        if entry.worker.is_some()
            || !entry.launch_pending
            || !matches!(
                entry.phase,
                BorderPhase::Starting | BorderPhase::Running | BorderPhase::StopRequested
            )
        {
            return Err(UnattachedWorker { id, worker });
        }
        entry.worker = Some(worker);
        entry.launch_pending = false;
        Ok(())
    }

    pub fn publish_running(
        &mut self,
        id: BorderInstanceId,
        hwnd: isize,
        thread_id: u32,
        revision: u64,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(&id) else {
            return false;
        };
        if entry.phase != BorderPhase::Starting
            || entry.control.is_stopped()
            || self.quarantined
            || self.shutdown_requested
            || self.admission != BorderAdmission::Open
            || self.current_by_hwnd.get(&entry.tracking.fingerprint.hwnd) != Some(&id)
            || self
                .observation_by_hwnd
                .get(&entry.tracking.fingerprint.hwnd)
                != Some(&entry.tracking.observation)
        {
            return false;
        }
        entry.phase = BorderPhase::Running;
        entry.border_hwnd = Some(hwnd);
        entry.worker_thread_id = Some(thread_id);
        entry.prepared_config_revision = Some(revision);
        if let Some(observation) = self.observations.get_mut(&entry.tracking.observation) {
            observation.healthy_since = Some(Instant::now());
        }
        true
    }

    pub fn request_stop(
        &mut self,
        id: BorderInstanceId,
        reason: BorderStopReason,
        now: Instant,
    ) -> Option<BorderStopAction> {
        let entry = self.entries.get_mut(&id)?;
        entry.control.record_stop();
        entry.stop.get_or_insert(BorderStopRequest {
            reason,
            requested_at: now,
        });
        if entry.phase != BorderPhase::Joining {
            entry.phase = BorderPhase::StopRequested;
        }
        Some(BorderStopAction {
            control: entry.control.clone(),
        })
    }

    pub fn recover_launch(&mut self, id: BorderInstanceId) -> Result<(), RegistryError> {
        self.request_stop(id, BorderStopReason::Failed, Instant::now())
            .ok_or(RegistryError::Stale)?;
        let entry = self.entries.get_mut(&id).ok_or(RegistryError::Stale)?;
        if entry.worker.is_some() {
            return Err(RegistryError::InvalidOwnership);
        }
        entry.launch_pending = false;
        Ok(())
    }

    pub fn settle_no_worker(
        &mut self,
        id: BorderInstanceId,
        failed: bool,
        now: Instant,
    ) -> Result<(), RegistryError> {
        let entry = self.entries.get(&id).ok_or(RegistryError::Stale)?;
        if !entry.launch_pending
            || entry.worker.is_some()
            || !entry.updates.is_closed()
            || !matches!(
                entry.phase,
                BorderPhase::Reserved | BorderPhase::Starting | BorderPhase::StopRequested
            )
        {
            return Err(RegistryError::InvalidOwnership);
        }
        self.record_outcome(id, failed, now);
        self.remove_exact(id);
        Ok(())
    }

    pub fn take_finished_workers(&mut self, limit: usize) -> Vec<JoiningBorderWorker> {
        let mut finished = Vec::with_capacity(limit.min(self.entries.len()));
        for entry in self.entries.values_mut() {
            if finished.len() == limit {
                break;
            }
            if !entry.launch_pending
                && entry
                    .worker
                    .as_ref()
                    .is_some_and(|worker| worker.is_finished())
            {
                entry.phase = BorderPhase::Joining;
                if let Some(worker) = entry.worker.take() {
                    finished.push(JoiningBorderWorker {
                        id: entry.id,
                        worker,
                    });
                }
            }
        }
        finished
    }

    pub fn finalize_join(
        &mut self,
        joined: JoinedBorderWorker,
        now: Instant,
    ) -> Result<BorderWorkerExit, JoinedBorderWorker> {
        if !self.entries.get(&joined.id).is_some_and(|entry| {
            entry.phase == BorderPhase::Joining
                && !entry.launch_pending
                && entry.worker.is_none()
                && entry.updates.is_closed()
        }) {
            return Err(joined);
        }
        self.record_outcome(joined.id, joined.outcome.failed(), now);
        self.remove_exact(joined.id);
        Ok(joined.outcome)
    }

    fn record_outcome(&mut self, id: BorderInstanceId, failed: bool, now: Instant) {
        let Some(entry) = self.entries.get(&id) else {
            return;
        };
        let Some(observation) = self.observations.get_mut(&entry.tracking.observation) else {
            return;
        };
        if !observation.live
            || self
                .observation_by_hwnd
                .get(&entry.tracking.fingerprint.hwnd)
                != Some(&entry.tracking.observation)
        {
            return;
        }
        let planned = entry.stop.is_some_and(|stop| {
            matches!(
                stop.reason,
                BorderStopReason::Reload | BorderStopReason::Shutdown | BorderStopReason::Explicit
            )
        });
        if !planned && (failed || entry.stop.is_none()) {
            observation.consecutive_failures = observation.consecutive_failures.saturating_add(1);
            let exponent = observation.consecutive_failures.saturating_sub(1).min(7);
            let delay = Duration::from_millis((250u64 << exponent).min(30_000));
            observation.retry_at = now.checked_add(delay);
        }
        observation.healthy_since = None;
    }

    fn remove_exact(&mut self, id: BorderInstanceId) {
        if let Some(entry) = self.entries.remove(&id) {
            let hwnd = entry.tracking.fingerprint.hwnd;
            if self.current_by_hwnd.get(&hwnd) == Some(&id) {
                self.current_by_hwnd.remove(&hwnd);
            }
            if self
                .observations
                .get(&entry.tracking.observation)
                .is_some_and(|observation| !observation.live)
                && !self
                    .entries
                    .values()
                    .any(|other| other.tracking.observation == entry.tracking.observation)
            {
                self.observations.remove(&entry.tracking.observation);
                self.initial_observations
                    .remove(&entry.tracking.observation);
            }
        }
    }

    pub fn reset_healthy_history(&mut self, id: BorderInstanceId, now: Instant) {
        let Some(entry) = self.entries.get(&id) else {
            return;
        };
        if entry.phase != BorderPhase::Running
            || entry.control.is_stopped()
            || entry.worker.as_ref().is_some_and(JoinHandle::is_finished)
            || self.current_by_hwnd.get(&entry.tracking.fingerprint.hwnd) != Some(&id)
            || self
                .observation_by_hwnd
                .get(&entry.tracking.fingerprint.hwnd)
                != Some(&entry.tracking.observation)
        {
            return;
        }
        let Some(observation) = self.observations.get_mut(&entry.tracking.observation) else {
            return;
        };
        if observation.live
            && observation.healthy_since.is_some_and(|start| {
                now.saturating_duration_since(start) >= Duration::from_secs(30)
            })
        {
            observation.consecutive_failures = 0;
            observation.retry_at = None;
        }
    }

    pub fn begin_operation(&mut self, shutdown: bool) -> Result<BorderOperationId, RegistryError> {
        if shutdown {
            self.shutdown_requested = true;
            self.admission = BorderAdmission::ShuttingDown;
        }
        if self.active_operation.is_some() {
            return Err(RegistryError::Busy);
        }
        if self.quarantined || (!shutdown && self.shutdown_requested) {
            return Err(RegistryError::Closed);
        }
        let id = BorderOperationId(self.allocate()?);
        self.active_operation = Some(id);
        if !shutdown {
            self.admission = BorderAdmission::Paused;
        }
        Ok(id)
    }

    pub fn finish_operation(
        &mut self,
        id: BorderOperationId,
        reopen: bool,
    ) -> Result<(), RegistryError> {
        if self.active_operation != Some(id) {
            return Err(RegistryError::Stale);
        }
        if reopen && (!self.entries.is_empty() || self.quarantined || self.shutdown_requested) {
            return Err(RegistryError::Closed);
        }
        self.active_operation = None;
        if reopen {
            self.admission = BorderAdmission::Open;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn admit_baseline(
        &mut self,
        operation: BorderOperationId,
        windows: Vec<BaselineWindow>,
    ) -> Result<Vec<BorderInstanceId>, RegistryError> {
        self.admit_baseline_unless_shutdown(operation, windows, None, None)
    }

    pub(crate) fn admit_baseline_unless_shutdown(
        &mut self,
        operation: BorderOperationId,
        windows: Vec<BaselineWindow>,
        deadline: Option<Instant>,
        submission: Option<&AtomicBool>,
    ) -> Result<Vec<BorderInstanceId>, RegistryError> {
        match self.build_baseline(operation, windows) {
            Ok((replacement, admitted)) => {
                if submission.is_some_and(|submission| submission.load(Ordering::Acquire)) {
                    self.shutdown_requested = true;
                    self.admission = BorderAdmission::ShuttingDown;
                    return Err(RegistryError::Closed);
                }
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return Err(RegistryError::Closed);
                }
                *self = replacement;
                Ok(admitted)
            }
            Err(RegistryError::Exhausted) => {
                self.quarantine();
                Err(RegistryError::Exhausted)
            }
            Err(error) => Err(error),
        }
    }

    fn build_baseline(
        &self,
        operation: BorderOperationId,
        windows: Vec<BaselineWindow>,
    ) -> Result<(Self, Vec<BorderInstanceId>), RegistryError> {
        if self.active_operation != Some(operation) {
            return Err(RegistryError::Stale);
        }
        if self.quarantined || self.shutdown_requested || !self.entries.is_empty() {
            return Err(RegistryError::Closed);
        }
        let mut replacement = Self {
            next_id: self.next_id,
            admission: BorderAdmission::Open,
            quarantined: false,
            shutdown_requested: false,
            active_operation: Some(operation),
            initial_observations: HashSet::new(),
            observations: HashMap::new(),
            observation_by_hwnd: HashMap::new(),
            current_by_hwnd: HashMap::new(),
            entries: HashMap::new(),
        };
        let capacity = self
            .observations
            .len()
            .checked_add(windows.len())
            .ok_or(RegistryError::Exhausted)?;
        replacement
            .observations
            .try_reserve(capacity)
            .map_err(|_| RegistryError::Exhausted)?;
        replacement
            .observation_by_hwnd
            .try_reserve(capacity)
            .map_err(|_| RegistryError::Exhausted)?;
        replacement
            .initial_observations
            .try_reserve(windows.len())
            .map_err(|_| RegistryError::Exhausted)?;
        replacement
            .current_by_hwnd
            .try_reserve(windows.len())
            .map_err(|_| RegistryError::Exhausted)?;
        replacement
            .entries
            .try_reserve(windows.len())
            .map_err(|_| RegistryError::Exhausted)?;
        let mut admitted = Vec::new();
        admitted
            .try_reserve(windows.len())
            .map_err(|_| RegistryError::Exhausted)?;
        replacement.observations.extend(
            self.observations
                .iter()
                .map(|(id, observation)| (*id, observation.clone())),
        );
        replacement.observation_by_hwnd.extend(
            self.observation_by_hwnd
                .iter()
                .map(|(hwnd, id)| (*hwnd, *id)),
        );
        let mut seen = HashSet::new();
        seen.try_reserve(windows.len())
            .map_err(|_| RegistryError::Exhausted)?;
        for window in windows {
            if !seen.insert(window.fingerprint.hwnd) {
                return Err(RegistryError::InvalidOwnership);
            }
            let identity = replacement.observe(window.fingerprint)?;
            replacement
                .initial_observations
                .insert(identity.observation);
            let observation = replacement
                .observations
                .get_mut(&identity.observation)
                .ok_or(RegistryError::Stale)?;
            observation.border_desired = window.updates.is_some();
            if let Some(updates) = window.updates {
                match replacement.reserve(identity, updates, Instant::now()) {
                    Ok(ReserveResult::Reserved(id)) => admitted.push(id),
                    Err(RegistryError::RetryNotDue) => {}
                    _ => return Err(RegistryError::InvalidOwnership),
                }
            }
        }
        replacement.finish_operation(operation, false)?;
        Ok((replacement, admitted))
    }

    pub fn quarantine(&mut self) {
        self.quarantined = true;
        if !self.shutdown_requested {
            self.admission = BorderAdmission::Paused;
        }
        self.observation_by_hwnd.retain(|hwnd, id| {
            self.observations.get(id).is_some_and(|observation| {
                observation.live
                    && observation.identity.observation == *id
                    && observation.identity.fingerprint.hwnd == *hwnd
            })
        });
        self.current_by_hwnd.retain(|hwnd, id| {
            self.entries.get(id).is_some_and(|entry| {
                entry.id == *id
                    && entry.tracking.fingerprint.hwnd == *hwnd
                    && self.observation_by_hwnd.get(hwnd) == Some(&entry.tracking.observation)
            })
        });
        self.initial_observations
            .retain(|id| self.observations.contains_key(id));
        for entry in self.entries.values_mut() {
            if entry.worker.is_some() {
                entry.launch_pending = false;
            }
            entry.control.record_stop();
            entry.stop.get_or_insert(BorderStopRequest {
                reason: BorderStopReason::Failed,
                requested_at: Instant::now(),
            });
            if entry.phase != BorderPhase::Joining {
                entry.phase = BorderPhase::StopRequested;
            }
        }
    }

    pub fn lock_recover(registry: &Mutex<Self>) -> MutexGuard<'_, Self> {
        match registry.lock() {
            Ok(guard) => guard,
            Err(poison) => {
                let mut guard = poison.into_inner();
                guard.quarantine();
                guard
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    fn fixture() -> (BorderRegistry, TrackingWindowIdentity, Arc<BorderControl>) {
        let mut registry = BorderRegistry {
            admission: BorderAdmission::Open,
            ..Default::default()
        };
        let identity = registry
            .observe(TrackingWindowFingerprint {
                hwnd: 1,
                process_id: 2,
                thread_id: 3,
            })
            .unwrap();
        (registry, identity, Arc::new(BorderControl::new().unwrap()))
    }

    fn reserve(
        registry: &mut BorderRegistry,
        identity: TrackingWindowIdentity,
        control: Arc<BorderControl>,
    ) -> BorderInstanceId {
        match registry
            .reserve(identity, mailbox(control), Instant::now())
            .unwrap()
        {
            ReserveResult::Reserved(id) => id,
            ReserveResult::AlreadyTracked(_) => panic!("expected a fresh reservation"),
        }
    }

    fn mailbox(control: Arc<BorderControl>) -> Arc<BorderUpdateMailbox> {
        Arc::new(BorderUpdateMailbox::new(control).unwrap())
    }

    fn finalize(registry: &mut BorderRegistry, joined: JoinedBorderWorker) {
        assert!(matches!(
            registry.entries[&joined.id].updates.try_close(),
            crate::border_updates::MailboxClosure::Closed { .. }
        ));
        registry.finalize_join(joined, Instant::now()).unwrap();
    }

    fn settle(registry: &mut BorderRegistry, id: BorderInstanceId, failed: bool, now: Instant) {
        assert!(matches!(
            registry.entries[&id].updates.try_close(),
            crate::border_updates::MailboxClosure::Closed { .. }
        ));
        registry.settle_no_worker(id, failed, now).unwrap();
    }

    fn finished() -> JoinHandle<BorderWorkerExit> {
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            sender.send(()).unwrap();
            BorderWorkerExit::completed()
        });
        receiver.recv().unwrap();
        while !worker.is_finished() {
            thread::yield_now();
        }
        worker
    }

    #[test]
    fn expired_baseline_deadline_cannot_commit_bookkeeping_or_admission() {
        let (mut registry, identity, _) = fixture();
        let operation = registry.begin_operation(false).unwrap();
        assert_eq!(
            registry.admit_baseline_unless_shutdown(
                operation,
                vec![BaselineWindow {
                    fingerprint: identity.fingerprint,
                    updates: Some(mailbox(Arc::new(BorderControl::new().unwrap())))
                }],
                Some(Instant::now()),
                None
            ),
            Err(RegistryError::Closed)
        );
        assert!(registry.entries.is_empty());
        assert!(registry.initial_observations.is_empty());
        assert_eq!(registry.admission, BorderAdmission::Paused);
        assert_eq!(registry.active_operation, Some(operation));
    }

    #[test]
    fn baseline_batch_reopens_and_reserves_atomically() {
        let (mut registry, identity, _) = fixture();
        let operation = registry.begin_operation(false).unwrap();
        let admitted = registry
            .admit_baseline(
                operation,
                vec![BaselineWindow {
                    fingerprint: identity.fingerprint,
                    updates: Some(mailbox(Arc::new(BorderControl::new().unwrap()))),
                }],
            )
            .unwrap();
        assert_eq!(admitted.len(), 1);
        assert_eq!(registry.admission, BorderAdmission::Open);
        assert!(registry.active_operation.is_none());
        assert!(
            registry
                .initial_observations
                .contains(&identity.observation)
        );
        assert_eq!(registry.entries[&admitted[0]].phase, BorderPhase::Reserved);
        settle(&mut registry, admitted[0], false, Instant::now());
    }

    #[test]
    fn failed_baseline_batch_has_no_partial_bookkeeping_or_reservations() {
        let (mut registry, identity, _) = fixture();
        let operation = registry.begin_operation(false).unwrap();
        let windows = (0..2)
            .map(|_| BaselineWindow {
                fingerprint: identity.fingerprint,
                updates: Some(mailbox(Arc::new(BorderControl::new().unwrap()))),
            })
            .collect();
        assert_eq!(
            registry.admit_baseline(operation, windows),
            Err(RegistryError::InvalidOwnership)
        );
        assert!(registry.entries.is_empty());
        assert!(registry.initial_observations.is_empty());
        assert_eq!(registry.admission, BorderAdmission::Paused);
        assert_eq!(registry.active_operation, Some(operation));
        assert_eq!(registry.observations.len(), 1);
    }

    #[test]
    fn shutdown_wins_final_baseline_commit_without_any_new_generation() {
        let (mut registry, identity, _) = fixture();
        let operation = registry.begin_operation(false).unwrap();
        assert_eq!(registry.begin_operation(true), Err(RegistryError::Busy));
        assert_eq!(
            registry.admit_baseline(
                operation,
                vec![BaselineWindow {
                    fingerprint: identity.fingerprint,
                    updates: Some(mailbox(Arc::new(BorderControl::new().unwrap())))
                }]
            ),
            Err(RegistryError::Closed)
        );
        assert_eq!(registry.admission, BorderAdmission::ShuttingDown);
        assert!(registry.entries.is_empty());
        assert!(registry.initial_observations.is_empty());
    }

    #[test]
    fn poison_repairs_indexes_without_losing_attached_handle() {
        let (mut registry, identity, control) = fixture();
        let id = reserve(&mut registry, identity, control.clone());
        registry.start(id, Instant::now()).unwrap();
        registry.attach_worker(id, finished()).unwrap();
        let registry = Mutex::new(registry);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = registry.lock().unwrap();
            guard.entries.get_mut(&id).unwrap().launch_pending = true;
            guard.observation_by_hwnd.insert(999, identity.observation);
            guard.current_by_hwnd.insert(999, id);
            guard
                .initial_observations
                .insert(TrackingObservationId(999));
            panic!("injected paired-index interruption");
        }));
        let mut recovered = BorderRegistry::lock_recover(&registry);
        assert!(recovered.quarantined);
        assert!(control.is_stopped());
        assert!(!recovered.observation_by_hwnd.contains_key(&999));
        assert!(!recovered.current_by_hwnd.contains_key(&999));
        assert!(
            !recovered
                .initial_observations
                .contains(&TrackingObservationId(999))
        );
        assert_eq!(
            recovered.current_by_hwnd.get(&identity.fingerprint.hwnd),
            Some(&id)
        );
        assert!(!recovered.entries[&id].launch_pending);
        assert!(matches!(
            recovered.observe(identity.fingerprint),
            Err(RegistryError::Closed)
        ));
        let joined = recovered.take_finished_workers(1).pop().unwrap().join();
        finalize(&mut recovered, joined);
        assert!(recovered.entries.is_empty());
    }

    #[test]
    fn ownership_table_rejects_every_other_combination() {
        let allowed = [
            (BorderPhase::Reserved, true, BorderHandleOwner::None),
            (BorderPhase::Starting, true, BorderHandleOwner::Launch),
            (BorderPhase::Starting, false, BorderHandleOwner::Entry),
            (BorderPhase::Running, true, BorderHandleOwner::Launch),
            (BorderPhase::Running, false, BorderHandleOwner::Entry),
            (BorderPhase::StopRequested, true, BorderHandleOwner::None),
            (BorderPhase::StopRequested, true, BorderHandleOwner::Launch),
            (BorderPhase::StopRequested, false, BorderHandleOwner::Entry),
            (
                BorderPhase::StopRequested,
                false,
                BorderHandleOwner::Recovery,
            ),
            (BorderPhase::Joining, false, BorderHandleOwner::JoinTicket),
        ];
        for phase in [
            BorderPhase::Reserved,
            BorderPhase::Starting,
            BorderPhase::Running,
            BorderPhase::StopRequested,
            BorderPhase::Joining,
        ] {
            for pending in [false, true] {
                for owner in [
                    BorderHandleOwner::None,
                    BorderHandleOwner::Launch,
                    BorderHandleOwner::Entry,
                    BorderHandleOwner::Recovery,
                    BorderHandleOwner::JoinTicket,
                ] {
                    assert_eq!(
                        phase.permits_owner(pending, owner),
                        allowed.contains(&(phase, pending, owner)),
                        "{phase:?}/{pending}/{owner:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn live_worker_is_not_reaped_and_stop_prevents_publication() {
        let (mut registry, identity, control) = fixture();
        let id = reserve(&mut registry, identity, control);
        registry.start(id, Instant::now()).unwrap();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            receiver.recv().unwrap();
            BorderWorkerExit::completed()
        });
        registry.attach_worker(id, worker).unwrap();
        assert!(registry.take_finished_workers(64).is_empty());
        registry.request_stop(id, BorderStopReason::Explicit, Instant::now());
        assert!(!registry.publish_running(id, 4, 5, 6));
        sender.send(()).unwrap();
        while !registry.entries[&id].worker.as_ref().unwrap().is_finished() {
            thread::yield_now();
        }
        let ticket = registry.take_finished_workers(1).pop().unwrap();
        finalize(&mut registry, ticket.join());
        assert_eq!(
            registry.observations[&identity.observation].consecutive_failures,
            0
        );
    }

    #[test]
    fn reservation_and_stop_before_spawn() {
        let (mut registry, identity, control) = fixture();
        let id = reserve(&mut registry, identity, control.clone());
        assert_eq!(
            registry.reserve(identity, mailbox(control.clone()), Instant::now()),
            Ok(ReserveResult::AlreadyTracked(id))
        );
        registry.request_stop(id, BorderStopReason::Destroyed, Instant::now());
        registry.request_stop(id, BorderStopReason::Reload, Instant::now());
        assert!(control.is_stopped());
        assert_eq!(
            registry.entries[&id].stop.unwrap().reason,
            BorderStopReason::Destroyed
        );
        assert!(registry.start(id, Instant::now()).is_err());
        settle(&mut registry, id, false, Instant::now());
        assert!(registry.entries.is_empty());
        assert!(registry.observations[&identity.observation].live);
    }

    #[test]
    fn fast_publication_and_completion_before_attachment() {
        let (mut registry, identity, control) = fixture();
        let id = reserve(&mut registry, identity, control.clone());
        registry.start(id, Instant::now()).unwrap();
        let mut slot = BorderLaunchSlot::new(id, control);
        slot.retain(finished()).unwrap();
        assert!(registry.publish_running(id, 4, 5, 6));
        assert!(registry.entries[&id].launch_pending);
        slot.attach(&mut registry).unwrap();
        let ticket = registry.take_finished_workers(1).pop().unwrap();
        assert_eq!(registry.entries[&id].phase, BorderPhase::Joining);
        assert_eq!(
            registry.reserve(
                identity,
                mailbox(Arc::new(BorderControl::new().unwrap())),
                Instant::now()
            ),
            Ok(ReserveResult::AlreadyTracked(id))
        );
        finalize(&mut registry, ticket.join());
        assert!(registry.entries.is_empty());
        assert_eq!(
            registry.observations[&identity.observation].consecutive_failures,
            1
        );
    }

    #[test]
    fn old_join_cannot_remove_new_generation() {
        let (mut registry, identity, control) = fixture();
        let old = reserve(&mut registry, identity, control);
        registry.start(old, Instant::now()).unwrap();
        registry.attach_worker(old, finished()).unwrap();
        registry.retire_observation(1, Some(3), Instant::now());
        let replacement = registry.observe(identity.fingerprint).unwrap();
        assert_ne!(identity.observation, replacement.observation);
        let new = reserve(
            &mut registry,
            replacement,
            Arc::new(BorderControl::new().unwrap()),
        );
        let ticket = registry.take_finished_workers(1).pop().unwrap();
        finalize(&mut registry, ticket.join());
        assert_eq!(registry.current_by_hwnd[&1], new);
        assert_eq!(
            registry.observations[&replacement.observation].consecutive_failures,
            0
        );
        assert!(!registry.publish_running(old, 4, 5, 6));
        registry.request_stop(new, BorderStopReason::Explicit, Instant::now());
        settle(&mut registry, new, false, Instant::now());
    }

    #[test]
    fn delayed_destroy_thread_mismatch_is_ignored() {
        let (mut registry, identity, control) = fixture();
        let id = reserve(&mut registry, identity, control.clone());
        assert!(
            registry
                .retire_observation(1, Some(99), Instant::now())
                .is_empty()
        );
        assert!(!control.is_stopped());
        assert!(registry.observations[&identity.observation].live);
        settle(&mut registry, id, false, Instant::now());
    }

    #[test]
    fn shutdown_overrides_reload_and_quarantine() {
        let (mut registry, _, _) = fixture();
        let reload = registry.begin_operation(false).unwrap();
        assert_eq!(registry.begin_operation(false), Err(RegistryError::Busy));
        assert_eq!(registry.begin_operation(true), Err(RegistryError::Busy));
        registry.quarantine();
        assert_eq!(registry.admission, BorderAdmission::ShuttingDown);
        assert_eq!(
            registry.finish_operation(reload, true),
            Err(RegistryError::Closed)
        );
        registry.finish_operation(reload, false).unwrap();
        assert_eq!(registry.begin_operation(false), Err(RegistryError::Closed));
    }

    #[test]
    fn exhaustion_quarantines_without_aliasing() {
        let (mut registry, identity, control) = fixture();
        registry.next_id = u64::MAX;
        assert_eq!(
            registry.reserve(identity, mailbox(control), Instant::now()),
            Err(RegistryError::Exhausted)
        );
        assert!(registry.entries.is_empty());
        assert!(registry.quarantined);
    }

    #[test]
    fn poison_retains_worker_for_join() {
        let (mut registry, identity, control) = fixture();
        let id = reserve(&mut registry, identity, control.clone());
        registry.start(id, Instant::now()).unwrap();
        registry.attach_worker(id, finished()).unwrap();
        let shared = Arc::new(Mutex::new(registry));
        let poisoned = shared.clone();
        assert!(
            thread::spawn(move || {
                let _guard = poisoned.lock().unwrap();
                panic!("injected poison");
            })
            .join()
            .is_err()
        );
        let mut registry = BorderRegistry::lock_recover(&shared);
        assert!(registry.quarantined);
        assert!(control.is_stopped());
        let ticket = registry.take_finished_workers(1).pop().unwrap();
        drop(registry);
        let joined = ticket.join();
        let updates = BorderRegistry::lock_recover(&shared).entries[&id]
            .updates
            .clone();
        assert!(matches!(
            updates.try_close(),
            crate::border_updates::MailboxClosure::Closed { .. }
        ));
        BorderRegistry::lock_recover(&shared)
            .finalize_join(joined, Instant::now())
            .unwrap();
        assert!(BorderRegistry::lock_recover(&shared).entries.is_empty());
    }

    #[test]
    fn attachment_failure_retains_recovery_slot() {
        let (mut registry, identity, control) = fixture();
        let id = reserve(&mut registry, identity, control.clone());
        let mut slot = BorderLaunchSlot::new(id, control.clone());
        slot.retain(finished()).unwrap();
        assert_eq!(
            slot.attach(&mut registry),
            Err(RegistryError::InvalidOwnership)
        );
        assert!(slot.owns_worker());
        assert!(control.is_stopped());
        assert!(!registry.entries[&id].launch_pending);
        let ticket = slot.take_finished(&mut registry).unwrap();
        finalize(&mut registry, ticket.join());
        assert!(registry.entries.is_empty());
    }

    #[test]
    fn retry_history_survives_worker_removal() {
        let (mut registry, identity, control) = fixture();
        let now = Instant::now();
        let id = reserve(&mut registry, identity, control);
        registry.start(id, now).unwrap();
        settle(&mut registry, id, true, now);
        assert_eq!(
            registry.observations[&identity.observation].consecutive_failures,
            1
        );
        assert_eq!(
            registry.reserve(
                identity,
                mailbox(Arc::new(BorderControl::new().unwrap())),
                now
            ),
            Err(RegistryError::RetryNotDue)
        );
        let later = now + Duration::from_secs(1);
        let control = Arc::new(BorderControl::new().unwrap());
        let id = match registry.reserve(identity, mailbox(control), later).unwrap() {
            ReserveResult::Reserved(id) => id,
            _ => panic!(),
        };
        registry.start(id, later).unwrap();
        settle(&mut registry, id, true, later);
        assert_eq!(
            registry.observations[&identity.observation].consecutive_failures,
            2
        );
    }

    #[test]
    fn retry_backoff_increases_caps_and_survives_fresh_publication() {
        let (mut registry, identity, _) = fixture();
        let mut now = Instant::now();
        for (index, delay_ms) in [
            250, 500, 1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000,
        ]
        .into_iter()
        .enumerate()
        {
            let id = match registry
                .reserve(
                    identity,
                    mailbox(Arc::new(BorderControl::new().unwrap())),
                    now,
                )
                .unwrap()
            {
                ReserveResult::Reserved(id) => id,
                _ => panic!("retry did not reserve a fresh instance"),
            };
            registry.start(id, now).unwrap();
            assert!(registry.publish_running(id, 100, 1, 1));
            registry
                .observations
                .get_mut(&identity.observation)
                .unwrap()
                .healthy_since = Some(now);
            registry.reset_healthy_history(id, now);
            assert_eq!(
                registry.observations[&identity.observation].consecutive_failures,
                index as u32
            );
            registry
                .request_stop(id, BorderStopReason::Failed, now)
                .unwrap();
            settle(&mut registry, id, true, now);
            let deadline = now + Duration::from_millis(delay_ms);
            assert_eq!(
                registry.observations[&identity.observation].retry_at,
                Some(deadline)
            );
            assert_eq!(
                registry.observations[&identity.observation].consecutive_failures,
                index as u32 + 1
            );
            assert!(registry.entries.is_empty());
            assert_eq!(
                registry.reserve(
                    identity,
                    mailbox(Arc::new(BorderControl::new().unwrap())),
                    deadline - Duration::from_nanos(1)
                ),
                Err(RegistryError::RetryNotDue)
            );
            now = deadline;
        }
    }

    #[test]
    fn retry_admission_stays_closed_when_ineligible_paused_quarantined_or_shutdown() {
        for mode in 0..4 {
            let (mut registry, identity, control) = fixture();
            let now = Instant::now();
            let id = reserve(&mut registry, identity, control);
            registry.start(id, now).unwrap();
            settle(&mut registry, id, true, now);
            let due = registry.observations[&identity.observation]
                .retry_at
                .unwrap();
            let expected = match mode {
                0 => {
                    registry
                        .observations
                        .get_mut(&identity.observation)
                        .unwrap()
                        .border_desired = false;
                    RegistryError::Stale
                }
                1 => {
                    registry.admission = BorderAdmission::Paused;
                    RegistryError::Closed
                }
                2 => {
                    registry.quarantine();
                    RegistryError::Closed
                }
                _ => {
                    registry.begin_operation(true).unwrap();
                    RegistryError::Closed
                }
            };
            assert_eq!(
                registry.reserve(
                    identity,
                    mailbox(Arc::new(BorderControl::new().unwrap())),
                    due
                ),
                Err(expected)
            );
            assert!(registry.entries.is_empty());
            assert_eq!(
                registry.observations[&identity.observation].consecutive_failures,
                1
            );
        }
    }

    #[test]
    fn healthy_history_resets_only_after_running_threshold() {
        let (mut registry, identity, control) = fixture();
        let now = Instant::now();
        let id = reserve(&mut registry, identity, control);
        registry.start(id, now).unwrap();
        assert!(registry.publish_running(id, 100, 1, 1));
        let observation = registry
            .observations
            .get_mut(&identity.observation)
            .unwrap();
        observation.healthy_since = Some(now);
        observation.consecutive_failures = 3;
        observation.retry_at = Some(now + Duration::from_secs(60));
        registry.reset_healthy_history(id, now + Duration::from_secs(29));
        assert_eq!(
            registry.observations[&identity.observation].consecutive_failures,
            3
        );
        registry.reset_healthy_history(id, now + Duration::from_secs(30));
        assert_eq!(
            registry.observations[&identity.observation].consecutive_failures,
            0
        );
        assert!(
            registry.observations[&identity.observation]
                .retry_at
                .is_none()
        );
    }

    #[test]
    fn healthy_history_does_not_reset_stopped_worker() {
        let (mut registry, identity, control) = fixture();
        let now = Instant::now();
        let id = reserve(&mut registry, identity, control.clone());
        registry.start(id, now).unwrap();
        assert!(registry.publish_running(id, 100, 1, 1));
        let observation = registry
            .observations
            .get_mut(&identity.observation)
            .unwrap();
        observation.healthy_since = Some(now);
        observation.consecutive_failures = 3;
        control.record_stop();
        registry.reset_healthy_history(id, now + Duration::from_secs(30));
        assert_eq!(
            registry.observations[&identity.observation].consecutive_failures,
            3
        );
    }

    #[test]
    fn held_mailbox_retains_joined_result_until_closure() {
        let (mut registry, identity, control) = fixture();
        let id = reserve(&mut registry, identity, control.clone());
        registry.start(id, Instant::now()).unwrap();
        registry.attach_worker(id, finished()).unwrap();
        let updates = registry.entries[&id].updates.clone();
        let mut retained = Some(registry.take_finished_workers(1).pop().unwrap().join());
        updates.while_locked(|| {
            registry.request_stop(id, BorderStopReason::Explicit, Instant::now());
            assert!(control.is_stopped());
            assert!(matches!(
                updates.try_close(),
                crate::border_updates::MailboxClosure::Deferred
            ));
            retained = Some(
                registry
                    .finalize_join(retained.take().unwrap(), Instant::now())
                    .unwrap_err(),
            );
            assert!(registry.entries.contains_key(&id));
        });
        finalize(&mut registry, retained.take().unwrap());
        assert!(registry.entries.is_empty());
    }

    #[test]
    fn manual_stop_event_remains_signaled_and_failure_keeps_latch() {
        use windows::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::WaitForSingleObject;
        let control = BorderControl::new().unwrap();
        assert_eq!(
            unsafe { WaitForSingleObject(control.stop_event.handle(), 0) },
            WAIT_TIMEOUT
        );
        control.signal_stop().unwrap();
        assert_eq!(
            unsafe { WaitForSingleObject(control.stop_event.handle(), 0) },
            WAIT_OBJECT_0
        );
        assert_eq!(
            unsafe { WaitForSingleObject(control.stop_event.handle(), 0) },
            WAIT_OBJECT_0
        );
        let failed = BorderControl::new().unwrap();
        assert!(
            failed
                .signal_stop_with(|| Err(windows::core::Error::from_hresult(
                    windows::core::HRESULT(0x80004005u32 as i32)
                )))
                .is_err()
        );
        assert!(failed.is_stopped());
    }

    #[test]
    fn no_worker_settlement_requires_closure() {
        let (mut registry, identity, control) = fixture();
        let id = reserve(&mut registry, identity, control);
        assert_eq!(
            registry.settle_no_worker(id, false, Instant::now()),
            Err(RegistryError::InvalidOwnership)
        );
        settle(&mut registry, id, false, Instant::now());
    }
}
