use anyhow::Context;
use std::iter;
use std::thread::{self, JoinHandle};
use std::time;

use windows::Win32::Foundation::{
    HANDLE, HWND, LPARAM, WAIT_ABANDONED_0, WAIT_EVENT, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::Registry::{
    HKEY, KEY_NOTIFY, REG_NOTIFY_CHANGE_LAST_SET, RegNotifyChangeKeyValue, RegOpenKeyExW,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use windows::core::PCWSTR;

use winreg::enums::HKEY_CURRENT_USER;

use crate::APP_STATE;
use crate::colors::get_accent_color_value;
use crate::utils::{
    LogIfErr, OwnedHANDLE, OwnedHKEY, WM_APP_ACCENT_CHANGED, get_last_error, post_message_w,
};

const DWM_SUBKEY: &str = r"SOFTWARE\Microsoft\Windows\DWM";

/// Watches for Windows accent color changes by monitoring the registry. The DWM key holds
/// several values, so we compare the accent color itself before notifying borders, which
/// avoids re-rendering them for unrelated changes (e.g. ColorizationColor).
#[derive(Debug)]
#[allow(unused)]
pub struct AccentWatcher {
    reg_key: OwnedHKEY,
    changed_event: OwnedHANDLE,
    stop_event: OwnedHANDLE,
    thread_handle: Option<JoinHandle<()>>,
}

impl AccentWatcher {
    pub fn new() -> anyhow::Result<Self> {
        // Open the registry key using the windows crate for RegNotifyChangeKeyValue
        let subkey_wide: Vec<u16> = DWM_SUBKEY.encode_utf16().chain(iter::once(0)).collect();

        let mut hkey = HKEY::default();
        unsafe {
            RegOpenKeyExW(
                HKEY(HKEY_CURRENT_USER as _),
                PCWSTR(subkey_wide.as_ptr()),
                Some(0),
                KEY_NOTIFY,
                &mut hkey,
            )
        }
        .ok()
        .context("could not open DWM registry key for accent watcher")?;

        let reg_key = OwnedHKEY(hkey);

        // Convert the HKEY to isize so we can move it into the new thread
        let reg_handle_isize = reg_key.0.0 as isize;

        let changed_event = {
            let handle = unsafe { CreateEventW(None, false, false, None)? };
            OwnedHANDLE(handle)
        };

        let stop_event = {
            let handle = unsafe { CreateEventW(None, true, false, None)? };
            OwnedHANDLE(handle)
        };

        // Convert HANDLEs to isize so we can move them into the new thread
        let changed_handle_isize = changed_event.0.0 as isize;
        let stop_handle_isize = stop_event.0.0 as isize;

        let thread_handle = thread::spawn(move || {
            debug!("entering accent watcher thread");

            // Reconvert isize back to the original types
            let reg_hkey = HKEY(reg_handle_isize as _);
            let events = [
                HANDLE(changed_handle_isize as _),
                HANDLE(stop_handle_isize as _),
            ];

            const WAIT_OBJECT_1: WAIT_EVENT = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);
            const WAIT_ABANDONED_1: WAIT_EVENT = WAIT_EVENT(WAIT_ABANDONED_0.0 + 1);

            let mut last_accent = get_accent_color_value();

            loop {
                // Register for async notification on value changes
                let reg_result = unsafe {
                    RegNotifyChangeKeyValue(
                        reg_hkey,
                        false,
                        REG_NOTIFY_CHANGE_LAST_SET,
                        Some(events[0]), // changed_event
                        true,            // asynchronous
                    )
                };

                if reg_result.is_err() {
                    error!("RegNotifyChangeKeyValue failed: {:?}", reg_result);
                    break;
                }

                // Wait for either a registry change or a stop signal
                let wait_result = unsafe { WaitForMultipleObjects(&events, false, INFINITE) };

                // If the stop event is signaled, exit the loop
                if wait_result == WAIT_OBJECT_1 {
                    break;
                }

                // If an error occurred, log it and exit the thread
                if wait_result == WAIT_ABANDONED_0
                    || wait_result == WAIT_ABANDONED_1
                    || wait_result == WAIT_FAILED
                {
                    let last_error = get_last_error();
                    error!("could not wait for accent color changes: {last_error:?}");
                    break;
                }

                // The DWM key is also written to by other settings, so we only notify
                // borders when the accent color itself is different
                let Some(current_accent) = get_accent_color_value() else {
                    continue;
                };
                if Some(current_accent) == last_accent {
                    continue;
                }
                last_accent = Some(current_accent);

                info!("accent color changed to {current_accent:#010x}; updating borders");

                // Small delay for the system to finish applying the new accent color
                thread::sleep(time::Duration::from_millis(200));

                notify_borders();
            }

            debug!("exiting accent watcher thread");
        });

        Ok(Self {
            reg_key,
            changed_event,
            stop_event,
            thread_handle: Some(thread_handle),
        })
    }
}

/// Tells every border to re-resolve its colors. Each border re-reads its own config, so
/// windows that don't use "accent" are left visually untouched.
fn notify_borders() {
    let border_hwnds: Vec<HWND> = APP_STATE
        .borders
        .lock()
        .unwrap()
        .values()
        .map(|hwnd_isize| HWND(*hwnd_isize as _))
        .collect();

    for border_hwnd in border_hwnds {
        post_message_w(
            Some(border_hwnd),
            WM_APP_ACCENT_CHANGED,
            WPARAM::default(),
            LPARAM::default(),
        )
        .context("WM_APP_ACCENT_CHANGED")
        .log_if_err();
    }
}

impl Drop for AccentWatcher {
    fn drop(&mut self) {
        let set_res = unsafe { SetEvent(self.stop_event.0) };

        match set_res {
            Ok(()) => match self.thread_handle.take() {
                Some(handle) => {
                    if let Err(err) = handle.join() {
                        error!("could not join accent watcher thread handle: {err:?}");
                    }
                }
                None => error!("could not take accent watcher thread handle"),
            },
            Err(err) => error!(
                "could not signal stop event on {:?} for accent watcher: {err:#}",
                self.stop_event
            ),
        }
    }
}
