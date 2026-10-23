use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY,
    EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_REORDER, EVENT_OBJECT_SHOW,
    EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, OBJID_CLIENT, OBJID_CURSOR, OBJID_WINDOW,
};

use crate::APP_STATE;
use crate::utils::{
    LogIfErr, broadcast_border_work, destroy_border_for_window, get_foreground_window,
    hide_border_for_window, request_border_work, show_border_for_window,
};

const MAX_HOOK_RECORDS: usize = 1024;

#[derive(Clone, Copy)]
struct HookRecord {
    event: u32,
    hwnd: isize,
    object: i32,
    child: i32,
    thread: u32,
}

thread_local! {
    static RECORDS: RefCell<VecDeque<HookRecord>> = const { RefCell::new(VecDeque::new()) };
    static WAKE: RefCell<Option<crate::border_registry::BorderEvent>> = const { RefCell::new(None) };
    static PROCESSING: Cell<bool> = const { Cell::new(false) };
    static FAILED: Cell<bool> = const { Cell::new(false) };
    static RETRY_AT: Cell<Option<std::time::Instant>> = const { Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_WAKE: Cell<bool> = const { Cell::new(false) };
}

pub fn initialize_hook_pump() -> windows::core::Result<windows::Win32::Foundation::HANDLE> {
    RECORDS.with(|records| *records.borrow_mut() = VecDeque::with_capacity(MAX_HOOK_RECORDS));
    WAKE.with(|wake| {
        let event = crate::border_registry::BorderEvent::new(false)?;
        let handle = event.handle();
        *wake.borrow_mut() = Some(event);
        FAILED.set(false);
        RETRY_AT.set(None);
        Ok(handle)
    })
}

fn wake_pump() {
    WAKE.with(|wake| {
        if let Ok(wake) = wake.try_borrow() {
            if wake.as_ref().is_none_or(|event| {
                #[cfg(test)]
                if FAIL_NEXT_WAKE.replace(false) {
                    return unsafe {
                        windows::Win32::System::Threading::SetEvent(
                            windows::Win32::Foundation::HANDLE::default(),
                        )
                    }
                    .is_err();
                }
                event.signal().is_err()
            }) {
                FAILED.set(true);
            }
        } else {
            FAILED.set(true);
        }
    });
}

pub extern "system" fn process_win_event(
    hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object: i32,
    child: i32,
    thread: u32,
    _time: u32,
) {
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _ = hook;
        if object == OBJID_CURSOR.0
            || !matches!(
                event,
                EVENT_OBJECT_CREATE
                    | EVENT_OBJECT_DESTROY
                    | EVENT_OBJECT_SHOW
                    | EVENT_OBJECT_HIDE
                    | EVENT_OBJECT_CLOAKED
                    | EVENT_OBJECT_UNCLOAKED
                    | EVENT_OBJECT_LOCATIONCHANGE
                    | EVENT_OBJECT_REORDER
                    | EVENT_SYSTEM_FOREGROUND
                    | EVENT_SYSTEM_MINIMIZESTART
                    | EVENT_SYSTEM_MINIMIZEEND
            )
        {
            return;
        }
        RECORDS.with(|records| {
            if let Ok(mut records) = records.try_borrow_mut() {
                if records.len() >= MAX_HOOK_RECORDS || records.capacity() < MAX_HOOK_RECORDS {
                    FAILED.set(true);
                } else {
                    records.push_back(HookRecord {
                        event,
                        hwnd: hwnd.0 as isize,
                        object,
                        child,
                        thread,
                    });
                }
            } else {
                FAILED.set(true);
            }
        });
        wake_pump();
    }));
    if let Err(payload) = result {
        FAILED.set(true);
        crate::border_registry::discard_panic(payload);
    }
}

struct ProcessingGuard;
impl Drop for ProcessingGuard {
    fn drop(&mut self) {
        PROCESSING.set(false);
    }
}

fn pump_with(limit: usize, mut process: impl FnMut(HookRecord)) -> bool {
    if PROCESSING.replace(true) {
        return true;
    }
    let _guard = ProcessingGuard;
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| {
        for _ in 0..limit {
            let record = RECORDS.with(|records| records.borrow_mut().pop_front());
            let Some(record) = record else {
                break;
            };
            process(record);
        }
    })) {
        FAILED.set(true);
        crate::border_registry::discard_panic(payload);
    }
    let remaining = RECORDS.with(|records| !records.borrow().is_empty());
    if remaining {
        wake_pump();
    }
    remaining && !FAILED.get()
}

pub fn pump_hook_events(limit: usize) -> bool {
    if FAILED.get() {
        let now = std::time::Instant::now();
        if RETRY_AT.get().is_some_and(|deadline| now < deadline) {
            return false;
        }
        RETRY_AT.set(Some(now + std::time::Duration::from_millis(10)));
        RECORDS.with(|records| records.borrow_mut().clear());
        let Some(mut registry) =
            crate::border_coordinator::try_registry(&APP_STATE.border_registry)
        else {
            return false;
        };
        if registry.shutdown_requested || registry.quarantined {
            FAILED.set(false);
            return false;
        }
        registry.admission = crate::border_registry::BorderAdmission::Paused;
        let hwnds: Vec<_> = registry.observation_by_hwnd.keys().copied().collect();
        let actions: Vec<_> = hwnds
            .into_iter()
            .flat_map(|hwnd| registry.retire_observation(hwnd, None, std::time::Instant::now()))
            .collect();
        let busy = registry.active_operation.is_some();
        drop(registry);
        for action in actions {
            action.signal().map_err(anyhow::Error::from).log_if_err();
        }
        if busy {
            return false;
        }
        if crate::submit_host_action(crate::HostAction::ReloadBorders).is_err() {
            return false;
        }
        FAILED.set(false);
        RETRY_AT.set(None);
    }
    pump_with(limit, |record| {
        interpret_win_event(
            HWINEVENTHOOK::default(),
            record.event,
            HWND(record.hwnd as _),
            record.object,
            record.child,
            record.thread,
            0,
        )
    })
}

pub(crate) fn invalidate_history() {
    FAILED.set(true);
    wake_pump();
}

fn interpret_win_event(
    _h_win_event_hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _dw_event_thread: u32,
    _dwms_event_time: u32,
) {
    // Ignore cursor events
    if _id_object == OBJID_CURSOR.0 {
        return;
    }

    match _event {
        EVENT_OBJECT_CREATE => {
            if _id_object == OBJID_WINDOW.0 && _id_child == CHILDID_SELF as i32 {
                if crate::border_registry::TrackingWindowFingerprint::capture(_hwnd).is_some_and(
                    |fingerprint| {
                        _dw_event_thread != 0 && fingerprint.thread_id != _dw_event_thread
                    },
                ) {
                    return;
                }
                destroy_border_for_window(_hwnd);
                show_border_for_window(_hwnd);
            }
        }
        EVENT_OBJECT_LOCATIONCHANGE => {
            if _id_child != CHILDID_SELF as i32 {
                return;
            }

            request_border_work(_hwnd, crate::border_runtime::WORK_GEOMETRY);
        }
        EVENT_OBJECT_REORDER => {
            // Ignore OBJIDs not needed for window Z-order handling.
            if _id_object != OBJID_CLIENT.0 {
                return;
            }

            broadcast_border_work(crate::border_runtime::WORK_Z_ORDER);
        }
        // Neither the HWND passed by this event nor the one returned by GetForegroundWindow() are
        // accurate 100% of the time. I tried finding workarounds without polling, but gave up.
        EVENT_SYSTEM_FOREGROUND => {
            let potential_active_hwnd = get_foreground_window();

            // Immediately try these HWNDs, and if they're wrong, hope that polling works.
            handle_foreground_event(potential_active_hwnd, _hwnd);
        }
        EVENT_OBJECT_SHOW | EVENT_OBJECT_UNCLOAKED => {
            if _id_object == OBJID_WINDOW.0 {
                show_border_for_window(_hwnd);
            }
        }
        EVENT_OBJECT_HIDE | EVENT_OBJECT_CLOAKED => {
            if _id_object == OBJID_WINDOW.0 {
                hide_border_for_window(_hwnd);
            }
        }
        EVENT_SYSTEM_MINIMIZESTART => {
            hide_border_for_window(_hwnd);
        }
        EVENT_SYSTEM_MINIMIZEEND => {
            show_border_for_window(_hwnd);
        }
        EVENT_OBJECT_DESTROY
            if _id_object == OBJID_WINDOW.0 && _id_child == CHILDID_SELF as i32 =>
        {
            let Some(mut registry) =
                crate::border_coordinator::try_registry(&APP_STATE.border_registry)
            else {
                invalidate_history();
                return;
            };
            let actions = registry.retire_observation(
                _hwnd.0 as isize,
                Some(_dw_event_thread),
                std::time::Instant::now(),
            );
            drop(registry);
            for action in actions {
                action.signal().map_err(anyhow::Error::from).log_if_err();
            }
            if let Ok(owner) = APP_STATE.lifecycle() {
                owner.wake();
            }
        }
        _ => {}
    }
}

pub fn handle_foreground_event(best_hwnd_guess: HWND, other_hwnd_guess: HWND) {
    let new_active_hwnd = match !best_hwnd_guess.is_invalid() {
        true => best_hwnd_guess,
        false => other_hwnd_guess,
    };
    *APP_STATE.active_window.lock().unwrap() = new_active_hwnd.0 as isize;

    broadcast_border_work(
        crate::border_runtime::WORK_RENDER | crate::border_runtime::WORK_KOMOREBI,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enqueue(hwnd: isize) {
        process_win_event(
            HWINEVENTHOOK::default(),
            EVENT_OBJECT_SHOW,
            HWND(hwnd as _),
            OBJID_WINDOW.0,
            CHILDID_SELF as i32,
            5,
            0,
        );
    }

    #[test]
    fn reentrant_enqueue_preserves_fifo_and_processing_guard() {
        initialize_hook_pump().unwrap();
        enqueue(1);
        enqueue(2);
        let mut seen = Vec::new();
        assert!(!pump_with(8, |record| {
            seen.push(record.hwnd);
            if record.hwnd == 1 {
                enqueue(3);
                assert!(pump_with(1, |_| panic!("nested processor")));
            }
        }));
        assert_eq!(seen, vec![1, 2, 3]);
        assert!(!PROCESSING.get());
    }

    #[test]
    fn bounded_overflow_and_panic_invalidate_history_and_reset_processing() {
        initialize_hook_pump().unwrap();
        for index in 0..MAX_HOOK_RECORDS + 1 {
            enqueue(index as isize);
        }
        assert!(FAILED.get());
        assert_eq!(
            RECORDS.with(|records| records.borrow().len()),
            MAX_HOOK_RECORDS
        );
        pump_with(1, |_| panic!("injected interpretation failure"));
        assert!(!PROCESSING.get());
        assert!(FAILED.get());
        RECORDS.with(|records| records.borrow_mut().clear());
        FAILED.set(false);
    }

    #[test]
    #[serial_test::serial]
    fn failed_native_hook_wake_retains_failure_and_bounded_retry() {
        let wake = initialize_hook_pump().unwrap();
        FAIL_NEXT_WAKE.set(true);
        process_win_event(
            HWINEVENTHOOK::default(),
            EVENT_OBJECT_DESTROY,
            HWND(1 as _),
            OBJID_WINDOW.0,
            CHILDID_SELF as i32,
            5,
            0,
        );
        assert!(!FAIL_NEXT_WAKE.get());
        assert!(FAILED.get());
        assert_eq!(
            unsafe { windows::Win32::System::Threading::WaitForSingleObject(wake, 0) },
            windows::Win32::Foundation::WAIT_TIMEOUT
        );
        let registry = APP_STATE.border_registry.lock().unwrap();
        assert!(!pump_hook_events(64));
        let retry = RETRY_AT.get().unwrap();
        assert!(!pump_hook_events(64));
        assert_eq!(RETRY_AT.get(), Some(retry));
        assert!(FAILED.get());
        assert!(RECORDS.with(|records| records.borrow().is_empty()));
        drop(registry);
        use crate::border_coordinator::BorderLifecycleCoordinator;
        use crate::border_registry::{BorderAdmission, BorderRegistry};
        struct RestoreHost {
            host: std::sync::Arc<BorderLifecycleCoordinator>,
            registry: BorderRegistry,
            previous: Option<std::sync::Arc<BorderLifecycleCoordinator>>,
        }
        impl Drop for RestoreHost {
            fn drop(&mut self) {
                self.host.request_shutdown();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while !self.host.has_joined() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "hook recovery host did not settle"
                    );
                    if self.host.is_terminal() {
                        let _ = self.host.join_terminal();
                    }
                    std::thread::yield_now();
                }
                *APP_STATE.lifecycle.lock().unwrap() = self.previous.take();
                *APP_STATE.border_registry.lock().unwrap() = std::mem::take(&mut self.registry);
                FAILED.set(false);
                RETRY_AT.set(None);
            }
        }
        let requests = crate::initialize_host_requests().unwrap();
        let original = std::mem::take(&mut *APP_STATE.border_registry.lock().unwrap());
        let host = BorderLifecycleCoordinator::start_with_empty_baseline_for_test(
            APP_STATE.border_registry.clone(),
        )
        .unwrap();
        let previous = APP_STATE.lifecycle.lock().unwrap().replace(host.clone());
        let restore = RestoreHost {
            host,
            registry: original,
            previous,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let action = loop {
            assert!(!pump_hook_events(64));
            if let Ok(action) = requests.receiver.try_recv() {
                break action;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "failed hook wake did not request recovery"
            );
            std::thread::yield_now();
        };
        assert!(matches!(action, crate::HostAction::ReloadBorders));
        assert!(!FAILED.get());
        assert!(RETRY_AT.get().is_none());
        assert_eq!(
            APP_STATE.border_registry.lock().unwrap().admission,
            BorderAdmission::Paused
        );
        crate::reload_borders().unwrap().wait().unwrap();
        let registry = APP_STATE.border_registry.lock().unwrap();
        assert_eq!(registry.admission, BorderAdmission::Open);
        assert!(!registry.shutdown_requested);
        assert!(registry.entries.is_empty());
        drop(registry);
        drop(restore);
    }

    #[test]
    #[serial_test::serial]
    fn busy_destroy_invalidates_history_without_actionable_retry_spin() {
        initialize_hook_pump().unwrap();
        let registry = APP_STATE.border_registry.lock().unwrap();
        interpret_win_event(
            HWINEVENTHOOK::default(),
            EVENT_OBJECT_DESTROY,
            HWND(1 as _),
            OBJID_WINDOW.0,
            CHILDID_SELF as i32,
            5,
            0,
        );
        assert!(FAILED.get());
        assert!(!pump_hook_events(64));
        let retry = RETRY_AT.get().unwrap();
        assert!(!pump_hook_events(64));
        assert_eq!(RETRY_AT.get(), Some(retry));
        drop(registry);
        FAILED.set(false);
        RETRY_AT.set(None);
    }
}
