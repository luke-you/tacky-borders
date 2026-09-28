#[macro_use]
extern crate log;
extern crate sp_log;

pub mod accent_watcher;
pub mod anim_timer;
pub mod animations;
pub mod auto_start;
pub mod border_config;
pub mod border_drawer;
pub mod colors;
pub mod config;
pub mod effects;
pub mod event_hook;
pub mod iocp;
pub mod ipc;
pub mod komorebi;
pub mod render_backend;
pub mod sys_tray_icon;
pub mod theme;
pub mod utils;
pub mod window_border;

use accent_watcher::AccentWatcher;
use anyhow::{Context, anyhow};
use config::{Config, ConfigWatcher, EnableMode, config_watcher_callback};
use ipc::IpcServer;
use komorebi::KomorebiIntegration;
use render_backend::RenderBackendConfig;
use sp_log::{ColorChoice, CombinedLogger, FileLogger, LevelFilter, TermLogger, TerminalMode};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, OnceLock, RwLock, RwLockWriteGuard};
use std::thread::{self, JoinHandle};
use theme::ThemeWatcher;
use utils::{
    LogIfErr, OwnedHANDLE, T_E_UNINIT, ToWindowsResult, WM_APP_RECREATE_DRAWER,
    WindowsCompatibleResult, WindowsContext, create_border_for_window, get_foreground_window,
    get_last_error, get_window_rule, has_filtered_style, is_window_cloaked, is_window_top_level,
    is_window_visible, post_message_w, send_notify_message_w,
};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_CLASS_ALREADY_EXISTS, HANDLE, HMODULE, HWND, LPARAM, TRUE,
    WAIT_ABANDONED_0, WAIT_EVENT, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_FACTORY_TYPE_MULTI_THREADED, D2D1CreateFactory, ID2D1Device, ID2D1Factory1,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_9_1, D3D_FEATURE_LEVEL_9_2,
    D3D_FEATURE_LEVEL_9_3, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_GPU_PREFERENCE_UNSPECIFIED, IDXGIAdapter,
    IDXGIDevice, IDXGIFactory6, IDXGIFactory7,
};
use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, INFINITE, OpenThread, SetEvent, THREAD_SYNCHRONIZE,
    WaitForMultipleObjects,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_MAX, EVENT_MIN, EnumWindows, GetWindowThreadProcessId, IDC_ARROW, IDNO, LoadCursorW,
    MB_DEFBUTTON2, MB_ICONERROR, MB_ICONQUESTION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    MESSAGEBOX_RESULT, MESSAGEBOX_STYLE, MessageBoxW, RegisterClassExW, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_NCDESTROY, WNDCLASSEXW,
};
use windows::core::{BOOL, Interface, PCWSTR, w};

static IS_WINDOWS_11: LazyLock<bool> = LazyLock::new(|| {
    let mut version_info = OSVERSIONINFOW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    unsafe { RtlGetVersion(&mut version_info) }
        .ok()
        .log_if_err();

    debug!(
        "windows version: {}.{}.{}",
        version_info.dwMajorVersion, version_info.dwMinorVersion, version_info.dwBuildNumber
    );

    version_info.dwBuildNumber >= 22000
});
pub static APP_STATE: LazyLock<AppState> = LazyLock::new(AppState::new);
pub static BG_SERVICES: LazyLock<Mutex<BackgroundServices>> =
    LazyLock::new(|| Mutex::new(BackgroundServices::new(&APP_STATE.config.read().unwrap())));

pub struct AppState {
    borders: Mutex<HashMap<isize, isize>>,
    initial_windows: Mutex<Vec<isize>>,
    active_window: Mutex<isize>,
    config: RwLock<Config>,
    render_factory: ID2D1Factory1,
    directx_devices: RwLock<Option<DirectXDevices>>,
}

unsafe impl Send for AppState {}
unsafe impl Sync for AppState {}

impl AppState {
    fn new() -> Self {
        let active_window = get_foreground_window().0 as isize;

        let config = match Config::create() {
            Ok(config) => config,
            Err(err) => {
                eprintln!("could not read config: {err:#}");
                thread::spawn(move || {
                    display_error_box(format!("could not read config: {err:#}"), None);
                });

                Config::default()
            }
        };

        let render_factory: ID2D1Factory1 = unsafe {
            D2D1CreateFactory(D2D1_FACTORY_TYPE_MULTI_THREADED, None).unwrap_or_else(|err| {
                error!("could not create ID2D1Factory: {err:#}");
                panic!()
            })
        };

        let directx_devices_opt = match config.render_backend {
            RenderBackendConfig::V2 => {
                // I think I have to just panic if .unwrap() fails tbh; don't know what else I could do.
                let directx_devices = DirectXDevices::new(&render_factory).unwrap_or_else(|err| {
                    error!("could not create directx devices: {err:#}");
                    panic!("could not create directx devices: {err:#}");
                });

                Some(directx_devices)
            }
            RenderBackendConfig::Legacy => None,
        };

        AppState {
            borders: Mutex::new(HashMap::new()),
            initial_windows: Mutex::new(Vec::new()),
            active_window: Mutex::new(active_window),
            config: RwLock::new(config),
            render_factory,
            directx_devices: RwLock::new(directx_devices_opt),
        }
    }

    // The following getter/setters are meant for use in testing
    pub fn get_config_mut(&self) -> RwLockWriteGuard<'_, Config> {
        self.config.write().unwrap()
    }

    pub fn get_render_factory(&self) -> &ID2D1Factory1 {
        &self.render_factory
    }

    pub fn get_directx_devices_mut(&self) -> RwLockWriteGuard<'_, Option<DirectXDevices>> {
        self.directx_devices.write().unwrap()
    }
}

#[derive(Default)]
pub struct BackgroundServices {
    accent_watcher: Option<AccentWatcher>,
    config_watcher: Option<ConfigWatcher>,
    komorebi_integration: Option<KomorebiIntegration>,
    theme_watcher: Option<ThemeWatcher>,
    ipc_server: Option<IpcServer>,
    #[allow(dead_code)]
    display_adapters_watcher: Option<DisplayAdaptersWatcher>,
}

unsafe impl Send for BackgroundServices {}
unsafe impl Sync for BackgroundServices {}

impl BackgroundServices {
    fn new(config: &Config) -> Self {
        let mut accent_watcher = None;
        let mut config_watcher = None;
        let mut komorebi_integration = None;
        let mut theme_watcher = None;
        let mut ipc_server = None;

        if config.enable_logging
            && let Err(err) = create_logger()
        {
            eprintln!("[ERROR] could not create logger: {err:#}");
        };

        if config.is_accent_aware_enabled() {
            accent_watcher = AccentWatcher::new()
                .inspect_err(|err| error!("could not start accent watcher: {err:#}"))
                .ok();
        }

        if config.is_config_watcher_enabled() {
            config_watcher = create_config_watcher()
                .inspect_err(|err| error!("could not start config watcher: {err:#}"))
                .ok()
        }

        if config.is_komorebi_integration_enabled() {
            komorebi_integration = KomorebiIntegration::new()
                .inspect_err(|err| error!("could not start komorebi integration: {err:#}"))
                .ok();
        }

        if config.is_theme_aware_enabled() {
            theme_watcher = ThemeWatcher::new()
                .inspect_err(|err| error!("could not start theme watcher: {err:#}"))
                .ok();
        }

        if config.is_ipc_server_enabled() {
            ipc_server = ipc::socket_path()
                .and_then(|path| IpcServer::new(&path))
                .inspect_err(|err| error!("could not start ipc server: {err:#}"))
                .ok();
        }

        let display_adapters_watcher = DisplayAdaptersWatcher::new()
            .inspect_err(|err| error!("could not start display adapters watcher: {err:#}"))
            .ok();

        Self {
            accent_watcher,
            config_watcher,
            komorebi_integration,
            theme_watcher,
            ipc_server,
            display_adapters_watcher,
        }
    }

    pub fn reload(&mut self, config: &Config) {
        // We take and destructure so that the compiler complains if we add a new
        // struct field and forget to add it to the reload logic
        let Self {
            mut accent_watcher,
            mut config_watcher,
            mut komorebi_integration,
            mut theme_watcher,
            mut ipc_server,
            display_adapters_watcher,
        } = std::mem::take(self);

        if config.is_accent_aware_enabled() && accent_watcher.is_none() {
            accent_watcher = AccentWatcher::new()
                .inspect_err(|err| error!("could not start accent watcher: {err:#}"))
                .ok();
        } else if !config.is_accent_aware_enabled() && accent_watcher.is_some() {
            accent_watcher = None;
        }

        if config.is_config_watcher_enabled() && config_watcher.is_none() {
            config_watcher = create_config_watcher()
                .inspect_err(|err| error!("could not start config watcher: {err:#}"))
                .ok();
        } else if !config.is_config_watcher_enabled() && config_watcher.is_some() {
            config_watcher = None;
        }

        if config.is_komorebi_integration_enabled() && komorebi_integration.is_none() {
            komorebi_integration = KomorebiIntegration::new()
                .inspect_err(|err| error!("could not start komorebi integration: {err:#}"))
                .ok();
        } else if !config.is_komorebi_integration_enabled() && komorebi_integration.is_some() {
            komorebi_integration = None;
        }

        if config.is_theme_aware_enabled() && theme_watcher.is_none() {
            theme_watcher = ThemeWatcher::new()
                .inspect_err(|err| error!("could not start theme watcher: {err:#}"))
                .ok();
        } else if !config.is_theme_aware_enabled() && theme_watcher.is_some() {
            theme_watcher = None;
        }

        if config.is_ipc_server_enabled() && ipc_server.is_none() {
            ipc_server = ipc::socket_path()
                .and_then(|path| IpcServer::new(&path))
                .inspect_err(|err| error!("could not start ipc server: {err:#}"))
                .ok();
        } else if !config.is_ipc_server_enabled() && ipc_server.is_some() {
            ipc_server = None;
        }

        *self = Self {
            accent_watcher,
            config_watcher,
            komorebi_integration,
            theme_watcher,
            ipc_server,
            display_adapters_watcher,
        }
    }

    pub fn shutdown(&mut self) {
        *self = Self::default();
    }
}

#[allow(unused)]
struct DisplayAdaptersWatcher {
    dxgi_factory: IDXGIFactory7,
    changed_event: OwnedHANDLE,
    changed_cookie: u32,
    stop_event: OwnedHANDLE,
    thread_handle: Option<JoinHandle<()>>,
}

impl DisplayAdaptersWatcher {
    fn new() -> anyhow::Result<Self> {
        let dxgi_factory: IDXGIFactory7 =
            unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS::default()) }
                .context("could not create dxgi_factory to watch for display adapter changes; issues may occur due to an inability to update DirectX devices accordingly")?;

        let changed_event = {
            let handle = unsafe { CreateEventW(None, false, false, None)? };
            OwnedHANDLE(handle)
        };
        let changed_cookie = unsafe { dxgi_factory.RegisterAdaptersChangedEvent(changed_event.0) }?;

        let stop_event = {
            let handle = unsafe { CreateEventW(None, true, false, None)? };
            OwnedHANDLE(handle)
        };

        // Convert the HANDLEs to isize so we can pass them into the thread below
        let changed_handle_isize = changed_event.0.0 as isize;
        let stop_handle_isize = stop_event.0.0 as isize;

        let thread_handle = thread::spawn(move || {
            debug!("entering display adapters watcher thread");

            let events = [
                HANDLE(changed_handle_isize as _),
                HANDLE(stop_handle_isize as _),
            ];

            const WAIT_OBJECT_1: WAIT_EVENT = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);
            const WAIT_ABANDONED_1: WAIT_EVENT = WAIT_EVENT(WAIT_ABANDONED_0.0 + 1);

            loop {
                // This function will block until an event or error has been signaled.
                let wait_event = unsafe { WaitForMultipleObjects(&events, false, INFINITE) };

                // If the stop event has been signaled, exit the loop
                if wait_event == WAIT_OBJECT_1 {
                    break;
                }

                // If an error has occurred, log it and exit the thread.
                if wait_event == WAIT_ABANDONED_0
                    || wait_event == WAIT_ABANDONED_1
                    || wait_event == WAIT_FAILED
                {
                    let last_error = get_last_error();
                    error!("could not check for display adapter changes: {last_error:?}");

                    break;
                }

                if let Some(directx_devices) = APP_STATE.directx_devices.write().unwrap().as_mut()
                    && let Err(err) = directx_devices.recreate_if_needed()
                {
                    error!("could not recreate directx devices if needed: {err:#}");
                    break;
                }

                for hwnd_isize in APP_STATE.borders.lock().unwrap().values() {
                    let border_hwnd = HWND(*hwnd_isize as _);
                    post_message_w(
                        Some(border_hwnd),
                        WM_APP_RECREATE_DRAWER,
                        WPARAM::default(),
                        LPARAM::default(),
                    )
                    .context("WM_APP_RECREATE_RENDERER")
                    .log_if_err();
                }
            }

            debug!("exiting display adapters watcher thread");
        });

        Ok(Self {
            dxgi_factory,
            changed_event,
            changed_cookie,
            stop_event,
            thread_handle: Some(thread_handle),
        })
    }
}

impl Drop for DisplayAdaptersWatcher {
    fn drop(&mut self) {
        unsafe {
            self.dxgi_factory
                .UnregisterAdaptersChangedEvent(self.changed_cookie)
        }
        .context("could not unregister adapters changed event")
        .log_if_err();

        let set_res = unsafe { SetEvent(self.stop_event.0) };

        match set_res {
            Ok(()) => match self.thread_handle.take() {
                Some(handle) => {
                    if let Err(err) = handle.join() {
                        error!("could not join display adapters watcher thread handle: {err:?}");
                    }
                }
                None => error!("could not take display adapters watcher thread handle"),
            },
            Err(err) => error!(
                "could not signal stop event on {:?} for display adapters watcher: {err:#}",
                self.stop_event
            ),
        }
    }
}

pub struct DirectXDevices {
    dxgi_device: IDXGIDevice,
    d2d_device: ID2D1Device,
}

impl DirectXDevices {
    pub fn new(factory: &ID2D1Factory1) -> WindowsCompatibleResult<Self> {
        let creation_flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;

        let feature_levels = [
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
            D3D_FEATURE_LEVEL_9_3,
            D3D_FEATURE_LEVEL_9_2,
            D3D_FEATURE_LEVEL_9_1,
        ];

        let mut device_opt: Option<ID3D11Device> = None;
        let mut feature_level: D3D_FEATURE_LEVEL = D3D_FEATURE_LEVEL::default();

        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                creation_flags,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device_opt),
                Some(&mut feature_level),
                None,
            )
        }?;

        debug!("directx feature_level: {feature_level:X?}");

        let d3d11_device = device_opt
            .context("could not get d3d11_device")
            .to_windows_result(T_E_UNINIT)?;
        let dxgi_device: IDXGIDevice = d3d11_device.cast().windows_context("dxgi_device")?;
        let d2d_device =
            unsafe { factory.CreateDevice(&dxgi_device) }.windows_context("d2d_device")?;

        let dxgi_adapter: IDXGIAdapter =
            unsafe { dxgi_device.GetAdapter() }.windows_context("dxgi_adapter")?;
        let adapter_desc = unsafe { dxgi_adapter.GetDesc() }.windows_context("adapter_desc")?;
        let name_len = adapter_desc
            .Description
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(adapter_desc.Description.len());
        let adapter_name = String::from_utf16_lossy(&adapter_desc.Description[..name_len]);
        debug!("display adapter name: {adapter_name}");

        Ok(Self {
            dxgi_device,
            d2d_device,
        })
    }

    pub fn needs_recreation(&self) -> WindowsCompatibleResult<bool> {
        let dxgi_factory: IDXGIFactory6 =
            unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS::default()) }.windows_context(
                "could not create dxgi_factory to check for GPU adapter changes",
            )?;

        let new_dxgi_adapter: IDXGIAdapter =
            unsafe { dxgi_factory.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_UNSPECIFIED)? };
        let new_adapter_desc = unsafe { new_dxgi_adapter.GetDesc() }
            .windows_context("could not get new_adapter_desc")?;

        let curr_dxgi_adapter: IDXGIAdapter = unsafe {
            self.dxgi_device
                .GetAdapter()
                .windows_context("could not get curr_dxgi_adapter")?
        };
        let curr_adapter_desc = unsafe { curr_dxgi_adapter.GetDesc() }
            .windows_context("could not get curr_adapter_desc")?;

        Ok(curr_adapter_desc.AdapterLuid != new_adapter_desc.AdapterLuid)
    }

    pub fn recreate_if_needed(&mut self) -> WindowsCompatibleResult<()> {
        if self.needs_recreation()? {
            info!("recreating render devices");
            *self = DirectXDevices::new(&APP_STATE.render_factory)?;
        }

        Ok(())
    }
}

fn create_config_watcher() -> anyhow::Result<ConfigWatcher> {
    let config_path = Config::get_dir()
        .map(|dir| dir.join("config.yaml"))
        .context("could not get dir for config watcher")?;
    ConfigWatcher::new(config_path, 500, config_watcher_callback)
        .context("could not create config watcher")
}

pub fn create_logger() -> anyhow::Result<()> {
    // NOTE: there are two Config structs in this function: tacky-borders' and sp_log's
    let log_path = crate::Config::get_dir()?.join("tacky-borders.log");
    let Some(path_str) = log_path.to_str() else {
        return Err(anyhow!("could not convert log_path to str"));
    };

    let logger_config = sp_log::ConfigBuilder::new()
        .set_location_level(LevelFilter::Error)
        .set_time_offset_to_local()
        .unwrap_or_else(|builder| {
            error!("could not set logger's time offset to local");
            builder // the Err type is just another &mut ConfigBuilder 
        })
        .build();

    CombinedLogger::init(vec![
        TermLogger::new(
            LevelFilter::Debug,
            logger_config.clone(),
            TerminalMode::Mixed,
            ColorChoice::Auto,
        ),
        FileLogger::new(
            LevelFilter::Info,
            logger_config,
            path_str,
            // 1 MB
            Some(1024 * 1024),
        ),
    ])?;

    Ok(())
}

pub fn attach_parent_console() -> WindowsCompatibleResult<()> {
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }?;
    Ok(())
}

pub fn register_border_window_class() -> anyhow::Result<()> {
    unsafe {
        let window_class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_border::WindowBorder::s_wnd_proc),
            hInstance: GetModuleHandleW(None)?.into(),
            lpszClassName: w!("border"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };

        let result = RegisterClassExW(&window_class);
        if result == 0 {
            let last_error = get_last_error();
            if last_error != ERROR_CLASS_ALREADY_EXISTS {
                return Err(anyhow!("could not register window class: {last_error:?}"));
            }
        }
    }

    Ok(())
}

pub fn set_event_hook() -> HWINEVENTHOOK {
    unsafe {
        SetWinEventHook(
            EVENT_MIN,
            EVENT_MAX,
            None,
            Some(event_hook::process_win_event),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    }
}

pub fn create_borders_for_existing_windows() -> WindowsCompatibleResult<()> {
    unsafe { EnumWindows(Some(create_borders_callback), LPARAM::default()) }?;
    debug!("windows have been enumerated!");

    Ok(())
}

pub fn destroy_borders() {
    // Copy the hashmap's values to prevent mutex deadlocks
    let border_hwnds: Vec<HWND> = APP_STATE
        .borders
        .lock()
        .unwrap()
        .values()
        .map(|hwnd_isize| HWND(*hwnd_isize as _))
        .collect();

    let thread_ids: HashSet<u32> = border_hwnds
        .iter()
        .filter_map(|hwnd| {
            let thread_id = unsafe { GetWindowThreadProcessId(*hwnd, None) };
            if thread_id != 0 {
                Some(thread_id)
            } else {
                error!(
                    "could not get thread id from {:?}: {:?}",
                    hwnd,
                    get_last_error()
                );
                None
            }
        })
        .collect();

    let thread_handles: Vec<HANDLE> = thread_ids
        .into_iter()
        .filter_map(
            |thread_id| match unsafe { OpenThread(THREAD_SYNCHRONIZE, false, thread_id) } {
                Ok(handle) => Some(handle),
                Err(err) => {
                    error!("could not get thread handle from thread id {thread_id}: {err:#}");
                    None
                }
            },
        )
        .collect();

    // Tell the border windows to destroy themselves
    for hwnd in border_hwnds.into_iter() {
        send_notify_message_w(hwnd, WM_NCDESTROY, WPARAM::default(), LPARAM::default())
            .with_context(|| format!("could not send notify WM_NCDESTROY to {hwnd:?}"))
            .log_if_err();
    }

    let timeout_ms = 1000;
    let wait_event = unsafe { WaitForMultipleObjects(&thread_handles, true, timeout_ms) };

    let wait_object_start = WAIT_OBJECT_0.0;
    let wait_object_end = WAIT_OBJECT_0.0 + (thread_handles.len() as u32);
    let wait_object_range = wait_object_start..wait_object_end;

    // If thread_handles is empty, WaitForMultipleObjects returns WAIT_FAILED since there's nothing
    // to wait on. This is expected and safely ignored by the is_empty() check below.
    if !thread_handles.is_empty() && !wait_object_range.contains(&wait_event.0) {
        error!(
            "failed to wait for all border threads to exit: {:?}",
            get_last_error()
        );
    }

    // Convert HANDLEs to OwnedHANDLEs so they automatically close when dropped
    let _owned_handles: Vec<OwnedHANDLE> = thread_handles.into_iter().map(OwnedHANDLE).collect();

    // NOTE: we will rely on each border thread to remove themselves from the hashmap, so we won't
    // do any manual cleanup here
}

pub fn reload_borders() {
    destroy_borders();
    APP_STATE.initial_windows.lock().unwrap().clear();
    create_borders_for_existing_windows().log_if_err();
}

pub fn display_error_box<T: std::fmt::Display>(
    error: T,
    extra_styles: Option<MESSAGEBOX_STYLE>,
) -> MESSAGEBOX_RESULT {
    let error_vec: Vec<u16> = error
        .to_string()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        MessageBoxW(
            None,
            PCWSTR(error_vec.as_ptr()),
            w!("tacky-borders"),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST | extra_styles.unwrap_or_default(),
        )
    }
}

pub fn display_question_box<T: std::fmt::Display>(
    question: T,
    extra_styles: Option<MESSAGEBOX_STYLE>,
) -> MESSAGEBOX_RESULT {
    let question_vec: Vec<u16> = question
        .to_string()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        MessageBoxW(
            None,
            PCWSTR(question_vec.as_ptr()),
            w!("tacky-borders"),
            MB_YESNO
                | MB_ICONQUESTION
                | MB_SETFOREGROUND
                | MB_TOPMOST
                | extra_styles.unwrap_or_default(),
        )
    }
}

pub fn is_unwanted_instance() -> bool {
    static IS_UNWANTED: OnceLock<bool> = OnceLock::new();

    *IS_UNWANTED.get_or_init(|| {
        const MUTEX_NAME: PCWSTR = w!("Local\\tacky-borders-a6b83a75-bebb-46ac-b29f-ea9aa0855fa2");

        // Note that this creates a raw handle that gets leaked when it goes out of scope. This is
        // intentional. We want this Mutex to remain alive for the duration of the program. And,
        // MSDN says the handle is automatically closed upon application exit, so leaking is fine.
        let mutex_res = unsafe { CreateMutexW(None, false, MUTEX_NAME) };

        // Note that in the case of ERROR_ALREADY_EXISTS, CreateMutexW still returns Ok with a handle
        // to the existing Mutex, so we need to check the error separately with GetLastError.
        if mutex_res.is_err() || get_last_error() == ERROR_ALREADY_EXISTS {
            let yes_no_decision = display_question_box(
                "an instance of tacky-borders is already running; continue anyways?",
                Some(MB_DEFBUTTON2),
            );

            return yes_no_decision == IDNO;
        }

        false
    })
}

unsafe extern "system" fn create_borders_callback(_hwnd: HWND, _lparam: LPARAM) -> BOOL {
    if is_window_top_level(_hwnd) {
        // Only create borders for visible windows
        if is_window_visible(_hwnd) && !is_window_cloaked(_hwnd) {
            let window_rule = get_window_rule(_hwnd);

            if window_rule.enabled == Some(EnableMode::Bool(false)) {
                info!("border is disabled for {_hwnd:?}");
            } else if window_rule.enabled == Some(EnableMode::Bool(true))
                || !has_filtered_style(_hwnd)
            {
                create_border_for_window(_hwnd, window_rule);
            }
        }

        // Add currently open windows to the intial windows list so we can keep track of them
        APP_STATE
            .initial_windows
            .lock()
            .unwrap()
            .push(_hwnd.0 as isize);
    }

    TRUE
}
