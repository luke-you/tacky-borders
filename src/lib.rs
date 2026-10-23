#[macro_use]
extern crate log;
extern crate sp_log;

pub mod anim_timer;
pub mod animations;
pub mod auto_start;
pub mod border_config;
pub mod border_coordinator;
pub mod border_drawer;
pub mod border_registry;
pub mod border_runtime;
pub mod border_updates;
pub mod colors;
pub mod command_context;
pub mod config;
pub mod effects;
pub mod event_hook;
pub mod iocp;
pub mod ipc;
mod ipc_security;
pub mod komorebi;
pub mod render_backend;
pub mod sys_tray_icon;
pub mod theme;
pub mod utils;
pub mod window_border;

use anyhow::{Context, anyhow};
use config::{Config, ConfigWatcher, RuntimeConfig, config_watcher_callback};
use ipc::IpcServer;
use komorebi::KomorebiIntegration;
use render_backend::RenderBackendConfig;
use sp_log::{ColorChoice, CombinedLogger, FileLogger, LevelFilter, TermLogger, TerminalMode};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock, RwLockWriteGuard};
use std::thread::{self, JoinHandle};
use theme::ThemeWatcher;
use utils::{
    LogIfErr, OwnedHANDLE, T_E_UNINIT, ToWindowsResult, WindowsCompatibleResult, WindowsContext,
    get_foreground_window, get_last_error, post_message_w,
};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_CLASS_ALREADY_EXISTS, ERROR_SUCCESS, FALSE, HANDLE, HMODULE, HWND,
    LPARAM, SetLastError, TRUE, WAIT_ABANDONED_0, WAIT_EVENT, WAIT_FAILED, WAIT_OBJECT_0,
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
    CreateEventW, CreateMutexW, INFINITE, SetEvent, WaitForMultipleObjects,
};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_MAX, EVENT_MIN, EnumWindows, IDC_ARROW, LoadCursorW, MB_ICONERROR, MB_OK,
    MB_SETFOREGROUND, MB_TOPMOST, MESSAGEBOX_RESULT, MESSAGEBOX_STYLE, MessageBoxW,
    RegisterClassExW, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WNDCLASSEXW,
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

static HOST_SUBMISSION: OnceLock<(
    std::sync::mpsc::SyncSender<HostAction>,
    Arc<border_registry::BorderEvent>,
)> = OnceLock::new();
static IPC_RELOAD_SUBMISSIONS: OnceLock<Mutex<bool>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReloadOrigin {
    Watcher,
    Tray,
    Ipc,
}

#[derive(Debug)]
pub struct IpcReloadResponse {
    pub outcome: config::ReloadOutcome,
    pub acknowledgement: std::sync::mpsc::SyncSender<()>,
}

#[derive(Clone)]
pub enum HostAction {
    ReloadConfig {
        origin: ReloadOrigin,
        response: Option<std::sync::mpsc::SyncSender<IpcReloadResponse>>,
        context: Option<command_context::CommandContext>,
    },
    ReloadBorders,
}

pub struct HostRequests {
    pub receiver: std::sync::mpsc::Receiver<HostAction>,
    pub wake: Arc<border_registry::BorderEvent>,
}

pub fn initialize_host_requests() -> anyhow::Result<HostRequests> {
    let wake = Arc::new(border_registry::BorderEvent::new(false)?);
    let (sender, receiver) =
        std::sync::mpsc::sync_channel(border_coordinator::MAX_COORDINATOR_BATCH);
    HOST_SUBMISSION
        .set((sender, wake.clone()))
        .map_err(|_| anyhow!("host queue already initialized"))?;
    IPC_RELOAD_SUBMISSIONS
        .set(Mutex::new(true))
        .map_err(|_| anyhow!("ipc reload submission gate already initialized"))?;
    Ok(HostRequests { receiver, wake })
}

pub fn submit_host_action(action: HostAction) -> anyhow::Result<()> {
    let (sender, wake) = HOST_SUBMISSION
        .get()
        .ok_or_else(|| anyhow!("host not started"))?;
    sender
        .try_send(action)
        .map_err(|error| anyhow!("host submission rejected: {error}"))?;
    wake.signal()?;
    Ok(())
}

pub fn submit_ipc_reload(
    response: std::sync::mpsc::SyncSender<IpcReloadResponse>,
    context: &command_context::CommandContext,
) -> anyhow::Result<()> {
    let accepting = context.lock(
        IPC_RELOAD_SUBMISSIONS
            .get()
            .ok_or_else(|| anyhow!("ipc reload gate not initialized"))?,
        "ipc reload gate",
    )?;
    anyhow::ensure!(*accepting, "ipc server is being retired");
    submit_host_action(HostAction::ReloadConfig {
        origin: ReloadOrigin::Ipc,
        response: Some(response),
        context: Some(context.clone()),
    })
}

pub fn set_ipc_reload_submissions(enabled: bool) -> anyhow::Result<()> {
    *IPC_RELOAD_SUBMISSIONS
        .get()
        .ok_or_else(|| anyhow!("ipc reload gate not initialized"))?
        .lock()
        .map_err(|_| anyhow!("ipc reload gate poisoned"))? = enabled;
    Ok(())
}

pub struct AppState {
    border_registry: Arc<Mutex<border_registry::BorderRegistry>>,
    lifecycle: Mutex<Option<Arc<border_coordinator::BorderLifecycleCoordinator>>>,
    active_window: Mutex<isize>,
    config: RwLock<RuntimeConfig>,
    config_generation: AtomicU64,
    appearance: Mutex<border_updates::AppearanceSequencer>,
    render_factory: ID2D1Factory1,
    directx_devices: RwLock<Option<DirectXDevices>>,
}

unsafe impl Send for AppState {}
unsafe impl Sync for AppState {}

impl AppState {
    fn new() -> Self {
        let active_window = get_foreground_window().0 as isize;

        let config = match RuntimeConfig::load() {
            Ok(config) => config,
            Err(err) => {
                eprintln!("could not read config: {err:#}");
                thread::spawn(move || {
                    display_error_box(format!("could not read config: {err:#}"), None);
                });

                RuntimeConfig::fallback()
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
            border_registry: Arc::new(Mutex::new(border_registry::BorderRegistry::default())),
            lifecycle: Mutex::new(None),
            active_window: Mutex::new(active_window),
            config: RwLock::new(config),
            config_generation: AtomicU64::new(0),
            appearance: Mutex::new(border_updates::AppearanceSequencer::default()),
            render_factory,
            directx_devices: RwLock::new(directx_devices_opt),
        }
    }

    // The following getter/setters are meant for use in testing
    pub fn start_lifecycle(
        &self,
    ) -> anyhow::Result<Arc<border_coordinator::BorderLifecycleCoordinator>> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| anyhow!("lifecycle owner poisoned"))?;
        if let Some(owner) = lifecycle.as_ref() {
            return Ok(owner.clone());
        }
        let owner =
            border_coordinator::BorderLifecycleCoordinator::start(self.border_registry.clone())?;
        *lifecycle = Some(owner.clone());
        Ok(owner)
    }

    pub(crate) fn lifecycle(
        &self,
    ) -> anyhow::Result<Arc<border_coordinator::BorderLifecycleCoordinator>> {
        self.lifecycle
            .try_lock()
            .map_err(|_| anyhow!("lifecycle owner unavailable"))?
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("lifecycle coordinator not started"))
    }

    pub(crate) fn border_windows(
        &self,
        context: &command_context::CommandContext,
    ) -> anyhow::Result<Vec<(isize, isize)>> {
        let registry = context.lock(&self.border_registry, "border registry")?;
        Ok(registry
            .entries
            .values()
            .filter(|entry| {
                entry.phase == border_registry::BorderPhase::Running && !entry.control.is_stopped()
            })
            .filter_map(|entry| {
                entry
                    .border_hwnd
                    .map(|hwnd| (entry.tracking.fingerprint.hwnd, hwnd))
            })
            .collect())
    }

    pub fn get_config_mut(&self) -> RwLockWriteGuard<'_, RuntimeConfig> {
        let config = self.config.write().unwrap();
        self.advance_config_generation()
            .expect("configuration generations exhausted");
        config
    }

    pub fn update_config(&self, update: impl FnOnce(&mut Config)) -> anyhow::Result<()> {
        let mut config = self
            .config
            .write()
            .map_err(|_| anyhow!("configuration poisoned"))?;
        let mut source = config.source().clone();
        update(&mut source);
        let runtime = RuntimeConfig::from_source(source)?;
        self.advance_config_generation()?;
        *config = runtime;
        Ok(())
    }

    pub(crate) fn config_generation(&self) -> u64 {
        self.config_generation.load(Ordering::Acquire)
    }

    pub(crate) fn publish_config_generation(&self, generation: u64) {
        self.config_generation.store(generation, Ordering::Release);
    }

    pub(crate) fn advance_config_generation(&self) -> anyhow::Result<u64> {
        self.config_generation
            .try_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation.checked_add(1)
            })
            .map(|generation| generation + 1)
            .map_err(|_| anyhow!("configuration generations exhausted"))
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
    config_watcher: Option<ConfigWatcher>,
    komorebi_integration: Option<KomorebiIntegration>,
    theme_watcher: Option<ThemeWatcher>,
    ipc_server: Option<IpcServer>,
    #[allow(dead_code)]
    display_adapters_watcher: Option<DisplayAdaptersWatcher>,
}

unsafe impl Send for BackgroundServices {}
unsafe impl Sync for BackgroundServices {}

enum ServiceTransition<T> {
    Keep,
    Start(T),
    Stop,
}

pub(crate) struct BackgroundServicePlan {
    config_watcher: ServiceTransition<ConfigWatcher>,
    komorebi_integration: ServiceTransition<KomorebiIntegration>,
    theme_watcher: ServiceTransition<ThemeWatcher>,
    ipc_server: ServiceTransition<IpcServer>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServicePresence {
    config_watcher: bool,
    komorebi_integration: bool,
    theme_watcher: bool,
    ipc_server: bool,
}

#[derive(Default)]
pub struct RetiredServices {
    config_watcher: Option<ConfigWatcher>,
    komorebi_integration: Option<KomorebiIntegration>,
    theme_watcher: Option<ThemeWatcher>,
    ipc_server: Option<IpcServer>,
}

#[derive(Default)]
pub struct RetiredResources {
    pub(crate) services: RetiredServices,
    pub(crate) directx_devices: Option<DirectXDevices>,
}

impl RetiredResources {
    pub fn retires_ipc_server(&self) -> bool {
        self.services.retires_ipc_server()
    }

    pub fn retire(self) {
        self.services.retire();
        drop(self.directx_devices);
    }
}

impl RetiredServices {
    pub(crate) fn retires_ipc_server(&self) -> bool {
        self.ipc_server.is_some()
    }

    pub fn retire(self) {
        drop((
            self.config_watcher,
            self.komorebi_integration,
            self.theme_watcher,
            self.ipc_server,
        ));
    }
}

impl BackgroundServices {
    fn new(config: &Config) -> Self {
        let mut config_watcher = None;
        let mut komorebi_integration = None;
        let mut theme_watcher = None;
        let mut ipc_server = None;

        if config.enable_logging
            && let Err(err) = create_logger()
        {
            eprintln!("[ERROR] could not create logger: {err:#}");
        };

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
            config_watcher,
            komorebi_integration,
            theme_watcher,
            ipc_server,
            display_adapters_watcher,
        }
    }

    pub(crate) fn presence(&self) -> ServicePresence {
        ServicePresence {
            config_watcher: self.config_watcher.is_some(),
            komorebi_integration: self.komorebi_integration.is_some(),
            theme_watcher: self.theme_watcher.is_some(),
            ipc_server: self.ipc_server.is_some(),
        }
    }

    pub fn ipc_server_active(&self) -> bool {
        self.ipc_server.is_some()
    }

    pub(crate) fn prepare_reload(
        presence: ServicePresence,
        config: &Config,
        context: &command_context::CommandContext,
    ) -> anyhow::Result<BackgroundServicePlan> {
        context.check()?;
        Ok(BackgroundServicePlan {
            config_watcher: Self::prepare_service(
                presence.config_watcher,
                config.is_config_watcher_enabled(),
                || {
                    context.check()?;
                    create_config_watcher()
                },
            )
            .map_err(|error| error.context("prepare config watcher"))?,
            komorebi_integration: Self::prepare_service(
                presence.komorebi_integration,
                config.is_komorebi_integration_enabled(),
                || {
                    context.check()?;
                    KomorebiIntegration::new()
                },
            )
            .map_err(|error| error.context("prepare komorebi integration"))?,
            theme_watcher: Self::prepare_service(
                presence.theme_watcher,
                config.is_theme_aware_enabled(),
                || {
                    context.check()?;
                    ThemeWatcher::new()
                },
            )
            .map_err(|error| error.context("prepare theme watcher"))?,
            ipc_server: Self::prepare_service(
                presence.ipc_server,
                config.is_ipc_server_enabled(),
                || {
                    context.check()?;
                    ipc::socket_path().and_then(|path| IpcServer::new(&path))
                },
            )
            .map_err(|error| error.context("prepare ipc server"))?,
        })
    }

    pub(crate) fn services_satisfy(presence: ServicePresence, config: &Config) -> bool {
        presence.config_watcher == config.is_config_watcher_enabled()
            && presence.komorebi_integration == config.is_komorebi_integration_enabled()
            && presence.theme_watcher == config.is_theme_aware_enabled()
            && presence.ipc_server == config.is_ipc_server_enabled()
    }

    pub(crate) fn commit_reload(&mut self, plan: BackgroundServicePlan) -> RetiredServices {
        RetiredServices {
            config_watcher: Self::apply_service_transition(
                &mut self.config_watcher,
                plan.config_watcher,
            ),
            komorebi_integration: Self::apply_service_transition(
                &mut self.komorebi_integration,
                plan.komorebi_integration,
            ),
            theme_watcher: Self::apply_service_transition(
                &mut self.theme_watcher,
                plan.theme_watcher,
            ),
            ipc_server: Self::apply_service_transition(&mut self.ipc_server, plan.ipc_server),
        }
    }

    pub fn shutdown(&mut self) {
        *self = Self::default();
    }

    fn prepare_service<T>(
        active: bool,
        desired: bool,
        create: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<ServiceTransition<T>> {
        match (active, desired) {
            (true, true) | (false, false) => Ok(ServiceTransition::Keep),
            (true, false) => Ok(ServiceTransition::Stop),
            (false, true) => create().map(ServiceTransition::Start),
        }
    }

    fn apply_service_transition<T>(
        current: &mut Option<T>,
        transition: ServiceTransition<T>,
    ) -> Option<T> {
        match transition {
            ServiceTransition::Keep => None,
            ServiceTransition::Start(service) => current.replace(service),
            ServiceTransition::Stop => current.take(),
        }
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

                utils::broadcast_border_work(border_runtime::WORK_DISPLAY);
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

#[derive(Clone)]
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

pub fn destroy_borders() -> anyhow::Result<border_coordinator::LifecycleReceipt> {
    APP_STATE.lifecycle()?.request(
        border_coordinator::OperationKind::Stop,
        std::time::Duration::from_secs(10),
    )
}

pub fn reload_borders() -> anyhow::Result<border_coordinator::LifecycleReceipt> {
    APP_STATE.lifecycle()?.request(
        border_coordinator::OperationKind::Reload,
        std::time::Duration::from_secs(10),
    )
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

const DAEMON_MUTEX_NAME: PCWSTR = w!("Local\\tacky-borders-a6b83a75-bebb-46ac-b29f-ea9aa0855fa2");

#[derive(Debug)]
pub enum DaemonInstanceClaim {
    Acquired(DaemonInstanceGuard),
    AlreadyRunning(DaemonInstanceGuard),
}

#[derive(Debug)]
pub struct DaemonInstanceGuard {
    _handle: OwnedHANDLE,
}

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_DAEMON_MUTEX_CLAIM: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
fn fail_next_daemon_mutex_claim_for_test() {
    FAIL_NEXT_DAEMON_MUTEX_CLAIM.set(true);
}

pub fn claim_daemon_instance() -> anyhow::Result<DaemonInstanceClaim> {
    claim_named_instance(DAEMON_MUTEX_NAME)
}

fn claim_named_instance(name: PCWSTR) -> anyhow::Result<DaemonInstanceClaim> {
    #[cfg(test)]
    if FAIL_NEXT_DAEMON_MUTEX_CLAIM.replace(false) {
        anyhow::bail!("injected CreateMutexW failure");
    }

    unsafe { SetLastError(ERROR_SUCCESS) };
    let handle = unsafe { CreateMutexW(None, false, name) }
        .context("could not create daemon instance mutex")?;
    let already_exists = get_last_error() == ERROR_ALREADY_EXISTS;
    let guard = DaemonInstanceGuard {
        _handle: OwnedHANDLE(handle),
    };

    if already_exists {
        Ok(DaemonInstanceClaim::AlreadyRunning(guard))
    } else {
        Ok(DaemonInstanceClaim::Acquired(guard))
    }
}

#[cfg(test)]
mod daemon_instance_claim_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_MUTEX: AtomicU64 = AtomicU64::new(0);

    fn test_mutex_name() -> Vec<u16> {
        format!(
            "Local\\tacky-borders-test-{}-{}",
            std::process::id(),
            NEXT_TEST_MUTEX.fetch_add(1, Ordering::Relaxed)
        )
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
    }

    fn claim_test_mutex(name: &[u16]) -> anyhow::Result<DaemonInstanceClaim> {
        claim_named_instance(PCWSTR(name.as_ptr()))
    }

    #[test]
    fn named_claim_is_exclusive_and_released_with_guard() {
        let name = test_mutex_name();
        let first = claim_test_mutex(&name).unwrap();
        assert!(matches!(&first, DaemonInstanceClaim::Acquired(_)));

        let second = claim_test_mutex(&name).unwrap();
        assert!(matches!(&second, DaemonInstanceClaim::AlreadyRunning(_)));
        drop(first);

        let third = claim_test_mutex(&name).unwrap();
        assert!(matches!(&third, DaemonInstanceClaim::AlreadyRunning(_)));

        drop((second, third));
        let fourth = claim_test_mutex(&name).unwrap();
        assert!(matches!(&fourth, DaemonInstanceClaim::Acquired(_)));
    }

    #[test]
    fn stale_already_exists_error_does_not_reject_a_new_mutex() {
        let name = test_mutex_name();
        unsafe { SetLastError(ERROR_ALREADY_EXISTS) };

        let claim = claim_test_mutex(&name).unwrap();
        assert!(matches!(&claim, DaemonInstanceClaim::Acquired(_)));
    }

    #[test]
    fn simultaneous_claims_have_exactly_one_winner() {
        let name = test_mutex_name();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let workers = (0..2)
            .map(|_| {
                let name = name.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    let claim = claim_test_mutex(&name).unwrap();
                    let acquired = matches!(&claim, DaemonInstanceClaim::Acquired(_));
                    barrier.wait();
                    drop(claim);
                    acquired
                })
            })
            .collect::<Vec<_>>();
        let claims = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(claims.iter().filter(|acquired| **acquired).count(), 1);
        assert_eq!(claims.iter().filter(|acquired| !**acquired).count(), 1);
    }

    #[test]
    fn mutex_creation_failure_is_not_reported_as_duplicate() {
        fail_next_daemon_mutex_claim_for_test();
        let error = claim_daemon_instance().unwrap_err();

        assert!(format!("{error:#}").contains("injected CreateMutexW failure"));
    }
}

thread_local! {
    static ENUMERATION_STATE: RefCell<Option<Vec<isize>>> = const { RefCell::new(None) };
    static ENUMERATION_ACTIVE: Cell<bool> = const { Cell::new(false) };
    static ENUMERATION_FAILED: Cell<bool> = const { Cell::new(false) };
    #[cfg(test)]
    static ENUMERATION_PANIC: Cell<bool> = const { Cell::new(false) };
    #[cfg(test)]
    static ENUMERATION_GROWTH_FAILURE: Cell<bool> = const { Cell::new(false) };
}

struct EnumerationGuard;

impl Drop for EnumerationGuard {
    fn drop(&mut self) {
        let _ = ENUMERATION_STATE.try_with(|state| {
            if let Ok(mut state) = state.try_borrow_mut() {
                *state = None;
            }
        });
        let _ = ENUMERATION_ACTIVE.try_with(|active| active.set(false));
    }
}

fn begin_enumeration() -> WindowsCompatibleResult<EnumerationGuard> {
    if ENUMERATION_ACTIVE.replace(true) {
        return Err(utils::WindowsCompatibleError::Standalone(
            utils::StandaloneWindowsError::new(utils::T_E_REENTRANCY, "nested window enumeration"),
        ));
    }
    let guard = EnumerationGuard;
    ENUMERATION_FAILED.set(false);
    ENUMERATION_STATE.with(|state| -> WindowsCompatibleResult<()> {
        let mut state = state.try_borrow_mut().map_err(|_| {
            utils::WindowsCompatibleError::Standalone(utils::StandaloneWindowsError::new(
                utils::T_E_ERROR,
                "enumeration state busy",
            ))
        })?;
        *state = Some(Vec::new());
        Ok(())
    })?;
    Ok(guard)
}

pub fn enumerate_tracking_windows() -> WindowsCompatibleResult<Vec<HWND>> {
    let _guard = begin_enumeration()?;
    let enumerated = unsafe { EnumWindows(Some(collect_windows_callback), LPARAM(0)) };
    if ENUMERATION_FAILED.get() {
        return Err(utils::WindowsCompatibleError::Standalone(
            utils::StandaloneWindowsError::new(
                utils::T_E_ERROR,
                "window enumeration callback failed",
            ),
        ));
    }
    enumerated?;
    let windows = ENUMERATION_STATE
        .with(|state| state.borrow_mut().take())
        .ok_or_else(|| {
            utils::WindowsCompatibleError::Standalone(utils::StandaloneWindowsError::new(
                utils::T_E_ERROR,
                "window enumeration state missing",
            ))
        })?;
    Ok(windows.into_iter().map(|hwnd| HWND(hwnd as _)).collect())
}

unsafe extern "system" fn collect_windows_callback(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let collected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        #[cfg(test)]
        if ENUMERATION_PANIC.replace(false) {
            panic!("injected enumeration failure");
        }
        if !ENUMERATION_ACTIVE.get() {
            return false;
        }
        ENUMERATION_STATE.with(|state| {
            let Ok(mut state) = state.try_borrow_mut() else {
                return false;
            };
            let Some(windows) = state.as_mut() else {
                return false;
            };
            #[cfg(test)]
            if ENUMERATION_GROWTH_FAILURE.replace(false) {
                return false;
            }
            if windows.try_reserve(1).is_err() {
                return false;
            }
            windows.push(hwnd.0 as isize);
            true
        })
    }));
    match collected {
        Ok(true) => TRUE,
        result => {
            let _ = ENUMERATION_FAILED.try_with(|failed| failed.set(true));
            if let Err(payload) = result {
                border_registry::discard_panic(payload);
            }
            FALSE
        }
    }
}

#[cfg(test)]
mod reload_service_plan_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn service_plan_reuses_active_and_stops_only_at_commit() {
        let mut active = Some(String::from("running"));
        let keep = BackgroundServices::prepare_service(active.is_some(), true, || {
            panic!("an active service must be reused")
        })
        .unwrap();
        assert!(matches!(&keep, ServiceTransition::Keep));
        assert!(BackgroundServices::apply_service_transition(&mut active, keep).is_none());
        assert_eq!(active.as_deref(), Some("running"));

        let stop = BackgroundServices::prepare_service(active.is_some(), false, || {
            panic!("a stopping service must not be recreated")
        })
        .unwrap();
        assert_eq!(active.as_deref(), Some("running"));
        let retired = BackgroundServices::apply_service_transition(&mut active, stop);
        assert_eq!(retired.as_deref(), Some("running"));
        assert!(active.is_none());
    }

    #[test]
    fn failed_later_preparation_drops_earlier_prepared_service() {
        let drops = Arc::new(AtomicUsize::new(0));
        let prepared =
            BackgroundServices::prepare_service(false, true, || Ok(DropProbe(drops.clone())))
                .unwrap();
        let later: anyhow::Result<ServiceTransition<()>> =
            BackgroundServices::prepare_service(false, true, || {
                anyhow::bail!("injected start failure")
            });
        assert!(later.is_err());
        drop(prepared);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn service_plan_starts_only_when_target_is_enabled_and_absent() {
        let calls = AtomicUsize::new(0);
        let start = BackgroundServices::prepare_service(false, true, || {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(42)
        })
        .unwrap();
        assert!(matches!(start, ServiceTransition::Start(42)));
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        let keep = BackgroundServices::prepare_service(false, false, || {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(7)
        })
        .unwrap();
        assert!(matches!(keep, ServiceTransition::Keep));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}

#[cfg(test)]
mod enumeration_tests {
    use super::*;

    #[test]
    fn nested_enumeration_does_not_replace_outer_collection() {
        let guard = begin_enumeration().unwrap();
        assert_eq!(
            unsafe { collect_windows_callback(HWND(1 as _), LPARAM(usize::MAX as _)) },
            TRUE
        );
        assert!(begin_enumeration().is_err());
        assert!(ENUMERATION_ACTIVE.get());
        assert_eq!(
            ENUMERATION_STATE.with(|state| state.borrow().as_ref().unwrap().clone()),
            vec![1]
        );
        assert!(!ENUMERATION_FAILED.get());
        drop(guard);
        assert!(!ENUMERATION_ACTIVE.get());
    }

    #[test]
    fn callback_panic_and_borrow_failure_reset_at_next_invocation() {
        let guard = begin_enumeration().unwrap();
        ENUMERATION_PANIC.set(true);
        assert_eq!(
            unsafe { collect_windows_callback(HWND(1 as _), LPARAM(1)) },
            FALSE
        );
        assert!(ENUMERATION_FAILED.get());
        drop(guard);
        let guard = begin_enumeration().unwrap();
        assert!(!ENUMERATION_FAILED.get());
        ENUMERATION_STATE.with(|state| {
            let _borrow = state.borrow_mut();
            assert_eq!(
                unsafe { collect_windows_callback(HWND(2 as _), LPARAM(1)) },
                FALSE
            );
        });
        assert!(ENUMERATION_FAILED.get());
        drop(guard);
        let _guard = begin_enumeration().unwrap();
        assert!(!ENUMERATION_FAILED.get());
        assert_eq!(
            unsafe { collect_windows_callback(HWND(3 as _), LPARAM(1)) },
            TRUE
        );
    }

    #[test]
    fn collection_growth_failure_discards_partial_enumeration_and_resets() {
        let guard = begin_enumeration().unwrap();
        assert_eq!(
            unsafe { collect_windows_callback(HWND(1 as _), LPARAM(0)) },
            TRUE
        );
        ENUMERATION_GROWTH_FAILURE.set(true);
        assert_eq!(
            unsafe { collect_windows_callback(HWND(2 as _), LPARAM(0)) },
            FALSE
        );
        assert!(ENUMERATION_FAILED.get());
        drop(guard);
        assert!(ENUMERATION_STATE.with(|state| state.borrow().is_none()));
        let _guard = begin_enumeration().unwrap();
        assert!(!ENUMERATION_FAILED.get());
        assert_eq!(
            unsafe { collect_windows_callback(HWND(3 as _), LPARAM(0)) },
            TRUE
        );
        assert_eq!(
            ENUMERATION_STATE.with(|state| state.borrow().as_ref().unwrap().clone()),
            vec![3]
        );
    }
}
