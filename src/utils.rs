use anyhow::{Context, anyhow};
use std::borrow::Cow;
use std::cell::Cell;
use std::error::Error;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::{fs, ptr, time};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ENVVAR_NOT_FOUND, ERROR_INVALID_WINDOW_HANDLE, ERROR_SUCCESS, GetLastError,
    HANDLE, HWND, LPARAM, RECT, SetLastError, WIN32_ERROR, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_CLOAKED, DWMWA_WINDOW_CORNER_PREFERENCE,
    DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use windows::Win32::System::Diagnostics::Debug::FACILITY_ITF;
use windows::Win32::System::Registry::{HKEY, RegCloseKey};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, GetDpiForMonitor, MONITOR_DPI_TYPE, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::Ime::ImmDisableIME;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyWindow, GWL_EXSTYLE, GWL_STYLE, GetForegroundWindow, GetWindowLongW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowArranged, IsWindowVisible, PostMessageW,
    RealGetWindowClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WS_CHILD, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_WINDOWEDGE, WS_MAXIMIZE,
};
use windows::core::{BOOL, HRESULT, PWSTR};

use crate::APP_STATE;
use crate::config::{EnableMode, MatchKind, WindowRule};
use crate::event_hook::handle_foreground_event;

pub const WM_APP_LOCATIONCHANGE: u32 = WM_APP;
pub const WM_APP_REORDER: u32 = WM_APP + 1;
pub const WM_APP_FOREGROUND: u32 = WM_APP + 2;
pub const WM_APP_SHOWUNCLOAKED: u32 = WM_APP + 3;
pub const WM_APP_HIDECLOAKED: u32 = WM_APP + 4;
pub const WM_APP_MINIMIZESTART: u32 = WM_APP + 5;
pub const WM_APP_MINIMIZEEND: u32 = WM_APP + 6;
pub const WM_APP_ANIMATE: u32 = WM_APP + 7;
pub const WM_APP_KOMOREBI: u32 = WM_APP + 8;
pub const WM_APP_RECREATE_DRAWER: u32 = WM_APP + 9;

// T_E_UNINIT indicates an uninitialized object, T_E_ERROR indicates a general error, and
// T_E_REENTRANCY indicates re-entrancy where there shouldn't have been any. These custom HRESULTs
// are used to help distinguish between this application's errors and Windows' COM interface
// errors. The codes used are arbitrary, but are within Microsoft's recommended range for custom
// FACILITY_ITF HRESULTs (0x0200 to 0xFFFF).
pub const T_E_UNINIT: HRESULT = HRESULT((1 << 31) | ((FACILITY_ITF.0 as i32) << 16) | (0x2222));
pub const T_E_ERROR: HRESULT = HRESULT((1 << 31) | ((FACILITY_ITF.0 as i32) << 16) | (0x2223));
pub const T_E_REENTRANCY: HRESULT = HRESULT((1 << 31) | ((FACILITY_ITF.0 as i32) << 16) | (0x2224));

pub trait LogIfErr {
    fn log_if_err(&self);
}
impl<T> LogIfErr for Result<(), T>
where
    T: std::fmt::Display,
{
    fn log_if_err(&self) {
        if let Err(err) = self {
            // NOTE: This will not print out the chain of errors unless using anyhow::Result or
            // WindowsCompatibleResult due to how they implement Display when using ':#'
            error!("{err:#}");
        }
    }
}

// Used to convert an already existing error to a Windows-style error (with HRESULT codes)
#[derive(Debug)]
pub struct WrappedWindowsError {
    code: HRESULT,
    inner: Box<dyn std::error::Error + Send + Sync>,
}

impl WrappedWindowsError {
    pub fn new(code: HRESULT, inner: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Self { code, inner }
    }

    pub fn code(&self) -> HRESULT {
        self.code
    }

    pub fn message(&self) -> Cow<'_, str> {
        Cow::Owned(self.inner.to_string())
    }
}

// Used to construct a standalone Windows-style error with an optional source chain
#[derive(Debug)]
pub struct StandaloneWindowsError {
    code: HRESULT,
    message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl StandaloneWindowsError {
    pub fn new<T>(code: HRESULT, message: T) -> Self
    where
        T: std::fmt::Display,
    {
        Self {
            code,
            message: message.to_string(),
            source: None,
        }
    }

    pub fn with_source<T>(
        code: HRESULT,
        message: T,
        source: Box<dyn std::error::Error + Send + Sync>,
    ) -> Self
    where
        T: std::fmt::Display,
    {
        Self {
            code,
            message: message.to_string(),
            source: Some(source),
        }
    }

    pub fn code(&self) -> HRESULT {
        self.code
    }

    pub fn message(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.message)
    }
}

#[derive(Debug)]
pub enum WindowsCompatibleError {
    Wrapped(WrappedWindowsError),
    Standalone(StandaloneWindowsError),
}

impl WindowsCompatibleError {
    pub fn code(&self) -> HRESULT {
        match self {
            WindowsCompatibleError::Wrapped(err) => err.code(),
            WindowsCompatibleError::Standalone(err) => err.code(),
        }
    }

    pub fn message(&self) -> Cow<'_, str> {
        match self {
            WindowsCompatibleError::Wrapped(err) => err.message(),
            WindowsCompatibleError::Standalone(err) => err.message(),
        }
    }
}

impl std::error::Error for WindowsCompatibleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WindowsCompatibleError::Wrapped(err) => err.inner.source(),
            WindowsCompatibleError::Standalone(err) => err
                .source
                .as_deref()
                .map(|err| err as &dyn std::error::Error),
        }
    }
}

impl std::fmt::Display for WindowsCompatibleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "{}", self.message())?;

        if f.alternate() {
            let mut source_opt = self.source();
            while let Some(source) = source_opt {
                write!(f, ": {source}")?;
                source_opt = source.source();
            }
        }

        Ok(())
    }
}

impl From<windows::core::Error> for WindowsCompatibleError {
    fn from(value: windows::core::Error) -> WindowsCompatibleError {
        WindowsCompatibleError::Wrapped(WrappedWindowsError::new(value.code(), Box::new(value)))
    }
}

// A custom Result type that is compatible with Windows-style errors (with HRESULT codes)
pub type WindowsCompatibleResult<T> = Result<T, WindowsCompatibleError>;

pub trait ToWindowsResult<T> {
    fn to_windows_result(self, hresult: HRESULT) -> WindowsCompatibleResult<T>;
}

impl<T> ToWindowsResult<T> for anyhow::Result<T> {
    fn to_windows_result(self, hresult: HRESULT) -> WindowsCompatibleResult<T> {
        match self {
            Ok(ok) => Ok(ok),
            Err(err) => Err(WindowsCompatibleError::Wrapped(WrappedWindowsError::new(
                hresult,
                err.into_boxed_dyn_error(),
            ))),
        }
    }
}

// Basically anyhow's Context, but named differently and preserves Windows' HRESULT codes
pub trait WindowsContext<T> {
    fn windows_context<C>(self, context: C) -> WindowsCompatibleResult<T>
    where
        C: std::fmt::Display + Send + Sync + 'static;

    fn with_windows_context<C, F>(self, context: F) -> WindowsCompatibleResult<T>
    where
        C: std::fmt::Display + Send + Sync + 'static,
        F: FnOnce() -> C;
}

impl<T> WindowsContext<T> for windows::core::Result<T> {
    fn windows_context<C>(self, context: C) -> WindowsCompatibleResult<T>
    where
        C: std::fmt::Display + Send + Sync + 'static,
    {
        match self {
            Ok(ok) => Ok(ok),
            Err(err) => Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::with_source(err.code(), context.to_string(), Box::new(err)),
            )),
        }
    }

    fn with_windows_context<C, F>(self, context: F) -> WindowsCompatibleResult<T>
    where
        C: std::fmt::Display + Send + Sync + 'static,
        F: FnOnce() -> C,
    {
        match self {
            Ok(ok) => Ok(ok),
            Err(err) => Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::with_source(
                    err.code(),
                    context().to_string(),
                    Box::new(err),
                ),
            )),
        }
    }
}

impl<T> WindowsContext<T> for WindowsCompatibleResult<T> {
    fn windows_context<C>(self, context: C) -> WindowsCompatibleResult<T>
    where
        C: std::fmt::Display + Send + Sync + 'static,
    {
        match self {
            Ok(ok) => Ok(ok),
            Err(err) => Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::with_source(err.code(), context.to_string(), Box::new(err)),
            )),
        }
    }

    fn with_windows_context<C, F>(self, context: F) -> WindowsCompatibleResult<T>
    where
        C: std::fmt::Display + Send + Sync + 'static,
        F: FnOnce() -> C,
    {
        match self {
            Ok(ok) => Ok(ok),
            Err(err) => Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::with_source(
                    err.code(),
                    context().to_string(),
                    Box::new(err),
                ),
            )),
        }
    }
}

pub struct ReentrancyBlockerGuard {
    local_key: &'static std::thread::LocalKey<ReentrancyBlocker>,
}

impl Drop for ReentrancyBlockerGuard {
    fn drop(&mut self) {
        self.local_key.with(|blocker| blocker.is_entered.set(false));
    }
}

pub struct ReentrancyBlocker {
    is_entered: Cell<bool>,
}

impl ReentrancyBlocker {
    pub fn new() -> Self {
        Self {
            is_entered: Cell::new(false),
        }
    }
}

impl Default for ReentrancyBlocker {
    fn default() -> Self {
        Self::new()
    }
}

pub trait ReentrancyBlockerExt {
    fn enter(&'static self) -> anyhow::Result<ReentrancyBlockerGuard>;
}

impl ReentrancyBlockerExt for std::thread::LocalKey<ReentrancyBlocker> {
    fn enter(&'static self) -> anyhow::Result<ReentrancyBlockerGuard> {
        if self.with(|blocker| blocker.is_entered.get()) {
            return Err(anyhow!("detected re-entrancy"));
        }
        self.with(|blocker| blocker.is_entered.set(true));

        Ok(ReentrancyBlockerGuard { local_key: self })
    }
}

// Prevents mutation while locked
#[derive(Debug, Default, Clone)]
pub struct WriteLockable<T> {
    inner: T,
    is_locked: bool,
}

impl<T> WriteLockable<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: value,
            is_locked: false,
        }
    }

    pub fn lock_writes(&mut self) {
        self.is_locked = true;
    }

    pub fn unlock_writes(&mut self) {
        self.is_locked = false;
    }

    pub fn is_locked(&self) -> bool {
        self.is_locked
    }

    pub fn get(&self) -> &T {
        &self.inner
    }

    pub fn set(&mut self, value: T) -> anyhow::Result<()> {
        if self.is_locked {
            return Err(anyhow!("attempted to set a value while it is locked"));
        } else {
            self.inner = value;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct OwnedHANDLE(pub HANDLE);

impl Drop for OwnedHANDLE {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) }
            .with_context(|| format!("could not close {:?}", self.0))
            .log_if_err();
    }
}

#[derive(Debug)]
pub struct OwnedHKEY(pub HKEY);

impl Drop for OwnedHKEY {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) }
            .ok()
            .with_context(|| format!("could not close {:?}", self.0))
            .log_if_err();
    }
}

// NOTE: This struct must stay on the thread that created the window because DestroyWindow only
// works on windows created by the calling thread.
#[derive(Debug)]
pub struct OwnedHWND(pub HWND);

impl OwnedHWND {
    pub fn destroy(&mut self) -> windows::core::Result<()> {
        self.destroy_with(|hwnd| unsafe { DestroyWindow(hwnd) })
    }

    fn destroy_with(
        &mut self,
        destroy: impl FnOnce(HWND) -> windows::core::Result<()>,
    ) -> windows::core::Result<()> {
        if self.0.is_invalid() {
            return Ok(());
        }
        destroy(self.0)?;
        self.0 = HWND::default();
        Ok(())
    }
}

impl Drop for OwnedHWND {
    fn drop(&mut self) {
        let _ = self.destroy();
    }
}

pub fn get_window_style(hwnd: HWND) -> WINDOW_STYLE {
    unsafe { WINDOW_STYLE(GetWindowLongW(hwnd, GWL_STYLE) as u32) }
}

pub fn get_window_ex_style(hwnd: HWND) -> WINDOW_EX_STYLE {
    unsafe { WINDOW_EX_STYLE(GetWindowLongW(hwnd, GWL_EXSTYLE) as u32) }
}

pub fn is_window_top_level(hwnd: HWND) -> bool {
    let style = get_window_style(hwnd);

    !style.contains(WS_CHILD)
}

pub fn has_filtered_style(hwnd: HWND) -> bool {
    let ex_style = get_window_ex_style(hwnd);

    ex_style.contains(WS_EX_TOOLWINDOW) || ex_style.contains(WS_EX_NOACTIVATE)
}

pub fn get_window_title(hwnd: HWND) -> anyhow::Result<String> {
    let mut title_buf: [u16; 256] = [0; 256];

    if unsafe { GetWindowTextW(hwnd, &mut title_buf) } == 0 {
        let last_error = get_last_error();

        // ERROR_ENVVAR_NOT_FOUND just means the title is empty which isn't necessarily an issue
        // TODO: figure out whats with the invalid window handles
        if !matches!(
            last_error,
            ERROR_ENVVAR_NOT_FOUND | ERROR_SUCCESS | ERROR_INVALID_WINDOW_HANDLE
        ) {
            // We manually reset LastError here because it doesn't seem to reset by itself
            unsafe { SetLastError(ERROR_SUCCESS) };
            return Err(anyhow!("{last_error:?}"));
        }
    }

    let title_binding = String::from_utf16_lossy(&title_buf);
    Ok(title_binding.split_once("\0").unwrap().0.to_string())
}

pub fn get_window_class(hwnd: HWND) -> anyhow::Result<String> {
    let mut class_buf: [u16; 256] = [0; 256];

    if unsafe { RealGetWindowClassW(hwnd, &mut class_buf) } == 0 {
        let last_error = get_last_error();

        // ERROR_ENVVAR_NOT_FOUND just means the title is empty which isn't necessarily an issue
        // TODO: figure out whats with the invalid window handles
        if !matches!(
            last_error,
            ERROR_ENVVAR_NOT_FOUND | ERROR_SUCCESS | ERROR_INVALID_WINDOW_HANDLE
        ) {
            // We manually reset LastError here because it doesn't seem to reset by itself
            unsafe { SetLastError(ERROR_SUCCESS) };
            return Err(anyhow!("{last_error:?}"));
        }
    }

    let class_binding = String::from_utf16_lossy(&class_buf);
    Ok(class_binding.split_once("\0").unwrap().0.to_string())
}

pub fn get_window_process_name(hwnd: HWND) -> anyhow::Result<String> {
    let mut process_id = 0;
    // This function returns the thread id. If it's 0, that means we likely passed an invalid hwnd.
    if unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) } == 0 {
        return Err(anyhow!(
            "could not get thread and process id for {hwnd:?}: {:?}",
            get_last_error()
        ));
    }

    let hprocess = {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
            .with_context(|| format!("could not open process of {hwnd:?}"))?;
        OwnedHANDLE(handle)
    };

    // TODO: Maybe increase buffer sizes
    let mut process_buf = [0u16; 256];
    let mut lpdwsize = process_buf.len() as u32;

    unsafe {
        QueryFullProcessImageNameW(
            hprocess.0,
            PROCESS_NAME_WIN32,
            PWSTR(process_buf.as_mut_ptr()),
            &mut lpdwsize,
        )
    }
    .with_context(|| format!("could not query process image name for {hwnd:?}"))?;

    // QueryFullProcessImageNameW will update lpdwsize with the number of characters written
    // (excluding the null terminating char), so if it's about the same as the size of our buffer,
    // it means we may not have gotten all of it
    if lpdwsize >= process_buf.len() as u32 - 1 {
        warn!("process buffer size too small; truncation may occur");
    }

    let exe_path = PathBuf::from(OsString::from_wide(&process_buf[..lpdwsize as usize]));

    Ok(exe_path
        .file_stem()
        .context("could not get exe file stem from process path")?
        .to_string_lossy()
        .into_owned())
}

pub(crate) fn shared_snapshot_error<Value>(
    error: std::sync::TryLockError<Value>,
) -> WindowsCompatibleError {
    let code = match error {
        std::sync::TryLockError::WouldBlock => T_E_REENTRANCY,
        std::sync::TryLockError::Poisoned(_) => T_E_ERROR,
    };
    WindowsCompatibleError::Standalone(StandaloneWindowsError::new(
        code,
        "shared snapshot unavailable",
    ))
}

pub fn get_window_rule_from_config(
    hwnd: HWND,
    config: &crate::config::RuntimeConfig,
) -> WindowRule {
    let mut title_opt: Option<String> = None;
    let mut class_opt: Option<String> = None;
    let mut process_opt: Option<String> = None;

    let title = || -> String {
        get_window_title(hwnd).unwrap_or_else(|err| {
            error!("could not retrieve window title for {hwnd:?}: {err:#}");
            "".to_string()
        })
    };
    let class = || -> String {
        get_window_class(hwnd).unwrap_or_else(|err| {
            error!("could not retrieve window class for {hwnd:?}: {err:#}");
            "".to_string()
        })
    };
    let process = || -> String {
        get_window_process_name(hwnd).unwrap_or_else(|err| {
            error!("could not retrieve window process name for {hwnd:?}: {err:#}");
            "".to_string()
        })
    };

    for rule in config.window_rules() {
        let window_name: &String = match rule.kind() {
            MatchKind::Title => title_opt.get_or_insert_with(title),
            MatchKind::Class => class_opt.get_or_insert_with(class),
            MatchKind::Process => process_opt.get_or_insert_with(process),
        };

        if rule.is_match(window_name) {
            return rule.source().clone();
        }
    }

    WindowRule::default()
}

pub fn are_rects_same_size(rect1: &RECT, rect2: &RECT) -> bool {
    rect1.right - rect1.left == rect2.right - rect2.left
        && rect1.bottom - rect1.top == rect2.bottom - rect2.top
}

pub fn is_window(hwnd: Option<HWND>) -> bool {
    unsafe { IsWindow(hwnd).as_bool() }
}

pub fn is_window_visible(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd).as_bool() }
}

pub fn is_window_cloaked(hwnd: HWND) -> bool {
    let mut cloaked_state = 0u32;
    if let Err(err) = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            ptr::addr_of_mut!(cloaked_state) as _,
            size_of::<u32>() as u32,
        )
    } {
        // I've never seen this fail, so I think returning a default value is better than a Result
        // type which would sacrifice ergonomics elsewhere in the code
        error!("could not check if window is cloaked: {err:#}");
        return false;
    }

    cloaked_state != 0
}

pub fn is_window_minimized(hwnd: HWND) -> bool {
    unsafe { IsIconic(hwnd).as_bool() }
}

pub fn is_window_arranged(hwnd: HWND) -> bool {
    unsafe { IsWindowArranged(hwnd).as_bool() }
}

pub fn get_foreground_window() -> HWND {
    unsafe { GetForegroundWindow() }
}

pub fn post_message_w(
    hwnd: Option<HWND>,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> windows::core::Result<()> {
    unsafe { PostMessageW(hwnd, msg, wparam, lparam) }
}

pub fn imm_disable_ime(param0: u32) -> BOOL {
    unsafe { ImmDisableIME(param0) }
}

pub fn set_process_dpi_awareness_context(
    value: DPI_AWARENESS_CONTEXT,
) -> windows::core::Result<()> {
    unsafe { SetProcessDpiAwarenessContext(value) }
}

pub fn has_native_border(hwnd: HWND) -> bool {
    let style = get_window_style(hwnd);
    let ex_style = get_window_ex_style(hwnd);

    !style.contains(WS_MAXIMIZE) && ex_style.contains(WS_EX_WINDOWEDGE)
}

pub fn create_border_for_window(tracking_window: HWND, window_rule: WindowRule) {
    if window_rule.enabled == Some(EnableMode::Bool(false)) {
        return;
    }
    let result = (|| -> anyhow::Result<()> {
        let fingerprint =
            crate::border_registry::TrackingWindowFingerprint::capture(tracking_window)
                .ok_or_else(|| anyhow!("tracking window vanished"))?;
        let updates = std::sync::Arc::new(crate::border_updates::BorderUpdateMailbox::new(
            std::sync::Arc::new(crate::border_registry::BorderControl::new()?),
        )?);
        let owner = APP_STATE.lifecycle()?;
        {
            let mut registry = crate::border_coordinator::try_registry(&APP_STATE.border_registry)
                .ok_or_else(|| anyhow!("registry busy"))?;
            let identity = registry
                .observe(fingerprint)
                .map_err(|error| anyhow!("observation: {error:?}"))?;
            if let Some(observation) = registry.observations.get_mut(&identity.observation) {
                observation.border_desired = true;
            }
            match registry.reserve(identity, updates, time::Instant::now()) {
                Ok(_)
                | Err(
                    crate::border_registry::RegistryError::Closed
                    | crate::border_registry::RegistryError::RetryNotDue,
                ) => {}
                Err(error) => return Err(anyhow!("reservation: {error:?}")),
            }
        }
        owner.wake();
        Ok(())
    })();
    result.log_if_err();
}

pub fn get_adjusted_radius(radius: f32, dpi: u32, border_width: i32) -> f32 {
    const BASE_DPI: f32 = 96.0; // Standard DPI on Windows for 100% scaling

    if radius == 0.0 {
        0.0
    } else {
        radius * dpi as f32 / BASE_DPI + (border_width as f32 / 2.0)
    }
}

pub fn get_window_corner_preference(
    tracking_window: HWND,
) -> anyhow::Result<DWM_WINDOW_CORNER_PREFERENCE> {
    let mut corner_preference = DWM_WINDOW_CORNER_PREFERENCE::default();

    unsafe {
        DwmGetWindowAttribute(
            tracking_window,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            ptr::addr_of_mut!(corner_preference) as _,
            size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        )
    }
    .context("could not retrieve window corner preference")?;

    Ok(corner_preference)
}

pub fn get_dpi_for_monitor(hmonitor: HMONITOR, dpitype: MONITOR_DPI_TYPE) -> anyhow::Result<u32> {
    let (mut dpi_x, mut dpi_y) = (0, 0);
    unsafe { GetDpiForMonitor(hmonitor, dpitype, &mut dpi_x, &mut dpi_y) }?;

    // According to the docs, dpi_x and dpi_y will always be identical, so we only need one
    Ok(dpi_x)
}

pub fn monitor_from_window(hwnd: HWND) -> HMONITOR {
    unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) }
}

pub fn get_monitor_info(hmonitor: HMONITOR) -> windows::core::Result<MONITORINFO> {
    let mut mi = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };

    if !unsafe { GetMonitorInfoW(hmonitor, &mut mi) }.as_bool() {
        return Err(windows::core::Error::new(
            T_E_ERROR,
            format!(
                "could not get monitor info for {:?}: {:?}",
                hmonitor,
                get_last_error()
            ),
        ));
    };

    Ok(mi)
}

pub fn destroy_border_for_window(tracking_window: HWND) {
    let Some(mut registry) = crate::border_coordinator::try_registry(&APP_STATE.border_registry)
    else {
        crate::event_hook::invalidate_history();
        return;
    };
    let actions =
        registry.retire_observation(tracking_window.0 as isize, None, time::Instant::now());
    drop(registry);
    for action in actions {
        action.signal().map_err(anyhow::Error::from).log_if_err();
    }
    if let Ok(owner) = APP_STATE.lifecycle() {
        owner.wake();
    }
}

pub fn show_border_for_window(hwnd: HWND) {
    if set_border_visibility(hwnd, crate::border_registry::DesiredVisibility::Visible) {
        return;
    }
    if is_window_top_level(hwnd) && is_window_visible(hwnd) && !is_window_cloaked(hwnd) {
        let config = match APP_STATE.config.try_read() {
            Ok(config) => config.clone(),
            Err(_) => {
                crate::event_hook::invalidate_history();
                return;
            }
        };
        let window_rule = get_window_rule_from_config(hwnd, &config);

        if window_rule.enabled == Some(EnableMode::Bool(false)) {
            info!("border is disabled for {hwnd:?}");
        } else if window_rule.enabled == Some(EnableMode::Bool(true)) || !has_filtered_style(hwnd) {
            create_border_for_window(hwnd, window_rule);
        }
    }
}

pub fn hide_border_for_window(hwnd: HWND) {
    set_border_visibility(hwnd, crate::border_registry::DesiredVisibility::Hidden);
}

fn target_updates(
    hwnd: HWND,
) -> Option<std::sync::Arc<crate::border_updates::BorderUpdateMailbox>> {
    let fingerprint = crate::border_registry::TrackingWindowFingerprint::capture(hwnd)?;
    let registry = crate::border_coordinator::try_registry(&APP_STATE.border_registry)?;
    let entry = registry
        .entries
        .get(registry.current_by_hwnd.get(&fingerprint.hwnd)?)?;
    (entry.tracking.fingerprint == fingerprint).then(|| entry.updates.clone())
}

pub(crate) fn request_border_work(hwnd: HWND, bits: u8) {
    if let Some(updates) = target_updates(hwnd) {
        updates.hint(bits).log_if_err();
    }
}

static BROADCAST_WORK: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub(crate) fn broadcast_border_work(bits: u8) {
    BROADCAST_WORK.fetch_or(bits, std::sync::atomic::Ordering::AcqRel);
    flush_border_work();
}

pub fn flush_border_work() {
    flush_work(&APP_STATE.border_registry, &BROADCAST_WORK);
}

fn flush_work(
    registry: &std::sync::Mutex<crate::border_registry::BorderRegistry>,
    pending: &std::sync::atomic::AtomicU8,
) {
    use std::sync::atomic::Ordering;
    if pending.load(Ordering::Acquire) == 0 {
        return;
    }
    let Some(registry) = crate::border_coordinator::try_registry(registry) else {
        return;
    };
    let targets = registry
        .entries
        .values()
        .map(|entry| entry.updates.clone())
        .collect::<Vec<_>>();
    let bits = pending.swap(0, Ordering::AcqRel);
    drop(registry);
    for updates in targets {
        if !updates.control().is_stopped() {
            updates.hint(bits).log_if_err();
        }
    }
}

fn set_border_visibility(
    hwnd: HWND,
    visibility: crate::border_registry::DesiredVisibility,
) -> bool {
    let Some(updates) = target_updates(hwnd) else {
        return false;
    };
    updates.control().set_visibility(visibility);
    updates
        .hint(crate::border_runtime::WORK_VISIBILITY)
        .log_if_err();
    true
}

pub struct WindowReconciler {
    pending: std::collections::VecDeque<(
        isize,
        Option<crate::border_registry::TrackingWindowIdentity>,
    )>,
    next_poll: time::Instant,
    next_discovery: time::Instant,
    retry_at: time::Instant,
}

impl Default for WindowReconciler {
    fn default() -> Self {
        let now = time::Instant::now();
        Self {
            pending: Default::default(),
            next_poll: now,
            next_discovery: now,
            retry_at: now,
        }
    }
}

impl WindowReconciler {
    pub fn tick(&mut self, now: time::Instant) -> bool {
        self.tick_with_discovery(now, crate::enumerate_tracking_windows)
    }

    fn tick_with_discovery(
        &mut self,
        now: time::Instant,
        discover: impl FnOnce() -> WindowsCompatibleResult<Vec<HWND>>,
    ) -> bool {
        use crate::border_registry::{
            BorderAdmission, DesiredVisibility, TrackingWindowFingerprint,
        };
        if now < self.retry_at {
            return false;
        }
        let Some(registry) = crate::border_coordinator::try_registry(&APP_STATE.border_registry)
        else {
            self.retry_at = now + time::Duration::from_millis(10);
            return false;
        };
        if registry.admission != BorderAdmission::Open
            || registry.quarantined
            || registry.shutdown_requested
        {
            self.pending.clear();
            return false;
        }
        if self.pending.is_empty() && now >= self.next_poll {
            self.pending.extend(
                registry
                    .observations
                    .values()
                    .filter(|observation| observation.live)
                    .map(|observation| {
                        (
                            observation.identity.fingerprint.hwnd,
                            Some(observation.identity),
                        )
                    }),
            );
            self.next_poll = now + time::Duration::from_millis(100);
        }
        drop(registry);
        let foreground = get_foreground_window();
        let previous = APP_STATE
            .active_window
            .try_lock()
            .ok()
            .map(|active| *active);
        if previous.is_some_and(|previous| previous != foreground.0 as isize)
            && !foreground.is_invalid()
        {
            handle_foreground_event(foreground, HWND::default());
        }
        if now >= self.next_discovery {
            self.next_discovery = now + time::Duration::from_secs(1);
            match discover() {
                Ok(windows) => self
                    .pending
                    .extend(windows.into_iter().map(|hwnd| (hwnd.0 as isize, None))),
                Err(error) => {
                    error!("window discovery failed: {error:#}");
                    crate::event_hook::invalidate_history();
                    return false;
                }
            }
        }
        for _ in 0..crate::border_coordinator::MAX_COORDINATOR_BATCH {
            let Some(candidate @ (hwnd, recorded)) = self.pending.pop_front() else {
                break;
            };
            let captured = TrackingWindowFingerprint::capture(HWND(hwnd as _));
            if recorded.is_some_and(|identity| captured != Some(identity.fingerprint)) {
                let Some(mut registry) =
                    crate::border_coordinator::try_registry(&APP_STATE.border_registry)
                else {
                    self.pending.push_front(candidate);
                    self.retry_at = now + time::Duration::from_millis(10);
                    return false;
                };
                let actions = if recorded.is_some_and(|identity| {
                    registry.observation_by_hwnd.get(&hwnd) == Some(&identity.observation)
                }) {
                    registry.retire_observation(hwnd, None, now)
                } else {
                    Vec::new()
                };
                drop(registry);
                for action in actions {
                    action.signal().map_err(anyhow::Error::from).log_if_err();
                }
                if let Ok(owner) = APP_STATE.lifecycle() {
                    owner.wake();
                }
                continue;
            }
            let Some(fingerprint) = captured else {
                continue;
            };
            if recorded.is_none()
                && fingerprint.process_id
                    == unsafe { windows::Win32::System::Threading::GetCurrentProcessId() }
            {
                continue;
            }
            let config = match APP_STATE.config.try_read() {
                Ok(config) => config.clone(),
                Err(_) => {
                    self.pending.push_front(candidate);
                    self.retry_at = now + time::Duration::from_millis(10);
                    return false;
                }
            };
            let tracking = HWND(hwnd as _);
            let rule = get_window_rule_from_config(tracking, &config);
            let eligible = is_window_top_level(tracking)
                && is_window_visible(tracking)
                && !is_window_cloaked(tracking)
                && !is_window_minimized(tracking)
                && rule.enabled != Some(EnableMode::Bool(false))
                && (rule.enabled == Some(EnableMode::Bool(true)) || !has_filtered_style(tracking));
            let Some(mut registry) =
                crate::border_coordinator::try_registry(&APP_STATE.border_registry)
            else {
                self.pending.push_front(candidate);
                self.retry_at = now + time::Duration::from_millis(10);
                return false;
            };
            if registry.admission != BorderAdmission::Open
                || registry.shutdown_requested
                || registry.quarantined
            {
                continue;
            }
            let identity = if let Some(identity) = recorded {
                if registry.observation_by_hwnd.get(&hwnd) != Some(&identity.observation) {
                    continue;
                }
                identity
            } else {
                match registry.observe(fingerprint) {
                    Ok(identity) => identity,
                    Err(_) => continue,
                }
            };
            let entry = registry
                .current_by_hwnd
                .get(&hwnd)
                .and_then(|id| registry.entries.get(id))
                .filter(|entry| entry.tracking == identity);
            let updates = entry.map(|entry| entry.updates.clone());
            let Some(observation) = registry.observations.get_mut(&identity.observation) else {
                continue;
            };
            observation.border_desired = eligible;
            let reserve = eligible
                && updates.is_none()
                && observation.retry_at.is_none_or(|deadline| now >= deadline);
            drop(registry);
            if let Some(updates) = updates {
                let visibility = if eligible {
                    DesiredVisibility::Visible
                } else {
                    DesiredVisibility::Hidden
                };
                if !updates.control().is_stopped() && updates.control().visibility() != visibility {
                    updates.control().set_visibility(visibility);
                    updates
                        .hint(crate::border_runtime::WORK_VISIBILITY)
                        .log_if_err();
                }
            } else if reserve {
                create_border_for_window(tracking, rule);
            }
        }
        !self.pending.is_empty()
    }
}

pub fn get_last_error() -> WIN32_ERROR {
    unsafe { GetLastError() }
}

pub fn remove_file_if_exists(file_path: &Path) -> anyhow::Result<()> {
    if fs::exists(file_path).context("could not check if file exists")? {
        fs::remove_file(file_path).context("could not remove file")?;
    }

    Ok(())
}

// Bezier curve algorithm together with @0xJWLabs
const SUBDIVISION_PRECISION: f32 = 0.0001; // Precision for binary subdivision
const SUBDIVISION_MAX_ITERATIONS: u32 = 10; // Maximum number of iterations for binary subdivision

#[derive(Debug)]
pub enum BezierError {
    InvalidControlPoint,
}

impl std::fmt::Display for BezierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BezierError::InvalidControlPoint => {
                write!(f, "cubic-bezier control points must be in the range [0, 1]")
            }
        }
    }
}
impl std::error::Error for BezierError {}

struct Point {
    x: f32,
    y: f32,
}

fn lerp(t: f32, p1: f32, p2: f32) -> f32 {
    p1 + (p2 - p1) * t
}

// Compute the cubic Bézier curve using De Casteljau's algorithm.
fn de_casteljau(t: f32, p_i: f32, p1: f32, p2: f32, p_f: f32) -> f32 {
    // First level
    let q1 = lerp(t, p_i, p1);
    let q2 = lerp(t, p1, p2);
    let q3 = lerp(t, p2, p_f);

    // Second level
    let r1 = lerp(t, q1, q2);
    let r2 = lerp(t, q2, q3);

    // Final level
    lerp(t, r1, r2)
}

// Generates a cubic Bézier curve function from control points.
pub fn cubic_bezier(control_points: &[f32; 4]) -> Result<impl Fn(f32) -> f32 + use<>, BezierError> {
    let [x1, y1, x2, y2] = *control_points;

    // Ensure control points are within bounds.
    //
    // I think any y-value for the control points should be fine. But, we can't have negative
    // x-values for the control points, otherwise the cubic bezier function could have multiple
    // outputs for any given input 'x' (different from the control points' x-values), meaning we
    // would have a mathematical non-function.
    if !(0.0..=1.0).contains(&x1) || !(0.0..=1.0).contains(&x2) {
        return Err(BezierError::InvalidControlPoint);
    }

    Ok(move |x: f32| {
        // If the curve is linear, shortcut.
        if x1 == y1 && x2 == y2 {
            return x;
        }

        // Boundary cases
        if x == 0.0 || x == 1.0 {
            return x;
        }

        let mut t0 = 0.0;
        let mut t1 = 1.0;
        let mut t = x;

        let p_i = Point { x: 0.0, y: 0.0 }; // Start point
        let p1 = Point { x: x1, y: y1 }; // First control point
        let p2 = Point { x: x2, y: y2 }; // Second control point
        let p_f = Point { x: 1.0, y: 1.0 }; // End point

        // Search for `t` from the 'x' given as an argument to this function.
        //
        // Note: 'x' and 't' are not the same. 'x' refers to the position along the x-axis, whereas
        // 't' refers to the position along the control point lines, hence why we need to search.
        for _ in 0..SUBDIVISION_MAX_ITERATIONS {
            // Evaluate the x-component of the Bézier curve at `t`
            let x_val = de_casteljau(t, p_i.x, p1.x, p2.x, p_f.x);
            let error = x - x_val;

            // Adjust the range based on the error.
            if error.abs() < SUBDIVISION_PRECISION {
                break;
            }
            if error > 0.0 {
                t0 = t;
            } else {
                t1 = t;
            }
            t = (t0 + t1) / 2.0;
        }

        // After finding 't', evalaute the y-component of the Bezier curve
        de_casteljau(t, p_i.y, p1.y, p2.y, p_f.y)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ReconciliationFixture {
        previous_registry: crate::border_registry::BorderRegistry,
        previous_config: crate::config::RuntimeConfig,
        window: OwnedHWND,
        identity: crate::border_registry::TrackingWindowIdentity,
        instance: crate::border_registry::BorderInstanceId,
        control: std::sync::Arc<crate::border_registry::BorderControl>,
        reconciler: WindowReconciler,
        now: time::Instant,
        coordinator: Option<crate::border_coordinator::BorderCoordinator>,
        host: Option<std::sync::Arc<crate::border_coordinator::BorderLifecycleCoordinator>>,
    }

    impl ReconciliationFixture {
        fn new() -> anyhow::Result<Self> {
            use crate::border_registry::{
                BorderAdmission, BorderControl, TrackingWindowFingerprint,
            };
            use crate::border_updates::BorderUpdateMailbox;
            use std::sync::Arc;
            use windows::Win32::UI::WindowsAndMessaging::{
                CreateWindowExW, GetSystemMetrics, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
                WINDOW_EX_STYLE, WS_POPUP, WS_VISIBLE,
            };
            let window = OwnedHWND(unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    windows::core::w!("STATIC"),
                    windows::core::w!("fix-01-isolated-reconciliation"),
                    WS_POPUP | WS_VISIBLE,
                    GetSystemMetrics(SM_XVIRTUALSCREEN) - 64,
                    GetSystemMetrics(SM_YVIRTUALSCREEN) - 64,
                    32,
                    32,
                    None,
                    None,
                    None,
                    None,
                )?
            });
            let now = time::Instant::now();
            let control = Arc::new(BorderControl::new()?);
            let mailbox = Arc::new(BorderUpdateMailbox::new(control.clone())?);
            let mut registry = crate::border_registry::BorderRegistry::default();
            registry.admission = BorderAdmission::Open;
            let identity = registry
                .observe(TrackingWindowFingerprint::capture(window.0).unwrap())
                .unwrap();
            registry.reserve(identity, mailbox, now).unwrap();
            let instance = registry.current_by_hwnd[&(window.0.0 as isize)];
            let previous_registry =
                std::mem::replace(&mut *APP_STATE.border_registry.lock().unwrap(), registry);
            let previous_config = std::mem::take(&mut *APP_STATE.get_config_mut());
            APP_STATE.get_config_mut().global.border_z_order =
                crate::config::ZOrderMode::BelowWindow;
            Ok(Self {
                previous_registry,
                previous_config,
                window,
                identity,
                instance,
                control,
                reconciler: WindowReconciler {
                    pending: Default::default(),
                    next_poll: now,
                    next_discovery: now + time::Duration::from_secs(60),
                    retry_at: now,
                },
                now,
                coordinator: None,
                host: None,
            })
        }

        fn pump_native() {
            use windows::Win32::UI::WindowsAndMessaging::{
                DispatchMessageW, MSG, PM_REMOVE, PeekMessageW,
            };
            let mut message = MSG::default();
            for _ in 0..64 {
                if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                    break;
                }
                unsafe {
                    DispatchMessageW(&message);
                }
            }
        }

        fn pump_until(
            &mut self,
            mut ready: impl FnMut(&crate::border_coordinator::BorderCoordinator) -> bool,
        ) {
            let deadline = time::Instant::now() + time::Duration::from_secs(10);
            loop {
                Self::pump_native();
                let coordinator = self.coordinator.as_mut().unwrap();
                coordinator.service_turn();
                if ready(coordinator) {
                    return;
                }
                assert!(
                    time::Instant::now() < deadline,
                    "isolated reconciliation worker did not settle"
                );
                std::thread::yield_now();
            }
        }

        fn start_worker(&mut self) -> anyhow::Result<isize> {
            crate::register_border_window_class()?;
            if APP_STATE.get_directx_devices_mut().is_none() {
                let devices = crate::DirectXDevices::new(APP_STATE.get_render_factory())?;
                *APP_STATE.get_directx_devices_mut() = Some(devices);
            }
            APP_STATE.get_config_mut().global.follow_native_border = false;
            self.coordinator = Some(crate::border_coordinator::BorderCoordinator::new(
                APP_STATE.border_registry.clone(),
            ));
            let instance = self.instance;
            self.pump_until(|coordinator| {
                coordinator
                    .registry
                    .lock()
                    .unwrap()
                    .entries
                    .get(&instance)
                    .is_some_and(|entry| {
                        entry.phase == crate::border_registry::BorderPhase::Running
                    })
            });
            Ok(APP_STATE.border_registry.lock().unwrap().entries[&instance]
                .border_hwnd
                .unwrap())
        }
    }

    impl Drop for ReconciliationFixture {
        fn drop(&mut self) {
            if let Some(host) = self.host.take() {
                host.request_shutdown();
                loop {
                    Self::pump_native();
                    if host.is_terminal() {
                        let _ = host.join_terminal();
                        if host.has_joined() {
                            break;
                        }
                    }
                    std::thread::yield_now();
                }
                APP_STATE.lifecycle.lock().unwrap().take();
            }
            if let Some(coordinator) = self.coordinator.as_mut() {
                let _ = coordinator.begin_operation(
                    crate::border_coordinator::OperationKind::Shutdown,
                    time::Instant::now() + time::Duration::from_secs(10),
                );
                while !coordinator.is_settled() {
                    Self::pump_native();
                    coordinator.service_turn();
                    std::thread::yield_now();
                }
            }
            *APP_STATE
                .border_registry
                .lock()
                .unwrap_or_else(|error| error.into_inner()) =
                std::mem::take(&mut self.previous_registry);
            *APP_STATE.get_config_mut() = std::mem::take(&mut self.previous_config);
        }
    }

    #[test]
    #[serial_test::serial]
    fn reconciliation_discovers_foreign_window_without_creation_hook() -> anyhow::Result<()> {
        use crate::border_registry::{BorderAdmission, BorderPhase};
        let child = DiscoveryChild::new()?;
        let mut fixture = ReconciliationFixture::new()?;
        crate::register_border_window_class()?;
        APP_STATE.get_config_mut().global.follow_native_border = false;
        {
            let mut registry = APP_STATE.border_registry.lock().unwrap();
            *registry = crate::border_registry::BorderRegistry::default();
            registry.admission = BorderAdmission::Open;
        }
        assert!(APP_STATE.lifecycle.lock().unwrap().is_none());
        fixture.host = Some(APP_STATE.start_lifecycle()?);
        fixture.reconciler.next_discovery = fixture.now;
        let tracking = HWND(child.hwnd as _);
        assert_ne!(
            crate::border_registry::TrackingWindowFingerprint::capture(tracking)
                .unwrap()
                .process_id,
            unsafe { windows::Win32::System::Threading::GetCurrentProcessId() }
        );
        fixture
            .reconciler
            .tick_with_discovery(fixture.now, || Ok(vec![fixture.window.0, tracking]));
        let instance = {
            let registry = APP_STATE.border_registry.lock().unwrap();
            assert_eq!(registry.observations.len(), 1);
            assert_eq!(registry.entries.len(), 1);
            registry.current_by_hwnd[&child.hwnd]
        };
        let deadline = time::Instant::now() + time::Duration::from_secs(10);
        let border = loop {
            ReconciliationFixture::pump_native();
            let registry = APP_STATE.border_registry.lock().unwrap();
            let entry = &registry.entries[&instance];
            if entry.phase == BorderPhase::Running {
                break HWND(entry.border_hwnd.unwrap() as _);
            }
            assert!(
                time::Instant::now() < deadline,
                "discovered worker did not run"
            );
            drop(registry);
            std::thread::yield_now();
        };
        assert!(is_window(Some(border)));
        fixture
            .reconciler
            .tick_with_discovery(fixture.now + time::Duration::from_secs(1), || {
                Ok(vec![tracking])
            });
        {
            let registry = APP_STATE.border_registry.lock().unwrap();
            assert_eq!(registry.entries.len(), 1);
            assert_eq!(registry.current_by_hwnd[&child.hwnd], instance);
        }
        let host = fixture.host.as_ref().unwrap().clone();
        drop(fixture);
        assert!(host.is_terminal());
        assert!(host.has_joined());
        host.join_terminal()?;
        assert!(!is_window(Some(border)));
        child.close()?;
        assert!(!is_window(Some(tracking)));
        Ok(())
    }

    struct DiscoveryChild {
        process: std::process::Child,
        output: Option<std::thread::JoinHandle<()>>,
        hwnd: isize,
    }

    impl DiscoveryChild {
        fn new() -> anyhow::Result<Self> {
            use std::io::BufRead;
            let mut process = std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "utils::tests::foreign_discovery_window_helper",
                    "--ignored",
                    "--nocapture",
                ])
                .env("FIX01_DISCOVERY_CHILD", "1")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()?;
            let stdout = process.stdout.take().unwrap();
            let (ready, receiver) = std::sync::mpsc::channel();
            let output = std::thread::spawn(move || {
                for line in std::io::BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
                        && let Some(hwnd) = value
                            .get("fix01_tracking_hwnd")
                            .and_then(|value| value.as_i64())
                    {
                        let _ = ready.send(hwnd as isize);
                    }
                }
            });
            let mut child = Self {
                process,
                output: Some(output),
                hwnd: 0,
            };
            child.hwnd = receiver.recv_timeout(time::Duration::from_secs(10))?;
            Ok(child)
        }

        fn close(mut self) -> anyhow::Result<()> {
            use std::io::Write;
            self.process.stdin.take().unwrap().write_all(b"close\n")?;
            let deadline = time::Instant::now() + time::Duration::from_secs(10);
            loop {
                if let Some(status) = self.process.try_wait()? {
                    anyhow::ensure!(status.success(), "discovery helper failed: {status}");
                    break;
                }
                anyhow::ensure!(
                    time::Instant::now() < deadline,
                    "discovery helper did not exit"
                );
                std::thread::yield_now();
            }
            self.output.take().unwrap().join().unwrap();
            Ok(())
        }
    }

    impl Drop for DiscoveryChild {
        fn drop(&mut self) {
            if self.process.try_wait().ok().flatten().is_none() {
                let _ = self.process.kill();
                let _ = self.process.wait();
            }
            if let Some(output) = self.output.take() {
                let _ = output.join();
            }
        }
    }

    #[test]
    #[ignore = "invoked only by the isolated discovery owner"]
    fn foreign_discovery_window_helper() -> anyhow::Result<()> {
        use std::io::Write;
        use windows::Win32::UI::WindowsAndMessaging::{
            MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, QS_ALLINPUT,
        };
        anyhow::ensure!(std::env::var("FIX01_DISCOVERY_CHILD")? == "1");
        let fixture = ReconciliationFixture::new()?;
        println!(
            "{}",
            serde_json::json!({"fix01_tracking_hwnd": fixture.window.0.0 as isize})
        );
        std::io::stdout().flush()?;
        let input = std::thread::spawn(|| -> anyhow::Result<()> {
            let mut command = String::new();
            std::io::stdin().read_line(&mut command)?;
            anyhow::ensure!(command.trim() == "close", "unexpected helper command");
            Ok(())
        });
        while !input.is_finished() {
            ReconciliationFixture::pump_native();
            unsafe {
                MsgWaitForMultipleObjectsEx(None, 10, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            }
        }
        input.join().unwrap()?;
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn reconciliation_retires_destroyed_observation_without_hook_event() -> anyhow::Result<()> {
        let mut fixture = ReconciliationFixture::new()?;
        fixture.window.destroy()?;
        assert!(!fixture.control.is_stopped());
        fixture.reconciler.tick(fixture.now);
        assert!(fixture.control.is_stopped());
        let registry = APP_STATE.border_registry.lock().unwrap();
        assert!(!registry.observations[&fixture.identity.observation].live);
        assert_eq!(
            registry.entries[&fixture.instance].phase,
            crate::border_registry::BorderPhase::StopRequested
        );
        assert!(
            !registry
                .observation_by_hwnd
                .contains_key(&fixture.identity.fingerprint.hwnd)
        );
        assert_eq!(registry.entries.len(), 1);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn reconciliation_recovers_visibility_without_hook_events() -> anyhow::Result<()> {
        use crate::border_registry::DesiredVisibility;
        use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNOACTIVATE, ShowWindow};
        let mut fixture = ReconciliationFixture::new()?;
        fixture.reconciler.tick(fixture.now);
        assert_eq!(fixture.control.visibility(), DesiredVisibility::Visible);
        fixture.control.take_work();
        unsafe {
            let _ = ShowWindow(fixture.window.0, SW_HIDE);
        }
        fixture
            .reconciler
            .tick(fixture.now + time::Duration::from_millis(100));
        assert_eq!(fixture.control.visibility(), DesiredVisibility::Hidden);
        assert_ne!(
            fixture.control.take_work() & crate::border_runtime::WORK_VISIBILITY,
            0
        );
        unsafe {
            let _ = ShowWindow(fixture.window.0, SW_SHOWNOACTIVATE);
        }
        fixture
            .reconciler
            .tick(fixture.now + time::Duration::from_millis(200));
        assert_eq!(fixture.control.visibility(), DesiredVisibility::Visible);
        assert_ne!(
            fixture.control.take_work() & crate::border_runtime::WORK_VISIBILITY,
            0
        );
        assert!(!fixture.control.is_stopped());
        let registry = APP_STATE.border_registry.lock().unwrap();
        assert_eq!(
            registry.current_by_hwnd[&fixture.identity.fingerprint.hwnd],
            fixture.instance
        );
        assert_eq!(registry.entries.len(), 1);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn reconciliation_recovers_minimize_restore_without_hook_events() -> anyhow::Result<()> {
        use crate::border_registry::DesiredVisibility;
        use windows::Win32::UI::WindowsAndMessaging::{
            SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, ShowWindow,
        };
        let mut fixture = ReconciliationFixture::new()?;
        fixture.reconciler.tick(fixture.now);
        assert_eq!(fixture.control.visibility(), DesiredVisibility::Visible);
        unsafe {
            let _ = ShowWindow(fixture.window.0, SW_SHOWMINNOACTIVE);
        }
        assert!(is_window_visible(fixture.window.0));
        assert!(is_window_minimized(fixture.window.0));
        fixture
            .reconciler
            .tick(fixture.now + time::Duration::from_millis(100));
        assert_eq!(fixture.control.visibility(), DesiredVisibility::Hidden);
        unsafe {
            let _ = ShowWindow(fixture.window.0, SW_SHOWNOACTIVATE);
        }
        assert!(!is_window_minimized(fixture.window.0));
        fixture
            .reconciler
            .tick(fixture.now + time::Duration::from_millis(200));
        assert_eq!(fixture.control.visibility(), DesiredVisibility::Visible);
        assert!(!fixture.control.is_stopped());
        let registry = APP_STATE.border_registry.lock().unwrap();
        assert_eq!(
            registry.current_by_hwnd[&fixture.identity.fingerprint.hwnd],
            fixture.instance
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn reconciliation_retains_candidate_while_config_is_locked() -> anyhow::Result<()> {
        use crate::border_registry::DesiredVisibility;
        let mut fixture = ReconciliationFixture::new()?;
        let config = APP_STATE.get_config_mut();
        assert!(!fixture.reconciler.tick(fixture.now));
        assert_eq!(fixture.reconciler.pending.len(), 1);
        assert_eq!(
            fixture.reconciler.pending.front().unwrap().1,
            Some(fixture.identity)
        );
        assert_eq!(fixture.control.visibility(), DesiredVisibility::Unknown);
        drop(config);
        assert!(
            !fixture
                .reconciler
                .tick(fixture.now + time::Duration::from_millis(9))
        );
        assert_eq!(fixture.reconciler.pending.len(), 1);
        fixture
            .reconciler
            .tick(fixture.now + time::Duration::from_millis(10));
        assert!(fixture.reconciler.pending.is_empty());
        assert_eq!(fixture.control.visibility(), DesiredVisibility::Visible);
        assert!(!fixture.control.is_stopped());
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn reconciliation_retries_registry_contention_after_bounded_delay() -> anyhow::Result<()> {
        use crate::border_registry::DesiredVisibility;
        let mut fixture = ReconciliationFixture::new()?;
        let registry = APP_STATE.border_registry.lock().unwrap();
        assert!(!fixture.reconciler.tick(fixture.now));
        assert_eq!(
            fixture.reconciler.retry_at,
            fixture.now + time::Duration::from_millis(10)
        );
        assert_eq!(fixture.control.visibility(), DesiredVisibility::Unknown);
        drop(registry);
        assert!(
            !fixture
                .reconciler
                .tick(fixture.now + time::Duration::from_millis(9))
        );
        assert_eq!(fixture.control.visibility(), DesiredVisibility::Unknown);
        fixture
            .reconciler
            .tick(fixture.now + time::Duration::from_millis(10));
        assert_eq!(fixture.control.visibility(), DesiredVisibility::Visible);
        assert!(!fixture.control.is_stopped());
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn reconciliation_joins_real_worker_after_missed_tracking_destruction() -> anyhow::Result<()> {
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;
        let mut fixture = ReconciliationFixture::new()?;
        let border = fixture.start_worker()?;
        assert!(!fixture.control.is_stopped());
        assert!(unsafe { IsWindow(Some(HWND(border as _))) }.as_bool());
        fixture.window.destroy()?;
        fixture.reconciler.tick(fixture.now);
        assert!(fixture.control.is_stopped());
        fixture.pump_until(|coordinator| coordinator.is_settled());
        assert_eq!(
            fixture.coordinator.as_mut().unwrap().take_outcome(),
            Some((
                fixture.instance,
                crate::border_registry::BorderWorkerExit::completed()
            ))
        );
        assert!(!unsafe { IsWindow(Some(HWND(border as _))) }.as_bool());
        let registry = APP_STATE.border_registry.lock().unwrap();
        assert!(registry.entries.is_empty());
        assert!(
            registry
                .observations
                .get(&fixture.identity.observation)
                .is_none_or(|observation| !observation.live)
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn invalidated_border_is_joined_and_replaced_after_backoff() -> anyhow::Result<()> {
        eligible_worker_failure_is_joined_and_replaced(None)
    }

    #[test]
    #[serial_test::serial]
    fn controlled_runtime_failures_are_replaced_after_backoff() -> anyhow::Result<()> {
        use crate::window_border::RuntimeStep;
        for step in [
            RuntimeStep::Render,
            RuntimeStep::Position,
            RuntimeStep::Device,
            RuntimeStep::DrawerRecreation,
        ] {
            eligible_worker_failure_is_joined_and_replaced(Some(step))?;
        }
        Ok(())
    }

    fn eligible_worker_failure_is_joined_and_replaced(
        fault: Option<crate::window_border::RuntimeStep>,
    ) -> anyhow::Result<()> {
        use crate::border_registry::{BorderControl, BorderPhase, RegistryError};
        use crate::border_updates::BorderUpdateMailbox;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
        let mut fixture = ReconciliationFixture::new()?;
        crate::register_border_window_class()?;
        if APP_STATE.get_directx_devices_mut().is_none() {
            *APP_STATE.get_directx_devices_mut() =
                Some(crate::DirectXDevices::new(APP_STATE.get_render_factory())?);
        }
        {
            let mut config = APP_STATE.get_config_mut();
            config.global.follow_native_border = false;
        }
        let destroyed = Arc::new(AtomicBool::new(false));
        let failed_control = fixture.control.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let observed_stages = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stage_log = observed_stages.clone();
        let host = crate::border_coordinator::BorderLifecycleCoordinator::start_with_worker_probe(
            APP_STATE.border_registry.clone(),
            Arc::new(move |stage| {
                stage_log.lock().unwrap().push(stage.clone());
                if let crate::border_runtime::WorkerStage::Running(hwnd) = stage
                    && !destroyed.swap(true, Ordering::AcqRel)
                {
                    if let Some(step) = fault {
                        crate::window_border::WindowBorder::fail_runtime_for_test(step);
                        failed_control.request_work(
                            crate::border_runtime::WORK_DISPLAY
                                | crate::border_runtime::WORK_GEOMETRY
                                | crate::border_runtime::WORK_RENDER,
                        );
                    } else {
                        unsafe { DestroyWindow(HWND(hwnd as _)) }.unwrap();
                    }
                    sender.send(hwnd).unwrap();
                }
            }),
        )?;
        host.fail_completion_signal_for_test();
        *APP_STATE.lifecycle.lock().unwrap() = Some(host.clone());
        fixture.host = Some(host.clone());
        let deadline = time::Instant::now() + time::Duration::from_secs(10);
        while !APP_STATE.border_registry.lock().unwrap().entries.is_empty() {
            ReconciliationFixture::pump_native();
            assert!(
                time::Instant::now() < deadline,
                "invalidated worker was not joined"
            );
            std::thread::yield_now();
        }
        let old_hwnd = receiver
            .recv_timeout(time::Duration::from_secs(2))
            .map_err(|error| {
                anyhow!(
                    "{error}; observed worker stages: {:?}",
                    *observed_stages.lock().unwrap()
                )
            })?;
        assert!(!is_window(Some(HWND(old_hwnd as _))));
        let old_id = fixture.instance;
        assert!(fixture.control.is_stopped());
        let updates = Arc::new(BorderUpdateMailbox::new(Arc::new(BorderControl::new()?))?);
        let retry_at = {
            let mut registry = APP_STATE.border_registry.lock().unwrap();
            let observation = &registry.observations[&fixture.identity.observation];
            assert!(observation.live);
            let retry_at = observation.retry_at.unwrap();
            assert_eq!(
                registry.reserve(
                    fixture.identity,
                    updates.clone(),
                    retry_at - time::Duration::from_nanos(1)
                ),
                Err(RegistryError::RetryNotDue)
            );
            retry_at
        };
        let deadline = time::Instant::now() + time::Duration::from_secs(10);
        let replacement = loop {
            let now = time::Instant::now();
            ReconciliationFixture::pump_native();
            fixture.reconciler.tick(now);
            let registry = APP_STATE.border_registry.lock().unwrap();
            if now < retry_at {
                assert!(registry.entries.is_empty(), "retry admitted before backoff");
            }
            if let Some(entry) = registry
                .entries
                .values()
                .find(|entry| entry.phase == BorderPhase::Running)
            {
                break entry.id;
            }
            assert!(
                now < deadline,
                "reconciliation did not recreate invalidated border"
            );
            drop(registry);
            std::thread::yield_now();
        };
        assert_ne!(replacement, old_id);
        let registry = APP_STATE.border_registry.lock().unwrap();
        assert_eq!(registry.entries.len(), 1);
        assert_eq!(registry.entries[&replacement].tracking, fixture.identity);
        assert!(is_window(Some(HWND(
            registry.entries[&replacement].border_hwnd.unwrap() as _
        ))));
        let replacement_hwnd = HWND(registry.entries[&replacement].border_hwnd.unwrap() as _);
        drop(registry);
        drop(fixture);
        assert!(host.has_joined());
        assert!(!is_window(Some(replacement_hwnd)));
        assert_eq!(
            unsafe {
                windows::Win32::System::Threading::WaitForSingleObject(host.completion_event(), 0)
            },
            windows::Win32::Foundation::WAIT_TIMEOUT
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn reconciliation_updates_real_worker_cloak_without_hook_events() -> anyhow::Result<()> {
        use crate::border_registry::DesiredVisibility;
        use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
        let mut fixture = ReconciliationFixture::new()?;
        let border = HWND(fixture.start_worker()? as _);
        fixture.reconciler.tick(fixture.now);
        fixture.pump_until(|_| is_window_visible(border));
        for (index, cloaked) in [1u32, 0u32].into_iter().enumerate() {
            unsafe {
                DwmSetWindowAttribute(
                    fixture.window.0,
                    DWMWA_CLOAK,
                    ptr::addr_of!(cloaked) as _,
                    size_of::<u32>() as u32,
                )?;
            }
            let tracking = fixture.window.0;
            fixture.pump_until(|_| is_window_cloaked(tracking) == (cloaked != 0));
            assert!(is_window_visible(tracking));
            fixture
                .reconciler
                .tick(fixture.now + time::Duration::from_millis(100 + index as u64 * 100));
            assert_eq!(
                fixture.control.visibility(),
                if cloaked != 0 {
                    DesiredVisibility::Hidden
                } else {
                    DesiredVisibility::Visible
                }
            );
            fixture.pump_until(|_| is_window_visible(border) == (cloaked == 0));
            assert!(!fixture.control.is_stopped());
            let registry = APP_STATE.border_registry.lock().unwrap();
            assert_eq!(
                registry.current_by_hwnd[&fixture.identity.fingerprint.hwnd],
                fixture.instance
            );
            assert_eq!(
                registry.entries[&fixture.instance].border_hwnd,
                Some(border.0 as isize)
            );
            assert_eq!(
                registry.entries[&fixture.instance].phase,
                crate::border_registry::BorderPhase::Running
            );
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn delayed_old_visibility_cannot_hide_native_replacement() -> anyhow::Result<()> {
        use crate::border_registry::{BorderControl, BorderPhase, DesiredVisibility};
        use crate::border_updates::BorderUpdateMailbox;
        use std::sync::Arc;
        let mut fixture = ReconciliationFixture::new()?;
        let old_border = HWND(fixture.start_worker()? as _);
        fixture.pump_until(|_| is_window_visible(old_border));
        let old_control = fixture.control.clone();
        let old_identity = fixture.identity;
        let actions = APP_STATE
            .border_registry
            .lock()
            .unwrap()
            .retire_observation(
                fixture.identity.fingerprint.hwnd,
                None,
                time::Instant::now(),
            );
        for action in actions {
            action.signal()?;
        }
        let old_instance = fixture.instance;
        fixture.pump_until(|coordinator| {
            !coordinator
                .registry
                .lock()
                .unwrap()
                .entries
                .contains_key(&old_instance)
        });
        assert!(!is_window(Some(old_border)));
        let control = Arc::new(BorderControl::new()?);
        let mailbox = Arc::new(BorderUpdateMailbox::new(control.clone())?);
        let mut registry = APP_STATE.border_registry.lock().unwrap();
        fixture.identity = registry.observe(old_identity.fingerprint).unwrap();
        assert_ne!(fixture.identity.observation, old_identity.observation);
        registry
            .reserve(fixture.identity, mailbox, time::Instant::now())
            .unwrap();
        fixture.instance = registry.current_by_hwnd[&fixture.identity.fingerprint.hwnd];
        drop(registry);
        fixture.control = control;
        let instance = fixture.instance;
        fixture.pump_until(|coordinator| {
            coordinator
                .registry
                .lock()
                .unwrap()
                .entries
                .get(&instance)
                .is_some_and(|entry| entry.phase == BorderPhase::Running)
        });
        let border = HWND(
            APP_STATE.border_registry.lock().unwrap().entries[&instance]
                .border_hwnd
                .unwrap() as _,
        );
        fixture.pump_until(|_| is_window_visible(border));
        for _ in 0..2 {
            old_control.set_visibility(DesiredVisibility::Hidden);
            old_control.request_work(crate::border_runtime::WORK_VISIBILITY);
            fixture.pump_until(|_| is_window_visible(border));
            assert!(!fixture.control.is_stopped());
            assert_eq!(fixture.control.visibility(), DesiredVisibility::Unknown);
            assert_eq!(
                APP_STATE.border_registry.lock().unwrap().current_by_hwnd
                    [&fixture.identity.fingerprint.hwnd],
                instance
            );
        }
        drop(fixture);
        assert!(!is_window(Some(border)));
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn reconciliation_updates_real_worker_visibility_without_hook_events() -> anyhow::Result<()> {
        use windows::Win32::UI::WindowsAndMessaging::{
            SW_HIDE, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, ShowWindow,
        };
        let mut fixture = ReconciliationFixture::new()?;
        let border = HWND(fixture.start_worker()? as _);
        fixture.reconciler.tick(fixture.now);
        fixture.pump_until(|_| is_window_visible(border));
        for (index, hiding) in [SW_HIDE, SW_SHOWMINNOACTIVE].into_iter().enumerate() {
            unsafe {
                let _ = ShowWindow(fixture.window.0, hiding);
            }
            fixture
                .reconciler
                .tick(fixture.now + time::Duration::from_millis(100 + index as u64 * 200));
            fixture.pump_until(|_| !is_window_visible(border));
            assert!(!fixture.control.is_stopped());
            unsafe {
                let _ = ShowWindow(fixture.window.0, SW_SHOWNOACTIVATE);
            }
            fixture
                .reconciler
                .tick(fixture.now + time::Duration::from_millis(200 + index as u64 * 200));
            fixture.pump_until(|_| is_window_visible(border));
            assert!(!fixture.control.is_stopped());
            let registry = APP_STATE.border_registry.lock().unwrap();
            assert_eq!(
                registry.current_by_hwnd[&fixture.identity.fingerprint.hwnd],
                fixture.instance
            );
            assert_eq!(
                registry.entries[&fixture.instance].border_hwnd,
                Some(border.0 as isize)
            );
            assert_eq!(
                registry.entries[&fixture.instance].phase,
                crate::border_registry::BorderPhase::Running
            );
        }
        Ok(())
    }

    #[test]
    fn failed_native_destruction_retains_hwnd_for_owner_drop_retry() {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, IsWindow, WINDOW_EX_STYLE, WS_POPUP,
        };
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                windows::core::w!("STATIC"),
                windows::core::w!("fix-01-destroy-retry"),
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
        }
        .unwrap();
        let mut owner = OwnedHWND(hwnd);
        assert!(
            owner
                .destroy_with(|_| Err(windows::core::Error::from_hresult(T_E_ERROR)))
                .is_err()
        );
        assert_eq!(owner.0, hwnd);
        assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
        drop(owner);
        assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
        let mut empty = OwnedHWND(HWND::default());
        empty
            .destroy_with(|_| panic!("null owner must not attempt native destruction"))
            .unwrap();
    }

    #[test]
    fn broadcast_work_survives_registry_contention() {
        use crate::border_registry::{
            BorderAdmission, BorderControl, BorderRegistry, TrackingWindowFingerprint,
        };
        use crate::border_updates::BorderUpdateMailbox;
        use std::sync::atomic::{AtomicU8, Ordering};
        use std::sync::{Arc, Mutex};
        let mut registry = BorderRegistry::default();
        registry.admission = BorderAdmission::Open;
        let identity = registry
            .observe(TrackingWindowFingerprint {
                hwnd: 1,
                process_id: 2,
                thread_id: 3,
            })
            .unwrap();
        let control = Arc::new(BorderControl::new().unwrap());
        let mailbox = Arc::new(BorderUpdateMailbox::new(control.clone()).unwrap());
        registry
            .reserve(identity, mailbox, time::Instant::now())
            .unwrap();
        let registry = Mutex::new(registry);
        let bits = crate::border_runtime::WORK_RENDER | crate::border_runtime::WORK_DISPLAY;
        let pending = AtomicU8::new(bits);
        let guard = registry.lock().unwrap();
        flush_work(&registry, &pending);
        assert_eq!(pending.load(Ordering::Acquire), bits);
        assert_eq!(control.take_work(), 0);
        drop(guard);
        flush_work(&registry, &pending);
        assert_eq!(pending.load(Ordering::Acquire), 0);
        assert_eq!(control.take_work(), bits);
        flush_work(&registry, &pending);
        assert_eq!(control.take_work(), 0);
    }

    #[test]
    fn test_cubic_bezier() -> anyhow::Result<()> {
        let easing_fn = cubic_bezier(&[0.45, 0.0, 0.55, 1.0])?;

        let y_coord_0_2 = easing_fn(0.2);
        let y_coord_0_5 = easing_fn(0.5);

        assert!((0.07..=0.08).contains(&y_coord_0_2));
        assert!((0.499..=0.501).contains(&y_coord_0_5));

        Ok(())
    }
}
