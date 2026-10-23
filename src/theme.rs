use anyhow::Context;
use std::iter;
use std::thread::{self, JoinHandle};
use std::time;

use windows::Win32::Foundation::{
    HANDLE, WAIT_ABANDONED_0, WAIT_EVENT, WAIT_FAILED, WAIT_OBJECT_0,
};
use windows::Win32::System::Registry::{
    HKEY, KEY_NOTIFY, REG_NOTIFY_CHANGE_LAST_SET, RegNotifyChangeKeyValue, RegOpenKeyExW,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use windows::core::PCWSTR;

use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

use crate::utils::{OwnedHANDLE, OwnedHKEY, get_last_error};
use crate::{HostAction, submit_host_action};

const PERSONALIZE_SUBKEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Themes\Personalize";
const THEME_VALUE_NAME: &str = "SystemUsesLightTheme";

/// Returns `true` if the Windows system theme is currently set to light mode.
/// Defaults to `false` (dark mode) on any error.
pub fn is_light_theme() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(personalize_key) = hkcu.open_subkey(PERSONALIZE_SUBKEY) else {
        debug!("could not open Personalize registry key; defaulting to dark theme");
        return false;
    };
    personalize_key
        .get_value::<u32, _>(THEME_VALUE_NAME)
        .unwrap_or(0)
        == 1
}

/// Watches for Windows system theme changes by monitoring the registry.
/// When the theme changes (dark ↔ light), it triggers a border reload.
#[derive(Debug)]
#[allow(unused)]
pub struct ThemeWatcher {
    reg_key: OwnedHKEY,
    changed_event: OwnedHANDLE,
    stop_event: OwnedHANDLE,
    thread_handle: Option<JoinHandle<()>>,
}

impl ThemeWatcher {
    pub fn new() -> anyhow::Result<Self> {
        // Open the registry key using the windows crate for RegNotifyChangeKeyValue
        let subkey_wide: Vec<u16> = PERSONALIZE_SUBKEY
            .encode_utf16()
            .chain(iter::once(0))
            .collect();

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
        .context("could not open Personalize registry key for theme watcher")?;

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

        let watcher_worker = move || {
            debug!("entering theme watcher thread");

            // Reconvert isize back to the original types
            let reg_hkey = HKEY(reg_handle_isize as _);
            let events = [
                HANDLE(changed_handle_isize as _),
                HANDLE(stop_handle_isize as _),
            ];

            const WAIT_OBJECT_1: WAIT_EVENT = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);
            const WAIT_ABANDONED_1: WAIT_EVENT = WAIT_EVENT(WAIT_ABANDONED_0.0 + 1);

            let mut last_theme = is_light_theme();

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
                    error!("could not wait for theme changes: {last_error:?}");
                    break;
                }

                // Registry change event signaled — check if theme actually toggled
                let current_theme = is_light_theme();
                if current_theme != last_theme {
                    last_theme = current_theme;
                    info!(
                        "system theme changed to {}; reloading borders",
                        if current_theme { "light" } else { "dark" }
                    );

                    // Small delay for the system to finish the theme transition
                    thread::sleep(time::Duration::from_millis(200));

                    crate::utils::LogIfErr::log_if_err(&submit_host_action(
                        HostAction::ReloadBorders,
                    ));
                }
            }

            debug!("exiting theme watcher thread");
        };
        let thread_handle = thread::Builder::new()
            .name("theme-watcher".into())
            .spawn(watcher_worker)
            .context("could not start theme watcher thread")?;

        Ok(Self {
            reg_key,
            changed_event,
            stop_event,
            thread_handle: Some(thread_handle),
        })
    }
}

impl Drop for ThemeWatcher {
    fn drop(&mut self) {
        let set_res = unsafe { SetEvent(self.stop_event.0) };

        match set_res {
            Ok(()) => match self.thread_handle.take() {
                Some(handle) => {
                    if let Err(err) = handle.join() {
                        error!("could not join theme watcher thread handle: {err:?}");
                    }
                }
                None => error!("could not take theme watcher thread handle"),
            },
            Err(err) => error!(
                "could not signal stop event on {:?} for theme watcher: {err:#}",
                self.stop_event
            ),
        }
    }
}
