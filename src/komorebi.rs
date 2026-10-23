use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time;
use windows::Win32::Foundation::HWND;

use crate::APP_STATE;
use crate::colors::ColorBrushConfig;
use crate::config::serde_default_bool;
use crate::iocp::{UnixStreamSink, write_to_unix_socket};
use crate::utils::{LogIfErr, get_foreground_window, is_window, remove_file_if_exists};

#[derive(Debug, Default, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct KomorebiColorsConfig {
    pub stack_color: Option<ColorBrushConfig>,
    pub monocle_color: Option<ColorBrushConfig>,
    pub floating_color: Option<ColorBrushConfig>,
    #[serde(default = "serde_default_bool::<true>")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowKind {
    Single,
    Stack,
    Monocle,
    Unfocused,
    Floating,
}

// Minimal version of a komorebi enum of the same name
#[derive(Serialize)]
#[serde(tag = "type", content = "content")]
enum SocketMessage {
    AddSubscriberSocket(String),
}

pub struct KomorebiIntegration {
    // NOTE: in komorebi it's <Border HWND, WindowKind>, but here it's <Tracking HWND, WindowKind>
    pub focus_state: Arc<Mutex<HashMap<isize, WindowKind>>>,
    _stream_sink: UnixStreamSink,
    subscription_stop: Arc<AtomicBool>,
    subscription_thread: Option<JoinHandle<()>>,
}

impl KomorebiIntegration {
    const TACKY_SOCKET: &str = "tacky-borders.sock";
    const KOMOREBI_SOCKET: &str = "komorebi.sock";
    const FOCUS_STATE_PRUNE_INTERVAL: time::Duration = time::Duration::from_secs(600);
    const SUBSCRIBE_RETRY_INTERVAL: time::Duration = time::Duration::from_secs(15);

    pub fn new() -> anyhow::Result<Self> {
        let komorebi_data_dir =
            Self::get_komorebi_data_dir().context("could not get komorebi data dir")?;
        let tacky_socket_path = komorebi_data_dir.join(Self::TACKY_SOCKET);
        let komorebi_socket_path = komorebi_data_dir.join(Self::KOMOREBI_SOCKET);

        remove_file_if_exists(&tacky_socket_path)
            .context("could not remove tacky-borders socket if it exists")?;

        let focus_state = Arc::new(Mutex::new(HashMap::new()));
        let focus_state_clone = focus_state.clone();

        let stream_sink =
            Self::spawn_komorebi_notification_handler(focus_state_clone, &tacky_socket_path)
                .context("could not spawn komorebi notification handler")?;

        let subscription_stop = Arc::new(AtomicBool::new(false));
        let subscription_thread =
            Self::spawn_komorebi_subscribe_thread(komorebi_socket_path, subscription_stop.clone())
                .context("could not spawn komorebi subscribe thread")?;

        Ok(Self {
            focus_state,
            _stream_sink: stream_sink,
            subscription_stop,
            subscription_thread: Some(subscription_thread),
        })
    }

    fn spawn_komorebi_notification_handler(
        focus_state: Arc<Mutex<HashMap<isize, WindowKind>>>,
        tacky_socket_path: &Path,
    ) -> anyhow::Result<UnixStreamSink> {
        let mut last_focus_state_prune = time::Instant::now();

        let callback = move |buffer: &[u8], bytes_received: u32| {
            if last_focus_state_prune.elapsed() > Self::FOCUS_STATE_PRUNE_INTERVAL {
                debug!("pruning focus state for komorebi integration");
                focus_state
                    .lock()
                    .unwrap()
                    .retain(|&hwnd_isize, _| is_window(Some(HWND(hwnd_isize as _))));
                last_focus_state_prune = time::Instant::now();
            }

            Self::process_komorebi_notification(&focus_state, buffer, bytes_received);
        };

        let stream_sink = UnixStreamSink::new(tacky_socket_path, callback)?;

        Ok(stream_sink)
    }

    fn spawn_komorebi_subscribe_thread(
        komorebi_socket_path: PathBuf,
        stop: Arc<AtomicBool>,
    ) -> anyhow::Result<JoinHandle<()>> {
        let mut subscribe_message = {
            let enum_variant = SocketMessage::AddSubscriberSocket(Self::TACKY_SOCKET.to_string());
            serde_json::to_string(&enum_variant)?
        };

        Self::spawn_subscription_worker(stop, move || {
            let subscribe_bytes = unsafe { subscribe_message.as_bytes_mut() };
            write_to_unix_socket(&komorebi_socket_path, subscribe_bytes)
        })
    }

    fn spawn_subscription_worker(
        stop: Arc<AtomicBool>,
        mut subscribe: impl FnMut() -> anyhow::Result<()> + Send + 'static,
    ) -> anyhow::Result<JoinHandle<()>> {
        thread::Builder::new()
            .name("komorebi-subscribe".into())
            .spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    if let Err(error) = subscribe() {
                        debug!("could not send subscribe-socket message to komorebi: {error:#}");
                        thread::park_timeout(KomorebiIntegration::SUBSCRIBE_RETRY_INTERVAL);
                    } else {
                        break;
                    }
                }
            })
            .context("could not spawn komorebi subscribe worker")
    }

    fn get_komorebi_data_dir() -> anyhow::Result<PathBuf> {
        Ok(dirs::data_local_dir()
            .context("could not get data local dir")?
            .join("komorebi"))
    }

    // Largely adapted from komorebi's own border implementation. Thanks @LGUG2Z
    fn process_komorebi_notification(
        focus_state_mutex: &Arc<Mutex<HashMap<isize, WindowKind>>>,
        buffer: &[u8],
        bytes_received: u32,
    ) {
        let notification: serde_json_borrow::Value =
            match serde_json::from_slice(&buffer[..bytes_received as usize]) {
                Ok(event) => event,
                Err(err) => {
                    error!("could not parse unix domain socket buffer: {err:#}");
                    return;
                }
            };

        let previous_focus_state = (*focus_state_mutex.lock().unwrap()).clone();

        let monitors = notification.get("state").get("monitors");
        let focused_monitor_idx = monitors.get("focused").as_u64().unwrap() as usize;
        let foreground_window = get_foreground_window();

        for (monitor_idx, m) in monitors
            .get("elements")
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            // Only operate on the focused workspace of each monitor
            if let Some(ws) = m
                .get("workspaces")
                .get("elements")
                .as_array()
                .unwrap()
                .get(m.get("workspaces").get("focused").as_u64().unwrap() as usize)
            {
                // Handle the monocle container separately
                let monocle = ws.get("monocle_container");
                if !monocle.is_null() {
                    let new_focus_state = if monitor_idx != focused_monitor_idx {
                        WindowKind::Unfocused
                    } else {
                        WindowKind::Monocle
                    };

                    {
                        // If this is a monocole, I assume there's only 1 window in "windows"
                        let tracking_hwnd =
                            monocle.get("windows").get("elements").as_array().unwrap()[0]
                                .get("hwnd")
                                .as_i64()
                                .unwrap() as isize;
                        let mut focus_state = focus_state_mutex.lock().unwrap();
                        let _ = focus_state.insert(tracking_hwnd, new_focus_state);
                    }
                }

                let foreground_hwnd = get_foreground_window();

                for (idx, c) in ws
                    .get("containers")
                    .get("elements")
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                {
                    let new_focus_state = if idx
                        != ws.get("containers").get("focused").as_i64().unwrap() as usize
                        || monitor_idx != focused_monitor_idx
                        || c.get("windows")
                            .get("elements")
                            .as_array()
                            .unwrap()
                            .get(c.get("windows").get("focused").as_u64().unwrap() as usize)
                            .map(|w| {
                                w.get("hwnd").as_i64().unwrap() as isize
                                    != foreground_hwnd.0 as isize
                            })
                            .unwrap_or_default()
                    {
                        WindowKind::Unfocused
                    } else if c.get("windows").get("elements").as_array().unwrap().len() > 1 {
                        WindowKind::Stack
                    } else {
                        WindowKind::Single
                    };

                    // Update the window kind for all containers on this workspace
                    {
                        let tracking_hwnd = c.get("windows").get("elements").as_array().unwrap()
                            [c.get("windows").get("focused").as_u64().unwrap() as usize]
                            .get("hwnd")
                            .as_i64()
                            .unwrap() as isize;
                        let mut focus_state = focus_state_mutex.lock().unwrap();
                        let _ = focus_state.insert(tracking_hwnd, new_focus_state);
                    }
                }
                {
                    for window in ws
                        .get("floating_windows")
                        .get("elements")
                        .as_array()
                        .unwrap()
                    {
                        let mut new_focus_state = WindowKind::Unfocused;

                        if foreground_window.0 as isize
                            == window.get("hwnd").as_i64().unwrap() as isize
                        {
                            new_focus_state = WindowKind::Floating;
                        }

                        {
                            let tracking_hwnd = window.get("hwnd").as_i64().unwrap() as isize;
                            let mut focus_state = focus_state_mutex.lock().unwrap();
                            let _ = focus_state.insert(tracking_hwnd, new_focus_state);
                        }
                    }
                }
            }
        }

        let new_focus_state = focus_state_mutex.lock().unwrap().clone();
        let targets = crate::border_coordinator::try_registry(&APP_STATE.border_registry)
            .map(|registry| {
                registry
                    .entries
                    .values()
                    .map(|entry| (entry.tracking.fingerprint.hwnd, entry.updates.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (tracking, updates) in targets {
            let previous_window_kind = previous_focus_state.get(&tracking);
            let new_window_kind = new_focus_state.get(&tracking);

            // Only post update messages when the window kind has actually changed
            if previous_window_kind != new_window_kind {
                // If the window kinds were just Single and Unfocused, then we can just rely on
                // tacky-borders' internal logic to update border colors
                if matches!(
                    previous_window_kind,
                    Some(WindowKind::Single) | Some(WindowKind::Unfocused)
                ) && matches!(
                    new_window_kind,
                    Some(WindowKind::Single) | Some(WindowKind::Unfocused)
                ) {
                    continue;
                }

                if !updates.control().is_stopped() {
                    updates
                        .hint(crate::border_runtime::WORK_KOMOREBI)
                        .log_if_err();
                }
            }
        }
    }
}

impl Drop for KomorebiIntegration {
    fn drop(&mut self) {
        self.subscription_stop.store(true, Ordering::Release);
        if let Some(thread) = self.subscription_thread.take() {
            thread.thread().unpark();
            if let Err(payload) = thread.join() {
                crate::border_registry::discard_panic(payload);
                error!("komorebi subscribe worker panicked");
            }
        }
    }
}

#[cfg(test)]
mod subscription_worker_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    #[test]
    fn stopping_retry_worker_wakes_and_joins_it() {
        let stop = Arc::new(AtomicBool::new(false));
        let attempts = Arc::new(AtomicUsize::new(0));
        let (started, ready) = mpsc::channel();
        let worker_stop = stop.clone();
        let worker_attempts = attempts.clone();
        let handle = KomorebiIntegration::spawn_subscription_worker(stop.clone(), move || {
            worker_attempts.fetch_add(1, Ordering::Relaxed);
            let _ = started.send(());
            anyhow::bail!("injected unavailable server")
        })
        .unwrap();

        ready.recv_timeout(time::Duration::from_secs(1)).unwrap();
        stop.store(true, Ordering::Release);
        handle.thread().unpark();
        handle.join().unwrap();
        assert!(worker_stop.load(Ordering::Acquire));
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }
}
