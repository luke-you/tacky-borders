use std::cell::{Cell, RefCell};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WAIT_FAILED, WPARAM};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, MSG, SC_CLOSE, WM_CLOSE, WM_CREATE, WM_DEVICECHANGE, WM_DISPLAYCHANGE,
    WM_DPICHANGED, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_POWERBROADCAST, WM_SIZE, WM_SYSCOMMAND,
    WM_TIMER, WM_WINDOWPOSCHANGED, WM_WINDOWPOSCHANGING,
};

use crate::border_registry::{BorderControl, discard_panic};
use crate::border_registry::{
    BorderInstanceId, BorderRegistry, BorderWorkerExit, BorderWorkerResult, TrackingWindowIdentity,
};
use crate::border_updates::{BorderUpdateMailbox, MailboxDrain, PendingIpcUpdates};
use crate::utils::{
    StandaloneWindowsError, T_E_ERROR, WM_APP_ANIMATE, WM_APP_FOREGROUND, WM_APP_HIDECLOAKED,
    WM_APP_KOMOREBI, WM_APP_LOCATIONCHANGE, WM_APP_MINIMIZEEND, WM_APP_MINIMIZESTART,
    WM_APP_RECREATE_DRAWER, WM_APP_REORDER, WM_APP_SHOWUNCLOAKED, WindowsCompatibleError,
    WindowsCompatibleResult,
};
use crate::window_border::WindowBorder;

const WM_APP_SET_COLORS: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 10;
const WM_APP_SET_WIDTH: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 11;
const WM_APP_SET_OFFSET: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 12;
const WM_APP_SET_RADIUS: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 13;

pub const WORK_VISIBILITY: u8 = 1;
pub const WORK_GEOMETRY: u8 = 2;
pub const WORK_Z_ORDER: u8 = 4;
pub const WORK_DPI: u8 = 8;
pub const WORK_DISPLAY: u8 = 16;
pub const WORK_ANIMATION: u8 = 32;
pub const WORK_RENDER: u8 = 64;
pub const WORK_KOMOREBI: u8 = 128;

#[derive(Debug, Default)]
pub struct VisibilityTiming {
    was_minimized: bool,
    restore_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityAction {
    Hidden,
    Waiting,
    Visible,
}

impl VisibilityTiming {
    pub fn observe(
        &mut self,
        now: Instant,
        visible: bool,
        minimized: bool,
        delay: Duration,
    ) -> Result<VisibilityAction, TimingError> {
        if minimized {
            self.was_minimized = true;
            self.restore_at = None;
            return Ok(VisibilityAction::Hidden);
        }
        if !visible {
            self.was_minimized = false;
            self.restore_at = None;
            return Ok(VisibilityAction::Hidden);
        }
        if self.was_minimized {
            self.was_minimized = false;
            self.restore_at = Some(now.checked_add(delay).ok_or(TimingError::Unrepresentable)?);
        }
        if self.restore_at.is_some_and(|deadline| now < deadline) {
            return Ok(VisibilityAction::Waiting);
        }
        self.restore_at = None;
        Ok(VisibilityAction::Visible)
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.restore_at
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingPhase {
    Empty,
    Constructing,
    Live,
    Destroying,
}

thread_local! {
    static BORDER_STATE: RefCell<Option<WindowBorder>> = const { RefCell::new(None) };
    static BOUND_HWND: Cell<isize> = const { Cell::new(0) };
    static BINDING_PHASE: Cell<BindingPhase> = const { Cell::new(BindingPhase::Empty) };
    static CALLBACK_FAILED: Cell<bool> = const { Cell::new(false) };
    static PENDING_WORK: Cell<u8> = const { Cell::new(0) };
    static DEFERRED_WORK: Cell<u8> = const { Cell::new(0) };
    static RESOURCE_RETRY_AT: Cell<Option<Instant>> = const { Cell::new(None) };
    static RUNTIME_FAILED: Cell<bool> = const { Cell::new(false) };
    static TIMER_CLEANUP_FAILED: Cell<bool> = const { Cell::new(false) };
    static RUNTIME_OUTCOME: RefCell<Option<BorderWorkerResult>> = const { RefCell::new(None) };
    static CONTROL: RefCell<Option<Arc<BorderControl>>> = const { RefCell::new(None) };
    #[cfg(test)]
    static PANIC_MESSAGE: Cell<Option<u32>> = const { Cell::new(None) };
    #[cfg(test)]
    static CREATED_HWND: Cell<isize> = const { Cell::new(0) };
}

fn state_error() -> WindowsCompatibleError {
    WindowsCompatibleError::Standalone(StandaloneWindowsError::new(
        T_E_ERROR,
        "private border state unavailable",
    ))
}

pub(crate) fn construct(
    border: WindowBorder,
    create: impl FnOnce() -> WindowsCompatibleResult<HWND>,
) -> WindowsCompatibleResult<WindowBorder> {
    if BOUND_HWND.get() != 0 {
        return Err(state_error());
    }
    BORDER_STATE.with(|state| {
        let mut slot = state.try_borrow_mut().map_err(|_| state_error())?;
        if slot.is_some() {
            return Err(state_error());
        }
        *slot = Some(border);
        Ok(())
    })?;
    BOUND_HWND.set(0);
    BINDING_PHASE.set(BindingPhase::Constructing);
    CALLBACK_FAILED.set(false);
    PENDING_WORK.set(0);
    DEFERRED_WORK.set(0);
    RESOURCE_RETRY_AT.set(None);
    RUNTIME_FAILED.set(false);
    let created = create();
    let mut border = take_border().ok_or_else(state_error)?;
    match created {
        Ok(hwnd) => {
            border.border_window.0 = hwnd;
            if CALLBACK_FAILED.get() || BOUND_HWND.get() != hwnd.0 as isize {
                clear_binding(hwnd);
                return Err(state_error());
            }
            BINDING_PHASE.set(BindingPhase::Live);
            Ok(border)
        }
        Err(error) => {
            border.border_window.0 = HWND::default();
            BOUND_HWND.set(0);
            BINDING_PHASE.set(BindingPhase::Empty);
            Err(error)
        }
    }
}

pub(crate) fn install_border(border: WindowBorder) -> Result<(), Box<WindowBorder>> {
    BORDER_STATE.with(|state| {
        let Ok(mut slot) = state.try_borrow_mut() else {
            return Err(Box::new(border));
        };
        if slot.is_some() {
            return Err(Box::new(border));
        }
        *slot = Some(border);
        Ok(())
    })
}

pub(crate) fn take_border() -> Option<WindowBorder> {
    BORDER_STATE
        .try_with(|state| state.try_borrow_mut().ok()?.take())
        .ok()
        .flatten()
}

pub(crate) fn clear_binding(hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    let _ = BOUND_HWND.try_with(|bound| {
        if bound.get() == hwnd.0 as isize {
            bound.set(0);
            let _ = BINDING_PHASE.try_with(|phase| phase.set(BindingPhase::Destroying));
        }
    });
}

pub(crate) fn bind_control(control: Arc<BorderControl>) -> bool {
    CONTROL
        .try_with(|slot| {
            let Ok(mut slot) = slot.try_borrow_mut() else {
                return false;
            };
            *slot = Some(control);
            true
        })
        .unwrap_or(false)
}

pub fn take_pending_work() -> u8 {
    PENDING_WORK.replace(0)
}

pub(crate) fn defer_pending_work(bits: u8) {
    DEFERRED_WORK.set(DEFERRED_WORK.get() | bits);
    if RESOURCE_RETRY_AT.get().is_none() {
        match Instant::now().checked_add(LOCK_RETRY_INTERVAL) {
            Some(deadline) => RESOURCE_RETRY_AT.set(Some(deadline)),
            None => record_runtime_failure(),
        }
    }
}

pub(crate) fn record_runtime_failure() {
    RUNTIME_FAILED.set(true);
}

pub(crate) fn record_timer_cleanup_failure() {
    let _ = TIMER_CLEANUP_FAILED.try_with(|failed| failed.set(true));
}

pub fn callback_failed() -> bool {
    CALLBACK_FAILED.try_with(Cell::get).unwrap_or(true)
}

pub(crate) fn stopped() -> bool {
    CONTROL
        .try_with(|slot| {
            slot.try_borrow().map_or(true, |control| {
                control.as_ref().is_some_and(|control| control.is_stopped())
            })
        })
        .unwrap_or(true)
}

fn reject_public_message(message: u32, wparam: WPARAM) -> bool {
    matches!(
        message,
        WM_CLOSE
            | WM_TIMER
            | WM_APP_SET_COLORS
            | WM_APP_SET_WIDTH
            | WM_APP_SET_OFFSET
            | WM_APP_SET_RADIUS
    ) || (message == WM_SYSCOMMAND && (wparam.0 & 0xfff0) == SC_CLOSE as usize)
        || matches!(message, WM_WINDOWPOSCHANGING | WM_WINDOWPOSCHANGED)
}

fn fallback(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if reject_public_message(message, wparam) {
        return LRESULT(0);
    }
    match message {
        WM_NCCREATE => LRESULT(0),
        WM_CREATE => LRESULT(-1),
        WM_DPICHANGED
        | WM_APP_ANIMATE
        | WM_APP_FOREGROUND
        | WM_APP_KOMOREBI
        | WM_APP_LOCATIONCHANGE
        | WM_APP_REORDER
        | WM_APP_SHOWUNCLOAKED
        | WM_APP_HIDECLOAKED
        | WM_APP_MINIMIZESTART
        | WM_APP_MINIMIZEEND
        | WM_APP_RECREATE_DRAWER => LRESULT(0),
        _ => unsafe { DefWindowProcW(window, message, wparam, lparam) },
    }
}

fn record_callback_failure() {
    let _ = CALLBACK_FAILED.try_with(|failed| failed.set(true));
    let _ = CONTROL.try_with(|slot| {
        if let Ok(control) = slot.try_borrow()
            && let Some(control) = control.as_ref()
        {
            control.record_callback_failure();
            let _ = control.signal_stop();
        }
    });
}

fn dispatch(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    #[cfg(test)]
    if PANIC_MESSAGE.get() == Some(message) {
        PANIC_MESSAGE.set(None);
        std::panic::panic_any(CallbackPanicPayload);
    }
    if reject_public_message(message, wparam) {
        return LRESULT(0);
    }
    let phase = BINDING_PHASE.get();
    if message == WM_NCCREATE {
        if phase != BindingPhase::Constructing || BOUND_HWND.get() != 0 || stopped() {
            return LRESULT(0);
        }
        let available = BORDER_STATE.with(|state| {
            state.try_borrow_mut().is_ok_and(|mut state| {
                if let Some(border) = state.as_mut() {
                    border.border_window.0 = window;
                    true
                } else {
                    false
                }
            })
        });
        if !available {
            record_callback_failure();
            return LRESULT(0);
        }
        BOUND_HWND.set(window.0 as isize);
        #[cfg(test)]
        CREATED_HWND.set(window.0 as isize);
        return unsafe { DefWindowProcW(window, message, wparam, lparam) };
    }
    if window.0 as isize != BOUND_HWND.get()
        || !matches!(phase, BindingPhase::Constructing | BindingPhase::Live)
    {
        return fallback(window, message, wparam, lparam);
    }
    if message == WM_CREATE {
        if phase != BindingPhase::Constructing {
            return LRESULT(-1);
        }
        let available =
            BORDER_STATE.with(|state| state.try_borrow_mut().is_ok_and(|state| state.is_some()));
        if !available || stopped() {
            record_callback_failure();
            return LRESULT(-1);
        }
        return LRESULT(0);
    }
    if message == WM_NCDESTROY {
        return unsafe { DefWindowProcW(window, message, wparam, lparam) };
    }
    if stopped() {
        return fallback(window, message, wparam, lparam);
    }
    let bits = match message {
        WM_APP_LOCATIONCHANGE | WM_SIZE => WORK_GEOMETRY,
        WM_APP_SHOWUNCLOAKED | WM_APP_HIDECLOAKED | WM_APP_MINIMIZESTART | WM_APP_MINIMIZEEND => {
            WORK_VISIBILITY
        }
        WM_APP_REORDER => WORK_Z_ORDER,
        WM_APP_FOREGROUND => WORK_RENDER,
        WM_APP_KOMOREBI => WORK_KOMOREBI,
        WM_DPICHANGED => WORK_DPI | WORK_GEOMETRY | WORK_RENDER,
        WM_DISPLAYCHANGE | WM_DEVICECHANGE | WM_APP_RECREATE_DRAWER | WM_POWERBROADCAST => {
            WORK_DISPLAY | WORK_GEOMETRY | WORK_RENDER
        }
        WM_APP_ANIMATE => WORK_ANIMATION | WORK_RENDER,
        WM_PAINT => {
            let _ = unsafe { ValidateRect(Some(window), None) };
            WORK_RENDER
        }
        _ => return fallback(window, message, wparam, lparam),
    };
    PENDING_WORK.set(PENDING_WORK.get() | bits);
    LRESULT(0)
}

pub(crate) fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match catch_unwind(AssertUnwindSafe(|| {
        dispatch(window, message, wparam, lparam)
    })) {
        Ok(result) => result,
        Err(payload) => {
            record_callback_failure();
            discard_panic(payload);
            fallback(window, message, wparam, lparam)
        }
    }
}

#[cfg(test)]
struct CallbackPanicPayload;

#[cfg(test)]
impl Drop for CallbackPanicPayload {
    fn drop(&mut self) {
        panic!("injected panic payload destructor failure");
    }
}

pub fn queued_message_allowed(message: &MSG) -> bool {
    message.message != WM_TIMER
}

pub fn service_pending_work() -> bool {
    let mut bits = take_pending_work();
    if RESOURCE_RETRY_AT
        .get()
        .is_some_and(|deadline| Instant::now() >= deadline)
    {
        bits |= DEFERRED_WORK.replace(0);
        RESOURCE_RETRY_AT.set(None);
    } else {
        bits &= !DEFERRED_WORK.get();
    }
    if bits == 0 {
        return true;
    }
    let Some(mut border) = take_border() else {
        return false;
    };
    let hidden = CONTROL.with(|slot| {
        slot.borrow().as_ref().is_some_and(|control| {
            control.visibility() == crate::border_registry::DesiredVisibility::Hidden
        })
    });
    if !stopped()
        && let Err(error) = border.service_work(bits, hidden, Instant::now())
    {
        if error
            .downcast_ref::<WindowsCompatibleError>()
            .is_some_and(|error| error.code() == crate::utils::T_E_REENTRANCY)
        {
            defer_pending_work(bits);
        } else {
            let outcome = if error
                .downcast_ref::<crate::anim_timer::AnimationTimerExit>()
                .is_some()
            {
                BorderWorkerResult::AnimationFailed(format!("animation startup: {error:#}"))
            } else {
                BorderWorkerResult::RuntimeFailed(format!("owner service: {error:#}"))
            };
            RUNTIME_OUTCOME.with(|slot| *slot.borrow_mut() = Some(outcome));
        }
    }
    install_border(border).is_ok()
}

pub const STARTUP_WORK_BUDGET: Duration = Duration::from_secs(10);
pub const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(10);
pub const MAX_CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub const MAX_MESSAGES_PER_TURN: usize = 64;

pub struct BorderWorkerContext {
    pub id: BorderInstanceId,
    pub tracking: TrackingWindowIdentity,
    pub control: Arc<BorderControl>,
    pub updates: Arc<BorderUpdateMailbox>,
    pub registry: Arc<Mutex<BorderRegistry>>,
    pub started_at: Instant,
    pub initial: bool,
    #[cfg(test)]
    pub probe: Option<Arc<dyn Fn(WorkerStage) + Send + Sync>>,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub enum WorkerStage {
    SnapshotDeferred,
    Created(isize),
    PreparationDeferred(isize),
    Prepared(isize),
    PublicationDeferred,
    Running(isize),
    SecondRenderDue,
    IdleWait,
    BodyExited(crate::border_registry::BorderWorkerResult),
}

impl BorderWorkerContext {
    #[cfg(test)]
    fn report_stage(&self, stage: WorkerStage) {
        if let Some(probe) = &self.probe {
            probe(stage);
        }
    }
}

enum SnapshotResult {
    Ready(
        Box<crate::config::RuntimeConfig>,
        u64,
        crate::border_updates::AppearanceStamps,
    ),
    Deferred,
    Failed,
}

fn configuration_snapshot() -> SnapshotResult {
    let gate = match crate::APP_STATE.appearance.try_lock() {
        Ok(gate) => gate,
        Err(TryLockError::WouldBlock) => return SnapshotResult::Deferred,
        Err(TryLockError::Poisoned(_)) => return SnapshotResult::Failed,
    };
    let config = match crate::APP_STATE.config.try_read() {
        Ok(config) => config.clone(),
        Err(TryLockError::WouldBlock) => return SnapshotResult::Deferred,
        Err(TryLockError::Poisoned(_)) => return SnapshotResult::Failed,
    };
    SnapshotResult::Ready(Box::new(config), gate.config_revision, gate.global_stamps)
}

fn publish(context: &BorderWorkerContext, hwnd: HWND, revision: u64) -> Result<Option<bool>, ()> {
    let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    let gate = match crate::APP_STATE.appearance.try_lock() {
        Ok(gate) => gate,
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Poisoned(_)) => return Err(()),
    };
    match crate::APP_STATE.config.try_read() {
        Ok(config) => drop(config),
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Poisoned(_)) => return Err(()),
    }
    if gate.config_revision != revision {
        return Ok(Some(false));
    }
    let mut registry = match context.registry.try_lock() {
        Ok(registry) => registry,
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Poisoned(_)) => return Err(()),
    };
    if registry.publish_running(context.id, hwnd.0 as isize, thread_id, revision) {
        Ok(Some(true))
    } else {
        Err(())
    }
}

fn initialization_failure(stage: &str) -> BorderWorkerResult {
    BorderWorkerResult::InitializationFailed(stage.into())
}

fn owned_border_invalid() -> bool {
    BORDER_STATE.with(|state| {
        let mut state = state.borrow_mut();
        let Some(border) = state.as_mut() else {
            return false;
        };
        if unsafe {
            windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(border.border_window.0))
                .as_bool()
                && windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                    border.border_window.0,
                    None,
                ) == windows::Win32::System::Threading::GetCurrentThreadId()
        } {
            return false;
        }
        clear_binding(border.border_window.0);
        border.border_window.0 = HWND::default();
        true
    })
}

fn invalidated_border_result(running: bool) -> BorderWorkerResult {
    if running {
        BorderWorkerResult::RuntimeFailed("owned border window invalidated".into())
    } else {
        initialization_failure("owned border window invalidated")
    }
}

pub fn run_border_worker(context: BorderWorkerContext) -> BorderWorkerExit {
    TIMER_CLEANUP_FAILED.set(false);
    let primary = match catch_unwind(AssertUnwindSafe(|| {
        let mut primary = run_worker_body(&context);
        #[cfg(test)]
        context.report_stage(WorkerStage::BodyExited(primary.clone()));
        if primary == BorderWorkerResult::Completed {
            if callback_failed() || context.control.callback_failed() {
                primary = BorderWorkerResult::CallbackFailed;
            } else if let Some(outcome) = RUNTIME_OUTCOME.with(|slot| slot.borrow_mut().take()) {
                primary = outcome;
            } else if RUNTIME_FAILED.get() {
                primary = BorderWorkerResult::RuntimeFailed("owner service failed".into());
            }
        }
        primary
    })) {
        Ok(primary) => primary,
        Err(payload) => {
            discard_panic(payload);
            BorderWorkerResult::Panicked
        }
    };
    let mut cleanup_failures = Vec::new();
    let cleanup = catch_unwind(AssertUnwindSafe(|| {
        owned_border_invalid();
        if let Some(mut border) = take_border() {
            clear_binding(border.border_window.0);
            cleanup_failures.extend(border.destroy_owned());
        }
    }));
    if let Err(payload) = cleanup {
        discard_panic(payload);
        cleanup_failures.push("owner cleanup panicked".into());
    }
    if TIMER_CLEANUP_FAILED.get() {
        cleanup_failures.push("animation cleanup backstop observed failure".into());
    }
    let _ = CONTROL.try_with(|slot| {
        if let Ok(mut slot) = slot.try_borrow_mut() {
            *slot = None;
        }
    });
    BorderWorkerExit {
        primary,
        cleanup_failures,
    }
}

fn run_worker_body(context: &BorderWorkerContext) -> BorderWorkerResult {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_NOREMOVE, PM_REMOVE,
        PeekMessageW, QS_ALLINPUT, TranslateMessage, WM_QUIT,
    };
    if !bind_control(context.control.clone()) {
        return initialization_failure("control binding");
    }
    CALLBACK_FAILED.set(false);
    RUNTIME_FAILED.set(false);
    RUNTIME_OUTCOME.with(|slot| *slot.borrow_mut() = None);
    let mut message = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
    }
    let Ok(mut timing) = StartupTiming::new(context.started_at, STARTUP_WORK_BUDGET) else {
        return initialization_failure("startup timing overflow");
    };
    let mut deferred = DeferredOperations::default();
    let mut effective = PendingIpcUpdates::default();
    let mut prepared_revision = None;
    let mut running = false;
    let mut appearance_pending = false;
    let mut second_render = None;
    loop {
        if callback_failed() || context.control.callback_failed() {
            return BorderWorkerResult::CallbackFailed;
        }
        if let Some(outcome) = RUNTIME_OUTCOME.with(|slot| slot.borrow_mut().take()) {
            return outcome;
        }
        if RUNTIME_FAILED.get() {
            return BorderWorkerResult::RuntimeFailed("owner service failed".into());
        }
        if context.control.is_stopped() {
            return BorderWorkerResult::Completed;
        }
        if !context.tracking.fingerprint.is_current() {
            return BorderWorkerResult::Completed;
        }
        if owned_border_invalid() {
            return invalidated_border_result(running);
        }
        let now = Instant::now();
        if !running && timing.check(now).is_err() {
            return initialization_failure("startup deadline expired");
        }
        if deferred.is_due(DeferredOperation::Mailbox, now) {
            match context.updates.try_drain() {
                MailboxDrain::Updates(updates) => {
                    let changed = !updates.is_empty();
                    effective.merge(updates);
                    deferred.complete(DeferredOperation::Mailbox);
                    if changed {
                        if running {
                            appearance_pending = true;
                        } else {
                            prepared_revision = None;
                        }
                    }
                }
                MailboxDrain::Deferred => {
                    let _ = deferred.defer(DeferredOperation::Mailbox, now);
                }
                MailboxDrain::Failed => {
                    if context.control.is_stopped() {
                        return BorderWorkerResult::Completed;
                    }
                    return BorderWorkerResult::RuntimeFailed("mailbox failed".into());
                }
            }
        }
        if context.control.is_stopped() {
            continue;
        }
        if running {
            PENDING_WORK.set(PENDING_WORK.get() | context.control.take_work());
        }
        if running
            && appearance_pending
            && deferred.is_due(DeferredOperation::Resources, now)
            && let Some(mut border) = take_border()
        {
            let applied = border.apply_updates(&effective);
            let _ = install_border(border);
            match applied {
                Ok(()) => {
                    appearance_pending = false;
                    deferred.complete(DeferredOperation::Resources);
                }
                Err(error)
                    if error
                        .downcast_ref::<WindowsCompatibleError>()
                        .is_some_and(|error| error.code() == crate::utils::T_E_REENTRANCY) =>
                {
                    if deferred.defer(DeferredOperation::Resources, now).is_err() {
                        return BorderWorkerResult::RuntimeFailed("resource retry overflow".into());
                    }
                }
                Err(error) => {
                    return BorderWorkerResult::RuntimeFailed(format!("appearance: {error:#}"));
                }
            }
        }
        if !running
            && prepared_revision.is_none()
            && deferred.is_due(DeferredOperation::Snapshot, now)
        {
            match configuration_snapshot() {
                SnapshotResult::Deferred => {
                    let _ = deferred.defer(DeferredOperation::Snapshot, now);
                    #[cfg(test)]
                    context.report_stage(WorkerStage::SnapshotDeferred);
                }
                SnapshotResult::Failed => {
                    return initialization_failure("configuration snapshot poisoned");
                }
                SnapshotResult::Ready(config, revision, stamps) => {
                    deferred.complete(DeferredOperation::Snapshot);
                    let hwnd = HWND(context.tracking.fingerprint.hwnd as _);
                    let rule = crate::utils::get_window_rule_from_config(hwnd, &config);
                    if rule.enabled == Some(crate::config::EnableMode::Bool(false)) {
                        return BorderWorkerResult::Completed;
                    }
                    let mut resolved = crate::border_config::BorderConfig::resolve(
                        &rule,
                        &config.global,
                        config.render_backend,
                        context.initial,
                    );
                    if timing
                        .capture_delay(Instant::now(), resolved.initialize_delay)
                        .is_err()
                    {
                        return initialization_failure("initial snapshot timing");
                    }
                    effective.merge(PendingIpcUpdates::from_config(&resolved, stamps));
                    effective.apply_to_config(&mut resolved);
                    let mut border = match take_border() {
                        Some(border) => border,
                        None => match WindowBorder::new(hwnd) {
                            Ok(border) => {
                                #[cfg(test)]
                                context.report_stage(WorkerStage::Created(
                                    border.border_window.0.0 as isize,
                                ));
                                border
                            }
                            Err(error) => {
                                return initialization_failure(&format!(
                                    "window creation: {error:#}"
                                ));
                            }
                        },
                    };
                    let prepared = border.prepare(resolved);
                    let _ = install_border(border);
                    if let Err(error) = prepared {
                        if error
                            .downcast_ref::<WindowsCompatibleError>()
                            .is_some_and(|error| error.code() == crate::utils::T_E_REENTRANCY)
                        {
                            let _ = deferred.defer(DeferredOperation::Snapshot, now);
                            #[cfg(test)]
                            {
                                let hwnd = BORDER_STATE.with(|state| {
                                    state.borrow().as_ref().unwrap().border_window.0.0 as isize
                                });
                                context.report_stage(WorkerStage::PreparationDeferred(hwnd));
                            }
                        } else {
                            return initialization_failure(&format!(
                                "hidden preparation: {error:#}"
                            ));
                        }
                    } else {
                        if timing.prepared(Instant::now()).is_err() {
                            return initialization_failure("activation timing");
                        }
                        prepared_revision = Some(revision);
                        #[cfg(test)]
                        {
                            let hwnd = BORDER_STATE.with(|state| {
                                state
                                    .borrow()
                                    .as_ref()
                                    .map(|border| border.border_window.0.0 as isize)
                                    .unwrap_or(0)
                            });
                            context.report_stage(WorkerStage::Prepared(hwnd));
                        }
                    }
                }
            }
        }
        if context.control.is_stopped() {
            continue;
        }
        if owned_border_invalid() {
            return invalidated_border_result(running);
        }
        if !running
            && timing.ready(Instant::now())
            && deferred.is_due(DeferredOperation::Publication, now)
            && let Some(revision) = prepared_revision
        {
            let hwnd = BORDER_STATE
                .with(|state| state.borrow().as_ref().map(|border| border.border_window.0));
            let Some(hwnd) = hwnd else {
                return initialization_failure("prepared state missing");
            };
            match publish(context, hwnd, revision) {
                Ok(None) => {
                    let _ = deferred.defer(DeferredOperation::Publication, now);
                    #[cfg(test)]
                    context.report_stage(WorkerStage::PublicationDeferred);
                }
                Ok(Some(false)) => {
                    prepared_revision = None;
                    deferred.complete(DeferredOperation::Publication);
                }
                Err(()) => return initialization_failure("publication rejected"),
                Ok(Some(true)) => {
                    if context.control.is_stopped() || !context.tracking.fingerprint.is_current() {
                        continue;
                    }
                    let Some(mut border) = take_border() else {
                        return initialization_failure("activation state missing");
                    };
                    let activated = border.activate(
                        context.control.visibility()
                            == crate::border_registry::DesiredVisibility::Hidden,
                    );
                    let _ = install_border(border);
                    if let Err(error) = activated {
                        return initialization_failure(&format!("activation: {error:#}"));
                    }
                    running = true;
                    #[cfg(test)]
                    context.report_stage(WorkerStage::Running(hwnd.0 as isize));
                    second_render = Instant::now().checked_add(Duration::from_millis(5));
                }
            }
        }
        if owned_border_invalid() {
            return invalidated_border_result(running);
        }
        if running {
            PENDING_WORK.set(PENDING_WORK.get() | WORK_VISIBILITY);
            if second_render.is_some_and(|deadline| Instant::now() >= deadline) {
                #[cfg(test)]
                context.report_stage(WorkerStage::SecondRenderDue);
                PENDING_WORK.set(PENDING_WORK.get() | WORK_RENDER);
                second_render = None;
            }
            if let Some(mut border) = take_border() {
                let failure = border.animation_failure();
                let _ = install_border(border);
                if let Some(failure) = failure {
                    return BorderWorkerResult::AnimationFailed(format!("{failure:?}"));
                }
            }
            service_pending_work();
        }
        for _ in 0..MAX_MESSAGES_PER_TURN {
            if context.control.is_stopped() {
                break;
            }
            if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                break;
            }
            if message.message == WM_QUIT {
                return BorderWorkerResult::Completed;
            }
            if queued_message_allowed(&message) {
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
        if context.control.is_stopped() {
            continue;
        }
        if running {
            service_pending_work();
        }
        let now = Instant::now();
        let mut timeout = deferred.next_wait(now);
        if !running {
            timeout = timeout.min(timing.wait_duration(now));
        }
        if let Some(deadline) = second_render {
            timeout = timeout.min(deadline.saturating_duration_since(now));
        }
        if let Some(deadline) = RESOURCE_RETRY_AT.get() {
            timeout = timeout.min(deadline.saturating_duration_since(now));
        }
        if let Some(deadline) = BORDER_STATE.with(|state| {
            state
                .borrow()
                .as_ref()
                .and_then(WindowBorder::restore_deadline)
        }) {
            timeout = timeout.min(deadline.saturating_duration_since(now));
        }
        if running && PENDING_WORK.get() != 0 {
            timeout = Duration::ZERO;
        }
        let handles = [
            context.control.stop_event.handle(),
            context.updates.wake_event.handle(),
        ];
        #[cfg(test)]
        if running && timeout >= MAX_CONTROL_POLL_INTERVAL {
            context.report_stage(WorkerStage::IdleWait);
        }
        let wait = unsafe {
            MsgWaitForMultipleObjectsEx(
                Some(&handles),
                timeout.as_millis().min(100) as u32,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            )
        };
        if wait == WAIT_FAILED {
            return BorderWorkerResult::RuntimeFailed("message-aware wait failed".into());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimingError {
    Unrepresentable,
    TimedOut,
    SnapshotNotCaptured,
}

#[derive(Debug)]
pub struct StartupTiming {
    started_at: Instant,
    work_budget: Duration,
    hard_deadline: Instant,
    frozen_delay: Option<Duration>,
    activation_at: Option<Instant>,
}

impl StartupTiming {
    pub fn new(started_at: Instant, work_budget: Duration) -> Result<Self, TimingError> {
        let hard_deadline = started_at
            .checked_add(work_budget)
            .ok_or(TimingError::Unrepresentable)?;
        Ok(Self {
            started_at,
            work_budget,
            hard_deadline,
            frozen_delay: None,
            activation_at: None,
        })
    }

    pub fn capture_delay(&mut self, now: Instant, delay_ms: u64) -> Result<(), TimingError> {
        self.check(now)?;
        if self.frozen_delay.is_some() {
            return Ok(());
        }
        let delay = Duration::from_millis(delay_ms);
        let total = self
            .work_budget
            .checked_add(delay)
            .ok_or(TimingError::Unrepresentable)?;
        let deadline = self
            .started_at
            .checked_add(total)
            .ok_or(TimingError::Unrepresentable)?;
        self.frozen_delay = Some(delay);
        self.hard_deadline = deadline;
        Ok(())
    }

    pub fn prepared(&mut self, now: Instant) -> Result<(), TimingError> {
        self.check(now)?;
        if self.activation_at.is_some() {
            return Ok(());
        }
        let delay = self.frozen_delay.ok_or(TimingError::SnapshotNotCaptured)?;
        self.activation_at = Some(now.checked_add(delay).ok_or(TimingError::Unrepresentable)?);
        Ok(())
    }

    pub fn check(&self, now: Instant) -> Result<(), TimingError> {
        if now >= self.hard_deadline {
            Err(TimingError::TimedOut)
        } else {
            Ok(())
        }
    }

    pub fn ready(&self, now: Instant) -> bool {
        now < self.hard_deadline && self.activation_at.is_some_and(|deadline| now >= deadline)
    }

    pub fn hard_deadline(&self) -> Instant {
        self.hard_deadline
    }

    pub fn activation_at(&self) -> Option<Instant> {
        self.activation_at
    }

    pub fn wait_duration(&self, now: Instant) -> Duration {
        let deadline = self
            .activation_at
            .filter(|deadline| *deadline > now)
            .map_or(self.hard_deadline, |activation| {
                activation.min(self.hard_deadline)
            });
        deadline
            .saturating_duration_since(now)
            .min(MAX_CONTROL_POLL_INTERVAL)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum DeferredOperation {
    Snapshot = 0,
    Publication = 1,
    Mailbox = 2,
    Resources = 3,
}

#[derive(Debug, Default)]
pub struct DeferredOperations {
    retry_at: [Option<Instant>; 4],
}

impl DeferredOperations {
    pub fn defer(&mut self, operation: DeferredOperation, now: Instant) -> Result<(), TimingError> {
        self.retry_at[operation as usize] = Some(
            now.checked_add(LOCK_RETRY_INTERVAL)
                .ok_or(TimingError::Unrepresentable)?,
        );
        Ok(())
    }

    pub fn is_due(&self, operation: DeferredOperation, now: Instant) -> bool {
        self.retry_at[operation as usize].is_none_or(|deadline| now >= deadline)
    }

    pub fn complete(&mut self, operation: DeferredOperation) {
        self.retry_at[operation as usize] = None;
    }

    pub fn next_wait(&self, now: Instant) -> Duration {
        self.retry_at
            .iter()
            .flatten()
            .map(|deadline| deadline.saturating_duration_since(now))
            .min()
            .unwrap_or(MAX_CONTROL_POLL_INTERVAL)
            .min(MAX_CONTROL_POLL_INTERVAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TrackingFixture(
        HWND,
        std::sync::mpsc::Sender<()>,
        Option<std::thread::JoinHandle<()>>,
        crate::config::ZOrderMode,
    );

    impl Drop for TrackingFixture {
        fn drop(&mut self) {
            let _ = self.1.send(());
            if let Some(worker) = self.2.take()
                && let Err(payload) = worker.join()
            {
                discard_panic(payload);
            }
            crate::APP_STATE.get_config_mut().global.border_z_order = self.3;
        }
    }

    fn tracking_window() -> TrackingFixture {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DispatchMessageW, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx,
            PM_REMOVE, PeekMessageW, QS_ALLINPUT, WINDOW_EX_STYLE, WS_POPUP,
        };
        let (created, ready) = std::sync::mpsc::channel();
        let (stop, stopping) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let window = crate::utils::OwnedHWND(unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    windows::core::w!("STATIC"),
                    windows::core::w!("fix-01-test-owned"),
                    WS_POPUP,
                    0,
                    0,
                    100,
                    100,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap()
            });
            created.send(window.0.0 as isize).unwrap();
            let mut message = MSG::default();
            while matches!(
                stopping.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ) {
                for _ in 0..64 {
                    if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                        break;
                    }
                    unsafe {
                        DispatchMessageW(&message);
                    }
                }
                unsafe {
                    MsgWaitForMultipleObjectsEx(None, 10, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                }
            }
        });
        let hwnd = HWND(ready.recv_timeout(Duration::from_secs(2)).unwrap() as _);
        let previous_z_order = {
            let mut config = crate::APP_STATE.get_config_mut();
            std::mem::replace(
                &mut config.global.border_z_order,
                crate::config::ZOrderMode::BelowWindow,
            )
        };
        TrackingFixture(hwnd, stop, Some(worker), previous_z_order)
    }

    fn worker_context(hwnd: HWND) -> BorderWorkerContext {
        let mut registry = BorderRegistry::default();
        registry.admission = crate::border_registry::BorderAdmission::Open;
        let tracking = registry
            .observe(crate::border_registry::TrackingWindowFingerprint::capture(hwnd).unwrap())
            .unwrap();
        let control = Arc::new(BorderControl::new().unwrap());
        let updates = Arc::new(BorderUpdateMailbox::new(control.clone()).unwrap());
        let started_at = Instant::now();
        let id = match registry
            .reserve(tracking, updates.clone(), started_at)
            .unwrap()
        {
            crate::border_registry::ReserveResult::Reserved(id) => id,
            _ => panic!(),
        };
        registry.start(id, started_at).unwrap();
        BorderWorkerContext {
            id,
            tracking,
            control,
            updates,
            registry: Arc::new(Mutex::new(registry)),
            started_at,
            initial: true,
            probe: None,
        }
    }

    fn join_promptly(worker: std::thread::JoinHandle<BorderWorkerExit>) -> BorderWorkerExit {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !worker.is_finished() {
            assert!(
                Instant::now() < deadline,
                "worker did not stop while shared lock remained held"
            );
            std::thread::yield_now();
        }
        worker.join().unwrap()
    }

    struct TestWorker {
        control: Arc<BorderControl>,
        worker: Option<std::thread::JoinHandle<BorderWorkerExit>>,
    }

    impl TestWorker {
        fn spawn(context: BorderWorkerContext) -> Self {
            Self {
                control: context.control.clone(),
                worker: Some(std::thread::spawn(move || run_border_worker(context))),
            }
        }

        fn join(mut self) -> BorderWorkerExit {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !self.worker.as_ref().unwrap().is_finished() {
                assert!(
                    Instant::now() < deadline,
                    "worker did not settle before test deadline"
                );
                std::thread::yield_now();
            }
            join_promptly(self.worker.take().unwrap())
        }
    }

    impl Drop for TestWorker {
        fn drop(&mut self) {
            self.control.record_stop();
            let _ = self.control.signal_stop();
            if let Some(worker) = self.worker.take()
                && let Err(payload) = worker.join()
            {
                discard_panic(payload);
            }
        }
    }

    fn destroyed_border_outcome(running: bool) -> BorderWorkerExit {
        use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, IsWindow};
        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        let mut context = worker_context(tracking.0);
        let (sender, receiver) = std::sync::mpsc::channel();
        context.probe = Some(Arc::new(move |stage| {
            let hwnd = match (running, stage) {
                (true, WorkerStage::Running(hwnd)) | (false, WorkerStage::Prepared(hwnd)) => hwnd,
                _ => return,
            };
            unsafe { DestroyWindow(HWND(hwnd as _)) }.unwrap();
            sender.send(hwnd).unwrap();
        }));
        let worker = TestWorker::spawn(context);
        let hwnd = HWND(receiver.recv_timeout(Duration::from_secs(2)).unwrap() as _);
        let outcome = worker.join();
        assert!(
            outcome.cleanup_failures.is_empty(),
            "unexpected cleanup: {outcome:?}"
        );
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
        assert!(unsafe { IsWindow(Some(tracking.0)) }.as_bool());
        outcome
    }

    #[test]
    #[serial_test::serial]
    fn destroyed_running_border_exits_as_runtime_failure() {
        let outcome = destroyed_border_outcome(true);
        assert!(
            matches!(outcome.primary, BorderWorkerResult::RuntimeFailed(_)),
            "unexpected exit: {outcome:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn destroyed_prepared_border_cannot_publish_running() {
        let outcome = destroyed_border_outcome(false);
        assert!(
            matches!(outcome.primary, BorderWorkerResult::InitializationFailed(_)),
            "unexpected exit: {outcome:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn destroyed_idle_border_exits_without_hook_or_update_wake() {
        use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, IsWindow};
        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        let mut context = worker_context(tracking.0);
        let control = context.control.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        context.probe = Some(Arc::new(move |stage| {
            if !matches!(stage, WorkerStage::IdleWait) {
                return;
            }
            assert!(!control.is_stopped());
            let hwnd = BOUND_HWND.get();
            unsafe { DestroyWindow(HWND(hwnd as _)) }.unwrap();
            PENDING_WORK.set(0);
            DEFERRED_WORK.set(0);
            RESOURCE_RETRY_AT.set(None);
            sender.send(hwnd).unwrap();
        }));
        let worker = TestWorker::spawn(context);
        let hwnd = HWND(receiver.recv_timeout(Duration::from_secs(2)).unwrap() as _);
        let outcome = worker.join();
        assert!(
            matches!(outcome.primary, BorderWorkerResult::RuntimeFailed(_)),
            "unexpected exit: {outcome:?}"
        );
        assert!(
            outcome.cleanup_failures.is_empty(),
            "unexpected cleanup: {outcome:?}"
        );
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
        assert!(unsafe { IsWindow(Some(tracking.0)) }.as_bool());
    }

    #[test]
    #[serial_test::serial]
    fn missing_running_renderer_preserves_service_error_and_cleans_up() {
        use windows::Win32::UI::WindowsAndMessaging::{
            IsWindow, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos,
        };
        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        unsafe {
            SetWindowPos(
                tracking.0,
                None,
                -32000,
                -32000,
                100,
                100,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
            )
        }
        .unwrap();
        let mut context = worker_context(tracking.0);
        let (sender, receiver) = std::sync::mpsc::channel();
        context.probe = Some(Arc::new(move |stage| {
            if let WorkerStage::Running(hwnd) = stage {
                BORDER_STATE.with(|state| {
                    let mut state = state.borrow_mut();
                    let border = state.as_mut().unwrap();
                    border.remove_running_renderer_for_test();
                });
                PENDING_WORK.set(WORK_RENDER);
                sender.send(hwnd).unwrap();
            }
        }));
        let worker = TestWorker::spawn(context);
        let hwnd = HWND(receiver.recv_timeout(Duration::from_secs(2)).unwrap() as _);
        let outcome = worker.join();
        assert!(
            matches!(&outcome.primary, BorderWorkerResult::RuntimeFailed(error) if error.contains("owner service:") && error.contains("render_backend is None")),
            "unexpected exit: {outcome:?}"
        );
        assert!(
            outcome.cleanup_failures.is_empty(),
            "unexpected cleanup: {outcome:?}"
        );
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
        assert!(unsafe { IsWindow(Some(tracking.0)) }.as_bool());
    }

    #[test]
    #[serial_test::serial]
    fn running_animation_spawn_failure_is_classified_without_orphan() {
        use windows::Win32::UI::WindowsAndMessaging::{
            IsWindow, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos,
        };
        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        unsafe {
            SetWindowPos(
                tracking.0,
                None,
                -32000,
                -32000,
                100,
                100,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
            )
        }
        .unwrap();
        let mut context = worker_context(tracking.0);
        let (sender, receiver) = std::sync::mpsc::channel();
        context.probe = Some(Arc::new(move |stage| {
            if let WorkerStage::Running(hwnd) = stage {
                BORDER_STATE.with(|state| {
                    state
                        .borrow_mut()
                        .as_mut()
                        .unwrap()
                        .enable_animation_for_test()
                });
                crate::anim_timer::AnimationTimer::fail_next_spawn_for_test();
                PENDING_WORK.set(WORK_RENDER);
                sender.send(hwnd).unwrap();
            }
        }));
        let worker = TestWorker::spawn(context);
        let hwnd = HWND(receiver.recv_timeout(Duration::from_secs(2)).unwrap() as _);
        let outcome = worker.join();
        assert!(
            matches!(&outcome.primary, BorderWorkerResult::AnimationFailed(error) if error.contains("SpawnFailed") && error.contains("injected timer spawn failure")),
            "unexpected exit: {outcome:?}"
        );
        assert!(
            outcome.cleanup_failures.is_empty(),
            "unexpected cleanup: {outcome:?}"
        );
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
        assert!(unsafe { IsWindow(Some(tracking.0)) }.as_bool());
    }

    #[test]
    #[serial_test::serial]
    fn callback_failure_latched_after_stop_check_cannot_report_completed() {
        failure_latched_after_stop(true);
    }

    #[test]
    #[serial_test::serial]
    fn runtime_failure_latched_after_stop_check_cannot_report_completed() {
        failure_latched_after_stop(false);
    }

    fn failure_latched_after_stop(callback: bool) {
        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        let mut context = worker_context(tracking.0);
        let control = context.control.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        context.probe = Some(Arc::new(move |stage| match stage {
            WorkerStage::Running(hwnd) => {
                sender.send(hwnd).unwrap();
                control.record_stop();
            }
            WorkerStage::BodyExited(_) => {
                if callback {
                    control.record_callback_failure();
                } else {
                    RUNTIME_OUTCOME.with(|slot| {
                        *slot.borrow_mut() = Some(BorderWorkerResult::RuntimeFailed(
                            "late retained service error".into(),
                        ))
                    });
                }
            }
            _ => {}
        }));
        let worker = TestWorker::spawn(context);
        let hwnd = HWND(receiver.recv_timeout(Duration::from_secs(2)).unwrap() as _);
        let outcome = worker.join();
        if callback {
            assert_eq!(outcome.primary, BorderWorkerResult::CallbackFailed);
        } else {
            assert_eq!(
                outcome.primary,
                BorderWorkerResult::RuntimeFailed("late retained service error".into())
            );
        }
        assert!(outcome.cleanup_failures.is_empty());
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool()
        );
        assert!(
            unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(tracking.0)) }
                .as_bool()
        );
    }

    fn idle_animation_failure(panics: bool) -> BorderWorkerExit {
        use std::sync::atomic::{AtomicBool, Ordering};
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;

        struct TimerExitProbe {
            hwnd: isize,
            exited: std::sync::mpsc::Sender<bool>,
        }

        impl Drop for TimerExitProbe {
            fn drop(&mut self) {
                let alive = unsafe { IsWindow(Some(HWND(self.hwnd as _))) }.as_bool();
                let _ = self.exited.send(alive);
            }
        }

        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        let mut context = worker_context(tracking.0);
        let (sender, receiver) = std::sync::mpsc::channel();
        let (exited, exit_receiver) = std::sync::mpsc::channel();
        let injected = AtomicBool::new(false);
        context.probe = Some(Arc::new(move |stage| {
            if !matches!(stage, WorkerStage::IdleWait) || injected.swap(true, Ordering::SeqCst) {
                return;
            }
            let hwnd = BOUND_HWND.get();
            let exit_probe = TimerExitProbe {
                hwnd,
                exited: exited.clone(),
            };
            let timer =
                crate::anim_timer::AnimationTimer::spawn(Duration::from_millis(1), move || {
                    let _ = &exit_probe;
                    if panics {
                        panic!("injected idle animation panic");
                    }
                    Err("injected idle animation post failure".into())
                })
                .unwrap();
            BORDER_STATE.with(|state| {
                state
                    .borrow_mut()
                    .as_mut()
                    .unwrap()
                    .install_animation_timer_for_test(timer);
            });
            sender.send(hwnd).unwrap();
        }));
        let worker = TestWorker::spawn(context);
        let hwnd = HWND(receiver.recv_timeout(Duration::from_secs(2)).unwrap() as _);
        let outcome = worker.join();
        assert!(
            exit_receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            "timer outlived border HWND"
        );
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
        assert!(unsafe { IsWindow(Some(tracking.0)) }.as_bool());
        outcome
    }

    #[test]
    #[serial_test::serial]
    fn idle_animation_post_failure_is_classified_and_joined() {
        let outcome = idle_animation_failure(false);
        assert!(
            matches!(&outcome.primary, BorderWorkerResult::AnimationFailed(error) if error.contains("PostFailed") && error.contains("injected idle animation post failure")),
            "unexpected exit: {outcome:?}"
        );
        assert!(
            outcome.cleanup_failures.is_empty(),
            "unexpected cleanup: {outcome:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn idle_animation_panic_is_classified_and_joined() {
        let outcome = idle_animation_failure(true);
        assert!(
            matches!(&outcome.primary, BorderWorkerResult::AnimationFailed(error) if error.contains("Panicked")),
            "unexpected exit: {outcome:?}"
        );
        assert_eq!(
            outcome.cleanup_failures,
            vec!["animation worker panicked during cleanup"]
        );
    }

    #[test]
    #[serial_test::serial]
    fn worker_stops_while_initial_snapshot_gate_remains_held() {
        let tracking = tracking_window();
        let mut context = worker_context(tracking.0);
        let control = context.control.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(16);
        context.probe = Some(Arc::new(move |stage| {
            let _ = sender.try_send(stage.clone());
        }));
        let gate = crate::APP_STATE.appearance.lock().unwrap();
        let worker = TestWorker::spawn(context);
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            WorkerStage::SnapshotDeferred
        ));
        control.record_stop();
        let outcome = worker.join();
        assert_eq!(outcome.primary, BorderWorkerResult::Completed);
        assert!(outcome.cleanup_failures.is_empty());
        drop(gate);
    }

    #[test]
    #[serial_test::serial]
    fn native_preparation_retries_held_device_without_new_wake() {
        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        let mut context = worker_context(tracking.0);
        let control = context.control.clone();
        let (ready, stages) = std::sync::mpsc::channel();
        let deferred = std::sync::atomic::AtomicBool::new(false);
        context.probe = Some(Arc::new(move |stage| match stage {
            WorkerStage::Created(hwnd) => {
                ready.send((0, hwnd, 0)).unwrap();
            }
            WorkerStage::PreparationDeferred(hwnd)
                if !deferred.swap(true, std::sync::atomic::Ordering::AcqRel) =>
            {
                ready.send((1, hwnd, 0)).unwrap();
            }
            WorkerStage::Running(hwnd) => BORDER_STATE.with(|state| {
                ready
                    .send((
                        2,
                        hwnd,
                        state.borrow().as_ref().unwrap().observed_state_for_test().0,
                    ))
                    .unwrap();
            }),
            _ => {}
        }));
        let devices = crate::APP_STATE.directx_devices.write().unwrap();
        let worker = TestWorker::spawn(context);
        let created = stages.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(created.0, 0);
        let waiting = stages.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(waiting.0, 1);
        assert_eq!(created.1, waiting.1);
        assert!(
            stages.try_recv().is_err(),
            "worker published under held device snapshot"
        );
        drop(devices);
        let running = stages.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(running.0, 2);
        assert_eq!(running.1, created.1);
        assert_eq!(running.2, unsafe {
            windows::Win32::UI::HiDpi::GetDpiForWindow(tracking.0)
        });
        control.signal_stop().unwrap();
        let outcome = worker.join();
        assert_eq!(outcome.primary, BorderWorkerResult::Completed);
        assert!(outcome.cleanup_failures.is_empty());
        assert!(
            !unsafe {
                windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(HWND(created.1 as _)))
            }
            .as_bool()
        );
    }

    #[test]
    #[serial_test::serial]
    fn stop_wins_at_native_second_render_deadline() {
        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                tracking.0,
                None,
                -32000,
                -32000,
                100,
                100,
                windows::Win32::UI::WindowsAndMessaging::SWP_ASYNCWINDOWPOS
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER
                    | windows::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW,
            )
        }
        .unwrap();
        let mut context = worker_context(tracking.0);
        let control = context.control.clone();
        let (due, observed) = std::sync::mpsc::channel();
        context.probe = Some(Arc::new(move |stage| match stage {
            WorkerStage::Prepared(_) => BORDER_STATE.with(|state| {
                state
                    .borrow_mut()
                    .as_mut()
                    .unwrap()
                    .set_restore_delay_for_test(0)
            }),
            WorkerStage::SecondRenderDue => {
                let hwnd = BORDER_STATE
                    .with(|state| state.borrow().as_ref().unwrap().border_window.0.0 as isize);
                control.signal_stop().unwrap();
                due.send(hwnd).unwrap();
            }
            _ => {}
        }));
        let worker = TestWorker::spawn(context);
        let hwnd = observed.recv_timeout(Duration::from_secs(2)).unwrap();
        let outcome = worker.join();
        assert_eq!(outcome.primary, BorderWorkerResult::Completed);
        assert!(outcome.cleanup_failures.is_empty());
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(HWND(hwnd as _))) }
                .as_bool()
        );
        assert!(
            unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(tracking.0)) }
                .as_bool()
        );
    }

    #[test]
    #[serial_test::serial]
    fn controlled_service_failures_are_classified_and_clean_owned_resources() {
        use crate::window_border::{RuntimeStep, WindowBorder};
        crate::register_border_window_class().unwrap();
        for step in [
            RuntimeStep::Render,
            RuntimeStep::Position,
            RuntimeStep::Device,
            RuntimeStep::DrawerRecreation,
        ] {
            let tracking = tracking_window();
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
                    tracking.0,
                    None,
                    -32000,
                    -32000,
                    100,
                    100,
                    windows::Win32::UI::WindowsAndMessaging::SWP_ASYNCWINDOWPOS
                        | windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE
                        | windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER
                        | windows::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW,
                )
            }
            .unwrap();
            let mut context = worker_context(tracking.0);
            let (running, ready) = std::sync::mpsc::channel();
            context.probe = Some(Arc::new(move |stage| match stage {
                WorkerStage::Prepared(_) => BORDER_STATE.with(|state| {
                    state
                        .borrow_mut()
                        .as_mut()
                        .unwrap()
                        .set_restore_delay_for_test(0)
                }),
                WorkerStage::Running(hwnd) => {
                    WindowBorder::fail_runtime_for_test(step);
                    PENDING_WORK
                        .set(PENDING_WORK.get() | WORK_DISPLAY | WORK_GEOMETRY | WORK_RENDER);
                    running.send((hwnd, Instant::now())).unwrap();
                }
                _ => {}
            }));
            let worker = TestWorker::spawn(context);
            let (hwnd, failed_at) = ready.recv_timeout(Duration::from_secs(2)).unwrap();
            let outcome = worker.join();
            assert!(
                failed_at.elapsed() <= Duration::from_millis(200),
                "{step:?} exceeded two reconciliation intervals"
            );
            assert!(
                matches!(&outcome.primary, BorderWorkerResult::RuntimeFailed(error) if error.contains(&format!("injected runtime checkpoint {step:?}"))),
                "unexpected {step:?} outcome: {outcome:?}"
            );
            assert!(outcome.cleanup_failures.is_empty());
            assert!(
                !unsafe {
                    windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(HWND(hwnd as _)))
                }
                .as_bool()
            );
            assert!(
                unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(tracking.0)) }
                    .as_bool()
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn worker_cleans_hidden_window_while_publication_gate_remains_held() {
        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        let mut context = worker_context(tracking.0);
        let control = context.control.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(16);
        let (resume, paused) = std::sync::mpsc::channel();
        let paused = Mutex::new(paused);
        let probe_control = control.clone();
        context.probe = Some(Arc::new(move |stage| {
            if matches!(stage, WorkerStage::Created(_)) {
                return;
            }
            let _ = sender.try_send(stage.clone());
            if matches!(stage, WorkerStage::Prepared(_)) {
                while !probe_control.is_stopped() {
                    match paused
                        .lock()
                        .unwrap()
                        .recv_timeout(MAX_CONTROL_POLL_INTERVAL)
                    {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            }
        }));
        let worker = TestWorker::spawn(context);
        let stage = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        let hwnd = match stage {
            WorkerStage::Prepared(hwnd) => HWND(hwnd as _),
            other => panic!("unexpected stage {other:?}"),
        };
        let gate = crate::APP_STATE.appearance.lock().unwrap();
        resume.send(()).unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            WorkerStage::PublicationDeferred
        ));
        let devices = crate::APP_STATE.directx_devices.write().unwrap();
        control.signal_stop().unwrap();
        let outcome = worker.join();
        assert_eq!(outcome.primary, BorderWorkerResult::Completed);
        assert!(outcome.cleanup_failures.is_empty());
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool()
        );
        drop(devices);
        drop(gate);
    }

    #[test]
    #[serial_test::serial]
    fn deferred_publication_refreshes_global_width_and_preserves_focused_radius() {
        use crate::border_updates::IpcUpdate;
        use crate::config::{RadiusConfig, WidthConfig};
        struct RestoreWidth(WidthConfig);
        impl Drop for RestoreWidth {
            fn drop(&mut self) {
                let mut gate = crate::APP_STATE
                    .appearance
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                let mut config = crate::APP_STATE
                    .config
                    .write()
                    .unwrap_or_else(|poison| poison.into_inner());
                config.global.border_width = self.0;
                if let (Some(command), Some(revision)) =
                    (gate.next_command(), gate.config_revision.checked_add(1))
                {
                    gate.global_stamps.width = command;
                    gate.config_revision = revision;
                }
            }
        }
        crate::register_border_window_class().unwrap();
        let original = crate::APP_STATE.config.read().unwrap().global.border_width;
        let _restore = RestoreWidth(original);
        let width = if original == WidthConfig::new(11.0) {
            WidthConfig::new(12.0)
        } else {
            WidthConfig::new(11.0)
        };
        let radius = RadiusConfig::Custom(17.0);
        let tracking = tracking_window();
        let mut context = worker_context(tracking.0);
        let control = context.control.clone();
        let updates = context.updates.clone();
        let registry = context.registry.clone();
        let id = context.id;
        let (stage_sender, stages) = std::sync::mpsc::channel();
        let (resume, paused) = std::sync::mpsc::channel();
        let paused = Mutex::new(paused);
        let prepared_barrier_used = std::sync::atomic::AtomicBool::new(false);
        context.probe = Some(Arc::new(move |stage| {
            if matches!(stage, WorkerStage::Running(_)) {
                BORDER_STATE.with(|state| {
                    let state = state.borrow();
                    let config = state.as_ref().unwrap().prepared_config();
                    assert_eq!(config.width, width);
                    assert_eq!(config.radius, radius);
                });
            }
            if !matches!(stage, WorkerStage::Created(_)) {
                let _ = stage_sender.send(stage.clone());
            }
            if matches!(stage, WorkerStage::Prepared(_))
                && !prepared_barrier_used.swap(true, std::sync::atomic::Ordering::AcqRel)
            {
                paused
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
            }
        }));
        let worker = TestWorker::spawn(context);
        let hwnd = match stages.recv_timeout(Duration::from_secs(2)).unwrap() {
            WorkerStage::Prepared(hwnd) => HWND(hwnd as _),
            other => panic!("unexpected initial stage {other:?}"),
        };
        let mut gate = crate::APP_STATE.appearance.lock().unwrap();
        let focused = gate.next_command().unwrap();
        updates.enqueue(focused, IpcUpdate::Radius(radius)).unwrap();
        let global = gate.next_command().unwrap();
        let revision = gate.config_revision.checked_add(1).unwrap();
        crate::APP_STATE.config.write().unwrap().global.border_width = width;
        gate.global_stamps.width = global;
        gate.config_revision = revision;
        resume.send(()).unwrap();
        assert!(matches!(
            stages.recv_timeout(Duration::from_secs(2)).unwrap(),
            WorkerStage::PublicationDeferred
        ));
        assert_eq!(
            registry.lock().unwrap().entries[&id].prepared_config_revision,
            None
        );
        drop(gate);
        let mut refreshed = false;
        loop {
            match stages.recv_timeout(Duration::from_secs(2)).unwrap() {
                WorkerStage::Prepared(prepared) => {
                    assert_eq!(prepared, hwnd.0 as isize);
                    refreshed = true;
                }
                WorkerStage::Running(running) => {
                    assert_eq!(running, hwnd.0 as isize);
                    break;
                }
                _ => {}
            }
        }
        assert!(refreshed);
        assert_eq!(
            registry.lock().unwrap().entries[&id].prepared_config_revision,
            Some(revision)
        );
        control.signal_stop().unwrap();
        let outcome = worker.join();
        assert_eq!(outcome.primary, BorderWorkerResult::Completed);
        assert!(outcome.cleanup_failures.is_empty());
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool()
        );
    }

    #[test]
    #[serial_test::serial]
    fn real_socket_setters_reach_owned_border_configuration() {
        use crate::colors::ColorBrushConfig;
        use crate::config::{OffsetConfig, RadiusConfig, WidthConfig};
        use crate::iocp::{UnixListener, UnixStream};
        use std::io::{BufRead, BufReader, Write};
        struct RestoreState {
            registry: BorderRegistry,
            width: WidthConfig,
            radius: RadiusConfig,
            offset: OffsetConfig,
            active: ColorBrushConfig,
            inactive: ColorBrushConfig,
            socket: std::path::PathBuf,
        }
        impl Drop for RestoreState {
            fn drop(&mut self) {
                *crate::APP_STATE.border_registry.lock().unwrap() =
                    std::mem::take(&mut self.registry);
                let mut gate = crate::APP_STATE.appearance.lock().unwrap();
                let command = gate.next_command().unwrap();
                let mut config = crate::APP_STATE.config.write().unwrap();
                config.global.border_width = self.width;
                config.global.border_radius = self.radius;
                config.global.border_offset = self.offset;
                config.global.active_color = self.active.clone();
                config.global.inactive_color = self.inactive.clone();
                gate.global_stamps.width = command;
                gate.global_stamps.radius = command;
                gate.global_stamps.offset = command;
                gate.global_stamps.active = command;
                gate.global_stamps.inactive = command;
                gate.config_revision = gate.config_revision.checked_add(1).unwrap();
                let _ = std::fs::remove_file(&self.socket);
            }
        }
        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        let mut context = worker_context(tracking.0);
        let previous_registry = std::mem::replace(
            &mut *crate::APP_STATE.border_registry.lock().unwrap(),
            std::mem::take(&mut *context.registry.lock().unwrap()),
        );
        context.registry = crate::APP_STATE.border_registry.clone();
        let original = crate::APP_STATE.config.read().unwrap();
        let restore = RestoreState {
            registry: previous_registry,
            width: original.global.border_width,
            radius: original.global.border_radius,
            offset: original.global.border_offset,
            active: original.global.active_color.clone(),
            inactive: original.global.inactive_color.clone(),
            socket: std::env::temp_dir().join(format!("fix01-ipc-{}.sock", std::process::id())),
        };
        drop(original);
        let width = WidthConfig::new(13.0);
        let radius = RadiusConfig::Custom(19.0);
        let offset: OffsetConfig = serde_json::from_value(serde_json::json!(7)).unwrap();
        let active = ColorBrushConfig::Solid("#12ab34".into());
        let inactive = ColorBrushConfig::Solid("#56cd78".into());
        let (ready, running) = std::sync::mpsc::channel();
        let (observed, snapshots) = std::sync::mpsc::channel();
        context.probe = Some(Arc::new(move |stage| match stage {
            WorkerStage::Running(hwnd) => {
                ready.send(hwnd).unwrap();
            }
            WorkerStage::IdleWait => {
                BORDER_STATE.with(|state| {
                    let state = state.borrow();
                    let config = state.as_ref().unwrap().prepared_config();
                    let _ = observed.send(config.clone());
                });
            }
            _ => {}
        }));
        let control = context.control.clone();
        let worker = TestWorker::spawn(context);
        let hwnd = HWND(running.recv_timeout(Duration::from_secs(2)).unwrap() as _);
        let listener = UnixListener::bind(&restore.socket).unwrap();
        std::thread::scope(|scope| {
            let server =
                scope.spawn(move || crate::ipc::handle_test_client(listener.accept().unwrap()));
            let stream = UnixStream::connect(&restore.socket).unwrap();
            let mut reader = BufReader::new(&stream);
            for command in [
                crate::ipc::IpcCommand::SetRadius {
                    radius,
                    focused: false,
                },
                crate::ipc::IpcCommand::SetWidth {
                    width,
                    focused: false,
                },
                crate::ipc::IpcCommand::SetOffset {
                    offset,
                    focused: false,
                },
                crate::ipc::IpcCommand::SetColor {
                    active: Some(active.clone()),
                    inactive: None,
                    focused: false,
                },
                crate::ipc::IpcCommand::SetColor {
                    active: None,
                    inactive: Some(inactive.clone()),
                    focused: false,
                },
            ] {
                writeln!(&stream, "{}", serde_json::to_string(&command).unwrap()).unwrap();
                let mut response = String::new();
                reader.read_line(&mut response).unwrap();
                let response: serde_json::Value = serde_json::from_str(&response).unwrap();
                assert_eq!(response["ok"], true, "socket command failed: {response}");
            }
            drop(reader);
            drop(stream);
            server.join().unwrap().unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let actual = snapshots
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if actual.width == width
                && actual.radius == radius
                && actual.offset == offset
                && actual.active_color == active
                && actual.inactive_color == inactive
            {
                break;
            }
        }
        control.signal_stop().unwrap();
        let outcome = worker.join();
        assert_eq!(outcome.primary, BorderWorkerResult::Completed);
        assert!(outcome.cleanup_failures.is_empty());
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool()
        );
    }

    #[test]
    #[serial_test::serial]
    fn hidden_preparation_panic_cleans_hwnd_without_terminal_registry_mutation() {
        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        let mut context = worker_context(tracking.0);
        let registry = context.registry.clone();
        let id = context.id;
        let (prepared, ready) = std::sync::mpsc::channel();
        context.probe = Some(Arc::new(move |stage| {
            if let WorkerStage::Prepared(hwnd) = stage {
                prepared.send(hwnd).unwrap();
                panic!("injected worker panic after hidden preparation");
            }
        }));
        let worker = TestWorker::spawn(context);
        let hwnd = HWND(ready.recv_timeout(Duration::from_secs(2)).unwrap() as _);
        let outcome = worker.join();
        assert_eq!(outcome.primary, BorderWorkerResult::Panicked);
        assert!(outcome.cleanup_failures.is_empty());
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool()
        );
        let registry = registry.lock().unwrap();
        assert_eq!(
            registry.entries[&id].phase,
            crate::border_registry::BorderPhase::Starting
        );
        assert_eq!(
            registry.current_by_hwnd.get(&(tracking.0.0 as isize)),
            Some(&id)
        );
    }

    #[test]
    #[serial_test::serial]
    fn startup_checkpoint_failures_release_acquired_native_resources() {
        use crate::window_border::PreparationStep;
        crate::register_border_window_class().unwrap();
        for step in [
            PreparationStep::Geometry,
            PreparationStep::CompositionAttributes,
            PreparationStep::Drawer,
        ] {
            let tracking = tracking_window();
            let mut context = worker_context(tracking.0);
            let (created, ready) = std::sync::mpsc::channel();
            context.probe = Some(Arc::new(move |stage| {
                if let WorkerStage::Created(hwnd) = stage {
                    WindowBorder::fail_preparation_for_test(step);
                    created.send(hwnd).unwrap();
                }
                assert!(
                    !matches!(stage, WorkerStage::Running(_)),
                    "failed startup became Running"
                );
            }));
            let worker = TestWorker::spawn(context);
            let hwnd = HWND(ready.recv_timeout(Duration::from_secs(2)).unwrap() as _);
            let outcome = worker.join();
            assert!(
                matches!(&outcome.primary, BorderWorkerResult::InitializationFailed(error) if error.contains(&format!("injected startup checkpoint {step:?}"))),
                "unexpected startup outcome: {outcome:?}"
            );
            assert!(
                outcome.cleanup_failures.is_empty(),
                "unexpected cleanup: {outcome:?}"
            );
            assert!(
                !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool()
            );
            assert!(
                unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(tracking.0)) }
                    .as_bool()
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn stop_interrupts_prepared_initialization_delay_without_activation() {
        struct RestoreDelay(u64);
        impl Drop for RestoreDelay {
            fn drop(&mut self) {
                crate::APP_STATE.get_config_mut().global.initialize_delay = self.0;
            }
        }
        crate::register_border_window_class().unwrap();
        let _restore = RestoreDelay(std::mem::replace(
            &mut crate::APP_STATE.get_config_mut().global.initialize_delay,
            60_000,
        ));
        let tracking = tracking_window();
        let mut context = worker_context(tracking.0);
        context.initial = false;
        let control = context.control.clone();
        let (prepared, ready) = std::sync::mpsc::channel();
        context.probe = Some(Arc::new(move |stage| {
            if let WorkerStage::Prepared(hwnd) = stage {
                BORDER_STATE.with(|state| {
                    assert_eq!(
                        state
                            .borrow()
                            .as_ref()
                            .unwrap()
                            .prepared_config()
                            .initialize_delay,
                        60_000
                    )
                });
                prepared.send(hwnd).unwrap();
            }
            assert!(
                !matches!(stage, WorkerStage::Running(_)),
                "stop did not prevent activation"
            );
        }));
        let worker = TestWorker::spawn(context);
        let hwnd = HWND(ready.recv_timeout(Duration::from_secs(2)).unwrap() as _);
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd) }.as_bool()
        );
        control.signal_stop().unwrap();
        let outcome = worker.join();
        assert_eq!(outcome.primary, BorderWorkerResult::Completed);
        assert!(outcome.cleanup_failures.is_empty());
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool()
        );
    }

    #[test]
    #[serial_test::serial]
    fn stop_interrupts_native_restore_delay() {
        use windows::Win32::UI::WindowsAndMessaging::{
            SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE,
            SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos, ShowWindow,
        };
        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        unsafe {
            SetWindowPos(
                tracking.0,
                None,
                -32000,
                -32000,
                100,
                100,
                SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
            )
        }
        .unwrap();
        let mut context = worker_context(tracking.0);
        let control = context.control.clone();
        let tracking_hwnd = tracking.0.0 as isize;
        let (phases, ready) = std::sync::mpsc::channel();
        context.probe = Some(Arc::new(move |stage| match stage {
            WorkerStage::Prepared(_) => BORDER_STATE.with(|state| {
                state
                    .borrow_mut()
                    .as_mut()
                    .unwrap()
                    .set_restore_delay_for_test(60_000)
            }),
            WorkerStage::Running(hwnd) => {
                phases.send((0, hwnd)).unwrap();
            }
            WorkerStage::IdleWait => BORDER_STATE.with(|state| {
                let state = state.borrow();
                let border = state.as_ref().unwrap();
                let phase = if crate::utils::is_window_minimized(HWND(tracking_hwnd as _)) {
                    Some(1)
                } else if border.restore_deadline().is_some() {
                    Some(2)
                } else {
                    None
                };
                if let Some(phase) = phase {
                    let _ = phases.send((phase, border.border_window.0.0 as isize));
                }
            }),
            _ => {}
        }));
        let worker = TestWorker::spawn(context);
        let hwnd = HWND(ready.recv_timeout(Duration::from_secs(2)).unwrap().1 as _);
        unsafe {
            let _ = ShowWindow(tracking.0, SW_SHOWMINNOACTIVE);
        }
        loop {
            if ready.recv_timeout(Duration::from_secs(2)).unwrap().0 == 1 {
                break;
            }
        }
        unsafe {
            let _ = ShowWindow(tracking.0, SW_SHOWNOACTIVATE);
        }
        loop {
            if ready.recv_timeout(Duration::from_secs(2)).unwrap().0 == 2 {
                break;
            }
        }
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd) }.as_bool()
        );
        control.signal_stop().unwrap();
        let outcome = worker.join();
        assert_eq!(outcome.primary, BorderWorkerResult::Completed);
        assert!(outcome.cleanup_failures.is_empty());
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool()
        );
    }

    #[test]
    #[serial_test::serial]
    fn forged_hints_use_native_authority_after_reentrant_state_borrow() {
        use windows::Win32::UI::HiDpi::GetDpiForWindow;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowRect, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW,
            SetWindowPos,
        };
        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        unsafe {
            SetWindowPos(
                tracking.0,
                None,
                -32000,
                -32000,
                100,
                100,
                SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
            )
        }
        .unwrap();
        let mut context = worker_context(tracking.0);
        let control = context.control.clone();
        let tracking_hwnd = tracking.0.0 as isize;
        let (ready, running) = std::sync::mpsc::channel();
        let (observed, snapshots) = std::sync::mpsc::channel();
        context.probe = Some(Arc::new(move |stage| match stage {
            WorkerStage::Prepared(_) => BORDER_STATE.with(|state| {
                state
                    .borrow_mut()
                    .as_mut()
                    .unwrap()
                    .set_restore_delay_for_test(0)
            }),
            WorkerStage::Running(hwnd) => {
                let hwnd = HWND(hwnd as _);
                let mut before = windows::Win32::Foundation::RECT::default();
                unsafe { GetWindowRect(hwnd, &mut before) }.unwrap();
                BORDER_STATE.with(|state| {
                    let _borrow = state.borrow_mut();
                    assert_eq!(
                        window_proc(hwnd, WM_DPICHANGED, WPARAM(0x7777_7777), LPARAM(1)).0,
                        0
                    );
                    assert_eq!(
                        window_proc(
                            hwnd,
                            WM_APP_FOREGROUND,
                            WPARAM(tracking_hwnd as usize),
                            LPARAM(1)
                        )
                        .0,
                        0
                    );
                    assert_eq!(
                        window_proc(hwnd, WM_APP_LOCATIONCHANGE, WPARAM(usize::MAX), LPARAM(1)).0,
                        0
                    );
                });
                ready.send((hwnd.0 as isize, before)).unwrap();
            }
            WorkerStage::IdleWait => BORDER_STATE.with(|state| {
                let state = state.borrow();
                let border = state.as_ref().unwrap();
                let mut rect = windows::Win32::Foundation::RECT::default();
                unsafe { GetWindowRect(border.border_window.0, &mut rect) }.unwrap();
                let _ = observed.send((border.observed_state_for_test(), rect));
            }),
            _ => {}
        }));
        let worker = TestWorker::spawn(context);
        let (hwnd, before) = running.recv_timeout(Duration::from_secs(2)).unwrap();
        let ((dpi, state), after) = snapshots.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(dpi, unsafe { GetDpiForWindow(tracking.0) });
        assert_eq!(state, crate::window_border::WindowState::Inactive);
        assert_eq!(
            (after.left, after.top, after.right, after.bottom),
            (before.left, before.top, before.right, before.bottom)
        );
        control.signal_stop().unwrap();
        let outcome = worker.join();
        assert_eq!(outcome.primary, BorderWorkerResult::Completed);
        assert!(outcome.cleanup_failures.is_empty());
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(HWND(hwnd as _))) }
                .as_bool()
        );
    }

    #[test]
    #[serial_test::serial]
    fn idle_worker_stops_when_every_stop_signal_fails() {
        use windows::Win32::Foundation::{E_FAIL, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::WaitForSingleObject;
        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        let mut context = worker_context(tracking.0);
        let control = context.control.clone();
        let (stage_sender, stages) = std::sync::mpsc::channel();
        let (resume, paused) = std::sync::mpsc::channel();
        let paused = Mutex::new(paused);
        let wait_barrier_used = std::sync::atomic::AtomicBool::new(false);
        context.probe = Some(Arc::new(move |stage| {
            let _ = stage_sender.send(stage.clone());
            if matches!(stage, WorkerStage::IdleWait)
                && !wait_barrier_used.swap(true, std::sync::atomic::Ordering::AcqRel)
            {
                paused
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
            }
        }));
        let worker = TestWorker::spawn(context);
        let mut hwnd = None;
        loop {
            match stages.recv_timeout(Duration::from_secs(2)).unwrap() {
                WorkerStage::Running(window) => hwnd = Some(HWND(window as _)),
                WorkerStage::IdleWait => break,
                _ => {}
            }
        }
        let hwnd = hwnd.expect("worker did not publish before idle wait");
        for _ in 0..3 {
            let error = control.signal_stop_with(|| Err(windows::core::Error::from(E_FAIL)));
            assert_eq!(error.unwrap_err().code(), E_FAIL);
            assert!(control.is_stopped());
            assert_eq!(
                unsafe { WaitForSingleObject(control.stop_event.handle(), 0) },
                WAIT_TIMEOUT
            );
        }
        resume.send(()).unwrap();
        let outcome = worker.join();
        assert_eq!(outcome.primary, BorderWorkerResult::Completed);
        assert!(outcome.cleanup_failures.is_empty());
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool()
        );
    }

    #[test]
    #[serial_test::serial]
    fn running_worker_stops_during_posted_message_flood() {
        stop_under_flood(0);
    }

    #[test]
    #[serial_test::serial]
    fn running_worker_stops_during_sent_message_flood() {
        stop_under_flood(1);
    }

    #[test]
    #[serial_test::serial]
    fn running_worker_stops_during_update_wakes_with_held_mailbox() {
        stop_under_flood(2);
    }

    fn stop_under_flood(mode: u8) {
        use windows::Win32::UI::WindowsAndMessaging::{SMTO_ABORTIFHUNG, SendMessageTimeoutW};
        crate::register_border_window_class().unwrap();
        let tracking = tracking_window();
        let mut context = worker_context(tracking.0);
        let control = context.control.clone();
        let updates = context.updates.clone();
        let (stage_sender, stages) = std::sync::mpsc::channel();
        context.probe = Some(Arc::new(move |stage| {
            let _ = stage_sender.send(stage);
        }));
        let worker = TestWorker::spawn(context);
        let hwnd = loop {
            if let WorkerStage::Running(hwnd) = stages.recv_timeout(Duration::from_secs(2)).unwrap()
            {
                break hwnd;
            }
        };
        let flood_control = control.clone();
        let flood_updates = updates.clone();
        let (started, starting) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let sender = scope.spawn(move || {
                for index in 0u64.. {
                    if flood_control.is_stopped() {
                        break;
                    }
                    match mode {
                        0 => {
                            let _ = crate::utils::post_message_w(
                                Some(HWND(hwnd as _)),
                                WM_APP_REORDER,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                        1 => unsafe {
                            let _ = SendMessageTimeoutW(
                                HWND(hwnd as _),
                                WM_APP_LOCATIONCHANGE,
                                WPARAM(0),
                                LPARAM(0),
                                SMTO_ABORTIFHUNG,
                                100,
                                None,
                            );
                        },
                        _ => {
                            let _ = flood_updates.wake_event.signal();
                        }
                    }
                    if index == 64 {
                        let _ = started.send(());
                    }
                }
            });
            starting.recv_timeout(Duration::from_secs(2)).unwrap();
            let mut outcome = None;
            updates.while_locked(|| {
                control.record_stop();
                outcome = Some(worker.join());
            });
            sender.join().unwrap();
            let outcome = outcome.unwrap();
            assert_eq!(outcome.primary, BorderWorkerResult::Completed);
            assert!(outcome.cleanup_failures.is_empty());
        });
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(HWND(hwnd as _))) }
                .as_bool()
        );
    }

    #[test]
    #[serial_test::serial]
    fn construction_panic_is_contained_and_failed_native_handle_cleared() {
        crate::register_border_window_class().unwrap();
        let control = Arc::new(BorderControl::new().unwrap());
        assert!(bind_control(control.clone()));
        CREATED_HWND.set(0);
        PANIC_MESSAGE.set(Some(WM_CREATE));
        assert!(WindowBorder::new(HWND::default()).is_err());
        assert!(control.callback_failed());
        assert!(control.is_stopped());
        let hwnd = HWND(CREATED_HWND.get() as _);
        assert!(!hwnd.is_invalid());
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool()
        );
        assert_eq!(BOUND_HWND.get(), 0);
        assert!(take_border().is_none());
    }

    #[test]
    #[serial_test::serial]
    fn construction_missing_state_fails_without_native_destruction() {
        construction_state_unavailable(false);
    }

    #[test]
    #[serial_test::serial]
    fn construction_borrow_conflict_fails_without_native_destruction() {
        construction_state_unavailable(true);
    }

    fn construction_state_unavailable(borrowed: bool) {
        crate::register_border_window_class().unwrap();
        for (message, expected) in [(WM_NCCREATE, LRESULT(0)), (WM_CREATE, LRESULT(-1))] {
            let control = Arc::new(BorderControl::new().unwrap());
            assert!(bind_control(control.clone()));
            let mut border = Some(WindowBorder::new(HWND::default()).unwrap());
            let hwnd = border.as_ref().unwrap().border_window.0;
            if borrowed {
                install_border(border.take().unwrap()).unwrap();
            }
            let bound = if message == WM_NCCREATE {
                0
            } else {
                hwnd.0 as isize
            };
            BOUND_HWND.set(bound);
            BINDING_PHASE.set(BindingPhase::Constructing);
            let result = if borrowed {
                BORDER_STATE.with(|state| {
                    let _guard = state.borrow_mut();
                    window_proc(hwnd, message, WPARAM(0), LPARAM(1))
                })
            } else {
                window_proc(hwnd, message, WPARAM(0), LPARAM(1))
            };
            assert_eq!(result, expected);
            assert!(control.callback_failed());
            assert!(control.is_stopped());
            assert_eq!(BOUND_HWND.get(), bound);
            assert!(
                unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool()
            );
            drop(border);
            drop(take_border());
            assert!(
                !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool()
            );
            assert_eq!(BOUND_HWND.get(), 0);
            BINDING_PHASE.set(BindingPhase::Empty);
        }
    }

    #[test]
    #[serial_test::serial]
    fn callback_panic_and_panicking_payload_cannot_destroy_live_window() {
        crate::register_border_window_class().unwrap();
        let control = Arc::new(BorderControl::new().unwrap());
        assert!(bind_control(control.clone()));
        let border = WindowBorder::new(HWND::default()).unwrap();
        let hwnd = border.border_window.0;
        PANIC_MESSAGE.set(Some(WM_DPICHANGED));
        let result = window_proc(hwnd, WM_DPICHANGED, WPARAM(usize::MAX), LPARAM(1));
        assert_eq!(result, LRESULT(0));
        assert!(control.callback_failed());
        assert!(control.is_stopped());
        assert!(unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool());
        drop(border);
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool()
        );
    }

    #[test]
    #[serial_test::serial]
    fn forged_construction_and_destroy_notifications_do_not_rebind() {
        crate::register_border_window_class().unwrap();
        assert!(bind_control(Arc::new(BorderControl::new().unwrap())));
        let border = WindowBorder::new(HWND::default()).unwrap();
        let hwnd = border.border_window.0;
        assert_eq!(
            window_proc(hwnd, WM_NCCREATE, WPARAM(0), LPARAM(1)),
            LRESULT(0)
        );
        assert_eq!(
            window_proc(hwnd, WM_CREATE, WPARAM(0), LPARAM(1)),
            LRESULT(-1)
        );
        window_proc(hwnd, WM_NCDESTROY, WPARAM(0), LPARAM(1));
        assert_eq!(BOUND_HWND.get(), hwnd.0 as isize);
        assert!(WindowBorder::new(HWND::default()).is_err());
        assert!(unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool());
        drop(border);
    }

    #[test]
    #[serial_test::serial]
    fn resource_deferral_retains_hints_without_actionable_spin() {
        PENDING_WORK.set(0);
        DEFERRED_WORK.set(0);
        defer_pending_work(WORK_DISPLAY | WORK_KOMOREBI);
        assert_eq!(take_pending_work(), 0);
        assert_eq!(DEFERRED_WORK.get(), WORK_DISPLAY | WORK_KOMOREBI);
        let deadline = RESOURCE_RETRY_AT.get().unwrap();
        defer_pending_work(WORK_DISPLAY);
        assert_eq!(RESOURCE_RETRY_AT.get(), Some(deadline));
        assert!(deadline.saturating_duration_since(Instant::now()) <= LOCK_RETRY_INTERVAL);
        assert!(deadline > Instant::now());
        DEFERRED_WORK.set(0);
        RESOURCE_RETRY_AT.set(None);
    }

    #[test]
    #[serial_test::serial]
    fn private_dispatch_ignores_public_pointers_and_close() {
        use windows::Win32::UI::WindowsAndMessaging::{
            GWLP_USERDATA, IsWindow, SendMessageW, SetWindowLongPtrW,
        };
        crate::register_border_window_class().unwrap();
        assert!(bind_control(Arc::new(BorderControl::new().unwrap())));
        let border = WindowBorder::new(HWND::default()).unwrap();
        let hwnd = border.border_window.0;
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 1);
        }
        for message in [
            WM_APP_SET_COLORS,
            WM_APP_SET_WIDTH,
            WM_APP_SET_OFFSET,
            WM_APP_SET_RADIUS,
            WM_CLOSE,
            WM_TIMER,
        ] {
            unsafe {
                SendMessageW(hwnd, message, Some(WPARAM(usize::MAX)), Some(LPARAM(1)));
            }
        }
        unsafe {
            SendMessageW(
                hwnd,
                WM_SYSCOMMAND,
                Some(WPARAM(SC_CLOSE as usize)),
                Some(LPARAM(1)),
            );
        }
        assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
        assert!(!callback_failed());
        drop(border);
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
    }

    #[test]
    #[serial_test::serial]
    fn hints_survive_a_borrowed_renderer_state() {
        crate::register_border_window_class().unwrap();
        let border = WindowBorder::new(HWND::default()).unwrap();
        let hwnd = border.border_window.0;
        assert!(bind_control(Arc::new(BorderControl::new().unwrap())));
        install_border(border).unwrap();
        BORDER_STATE.with(|state| {
            let _guard = state.borrow_mut();
            window_proc(hwnd, WM_DPICHANGED, WPARAM(0), LPARAM(1));
            window_proc(hwnd, WM_APP_LOCATIONCHANGE, WPARAM(0), LPARAM(0));
            window_proc(HWND(usize::MAX as _), WM_APP_ANIMATE, WPARAM(0), LPARAM(0));
        });
        let bits = take_pending_work();
        assert_eq!(
            bits & (WORK_DPI | WORK_GEOMETRY | WORK_RENDER),
            WORK_DPI | WORK_GEOMETRY | WORK_RENDER
        );
        assert_eq!(bits & WORK_ANIMATION, 0);
        assert!(!callback_failed());
        drop(take_border());
    }

    #[test]
    #[serial_test::serial]
    fn hints_preserve_every_mapping_across_borrow_and_binding_phases() {
        crate::register_border_window_class().unwrap();
        let control = Arc::new(BorderControl::new().unwrap());
        assert!(bind_control(control.clone()));
        let border = WindowBorder::new(HWND::default()).unwrap();
        let hwnd = border.border_window.0;
        install_border(border).unwrap();
        let hints = [
            (WM_APP_LOCATIONCHANGE, WORK_GEOMETRY),
            (WM_SIZE, WORK_GEOMETRY),
            (WM_APP_SHOWUNCLOAKED, WORK_VISIBILITY),
            (WM_APP_HIDECLOAKED, WORK_VISIBILITY),
            (WM_APP_MINIMIZESTART, WORK_VISIBILITY),
            (WM_APP_MINIMIZEEND, WORK_VISIBILITY),
            (WM_APP_REORDER, WORK_Z_ORDER),
            (WM_APP_FOREGROUND, WORK_RENDER),
            (WM_APP_KOMOREBI, WORK_KOMOREBI),
            (WM_DPICHANGED, WORK_DPI | WORK_GEOMETRY | WORK_RENDER),
            (WM_DISPLAYCHANGE, WORK_DISPLAY | WORK_GEOMETRY | WORK_RENDER),
            (WM_DEVICECHANGE, WORK_DISPLAY | WORK_GEOMETRY | WORK_RENDER),
            (
                WM_APP_RECREATE_DRAWER,
                WORK_DISPLAY | WORK_GEOMETRY | WORK_RENDER,
            ),
            (
                WM_POWERBROADCAST,
                WORK_DISPLAY | WORK_GEOMETRY | WORK_RENDER,
            ),
            (WM_APP_ANIMATE, WORK_ANIMATION | WORK_RENDER),
            (WM_PAINT, WORK_RENDER),
        ];
        BORDER_STATE.with(|state| {
            let _guard = state.borrow_mut();
            for phase in [BindingPhase::Constructing, BindingPhase::Live] {
                BINDING_PHASE.set(phase);
                for (message, bits) in hints {
                    PENDING_WORK.set(0);
                    assert_eq!(
                        window_proc(hwnd, message, WPARAM(usize::MAX), LPARAM(1)),
                        LRESULT(0)
                    );
                    assert_eq!(take_pending_work(), bits);
                    window_proc(hwnd, message, WPARAM(0), LPARAM(0));
                    assert_eq!(take_pending_work(), bits);
                }
            }
            control.record_stop();
            for (message, _) in hints {
                window_proc(hwnd, message, WPARAM(usize::MAX), LPARAM(1));
                assert_eq!(take_pending_work(), 0);
            }
            BINDING_PHASE.set(BindingPhase::Destroying);
            for (message, _) in hints {
                window_proc(hwnd, message, WPARAM(usize::MAX), LPARAM(1));
                assert_eq!(take_pending_work(), 0);
            }
            assert!(!callback_failed());
        });
        drop(take_border());
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool()
        );
    }

    #[test]
    fn restore_delay_is_scheduled_once_and_cancelled_by_hide() {
        let start = Instant::now();
        let mut visibility = VisibilityTiming::default();
        let delay = Duration::from_millis(250);
        assert_eq!(
            visibility.observe(start, true, true, delay).unwrap(),
            VisibilityAction::Hidden
        );
        assert_eq!(
            visibility.observe(start, true, false, delay).unwrap(),
            VisibilityAction::Waiting
        );
        let deadline = visibility.deadline().unwrap();
        for offset in [10, 100, 200] {
            assert_eq!(
                visibility
                    .observe(
                        start + Duration::from_millis(offset),
                        true,
                        false,
                        Duration::from_secs(60)
                    )
                    .unwrap(),
                VisibilityAction::Waiting
            );
            assert_eq!(visibility.deadline(), Some(deadline));
        }
        assert_eq!(
            visibility.observe(deadline, true, false, delay).unwrap(),
            VisibilityAction::Visible
        );
        assert!(visibility.deadline().is_none());
        assert_eq!(
            visibility.observe(deadline, true, false, delay).unwrap(),
            VisibilityAction::Visible
        );
        visibility.observe(deadline, true, true, delay).unwrap();
        visibility.observe(deadline, true, false, delay).unwrap();
        assert_eq!(
            visibility.observe(deadline, false, false, delay).unwrap(),
            VisibilityAction::Hidden
        );
        assert!(visibility.deadline().is_none());
        assert_eq!(
            visibility.observe(deadline, true, false, delay).unwrap(),
            VisibilityAction::Visible
        );
    }

    #[test]
    fn queued_timer_callbacks_never_reach_native_dispatch() {
        let timer = MSG {
            message: WM_TIMER,
            lParam: LPARAM(1),
            ..Default::default()
        };
        assert!(!queued_message_allowed(&timer));
        let hint = MSG {
            message: WM_APP_ANIMATE,
            ..Default::default()
        };
        assert!(queued_message_allowed(&hint));
    }

    #[test]
    fn zero_default_and_long_delays_get_one_allowance() {
        let start = Instant::now();
        for delay_ms in [0, 250, 60_000] {
            let mut timing = StartupTiming::new(start, STARTUP_WORK_BUDGET).unwrap();
            let capture = start + Duration::from_secs(1);
            timing.capture_delay(capture, delay_ms).unwrap();
            let preparation = start + Duration::from_secs(2);
            timing.prepared(preparation).unwrap();
            assert_eq!(
                timing.hard_deadline(),
                start + STARTUP_WORK_BUDGET + Duration::from_millis(delay_ms)
            );
            assert_eq!(
                timing.activation_at(),
                Some(preparation + Duration::from_millis(delay_ms))
            );
            assert!(timing.ready(timing.activation_at().unwrap()));
        }
    }

    #[test]
    fn snapshot_contention_cannot_grant_delay_after_budget() {
        let start = Instant::now();
        let mut timing = StartupTiming::new(start, STARTUP_WORK_BUDGET).unwrap();
        assert_eq!(
            timing.capture_delay(start + STARTUP_WORK_BUDGET, 60_000),
            Err(TimingError::TimedOut)
        );
        assert_eq!(timing.hard_deadline(), start + STARTUP_WORK_BUDGET);
        assert!(timing.activation_at().is_none());
    }

    #[test]
    fn revision_churn_does_not_extend_or_reschedule() {
        let start = Instant::now();
        let mut timing = StartupTiming::new(start, STARTUP_WORK_BUDGET).unwrap();
        timing.capture_delay(start, 250).unwrap();
        timing.prepared(start + Duration::from_secs(1)).unwrap();
        let activation = timing.activation_at();
        let hard = timing.hard_deadline();
        for revision in 2..10 {
            let now = start + Duration::from_secs(revision);
            timing.capture_delay(now, 60_000).unwrap();
            timing.prepared(now).unwrap();
            assert_eq!(timing.activation_at(), activation);
            assert_eq!(timing.hard_deadline(), hard);
        }
        assert_eq!(timing.check(hard), Err(TimingError::TimedOut));
    }

    #[test]
    fn long_wait_is_capped_and_never_overrides_stop_control() {
        let start = Instant::now();
        let mut timing = StartupTiming::new(start, STARTUP_WORK_BUDGET).unwrap();
        timing.capture_delay(start, 60_000).unwrap();
        timing.prepared(start).unwrap();
        assert_eq!(timing.wait_duration(start), MAX_CONTROL_POLL_INTERVAL);
        let control = crate::border_registry::BorderControl::new().unwrap();
        control.record_stop();
        assert!(control.is_stopped());
        assert!(!timing.ready(start));
    }

    #[test]
    fn checked_arithmetic_fails_without_clipping() {
        let start = Instant::now();
        assert!(matches!(
            StartupTiming::new(start, Duration::MAX),
            Err(TimingError::Unrepresentable)
        ));
        let mut timing = StartupTiming::new(start, STARTUP_WORK_BUDGET).unwrap();
        let original = timing.hard_deadline();
        let mut overflowing = StartupTiming::new(start, STARTUP_WORK_BUDGET).unwrap();
        overflowing.work_budget = Duration::MAX;
        assert_eq!(
            overflowing.capture_delay(start, 1),
            Err(TimingError::Unrepresentable)
        );
        assert_eq!(overflowing.hard_deadline(), original);
        timing.prepared(start).unwrap_err();
    }

    #[test]
    fn deferred_flags_wait_instead_of_contention_spin() {
        let start = Instant::now();
        let mut deferred = DeferredOperations::default();
        deferred.defer(DeferredOperation::Mailbox, start).unwrap();
        assert!(!deferred.is_due(DeferredOperation::Mailbox, start));
        assert!(deferred.is_due(DeferredOperation::Snapshot, start));
        assert_eq!(deferred.next_wait(start), LOCK_RETRY_INTERVAL);
        assert!(deferred.is_due(DeferredOperation::Mailbox, start + LOCK_RETRY_INTERVAL));
        deferred.complete(DeferredOperation::Mailbox);
        assert_eq!(deferred.next_wait(start), MAX_CONTROL_POLL_INTERVAL);
    }
}
