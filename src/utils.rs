use anyhow::{Context, anyhow};
use regex::Regex;
use std::borrow::Cow;
use std::cell::Cell;
use std::error::Error;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::{fs, ptr, thread, time};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ENVVAR_NOT_FOUND, ERROR_INVALID_WINDOW_HANDLE, ERROR_SUCCESS, GetLastError,
    HANDLE, HWND, LPARAM, LRESULT, RECT, SetLastError, WIN32_ERROR, WPARAM,
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
    DestroyWindow, DispatchMessageW, GWL_EXSTYLE, GWL_STYLE, GetForegroundWindow, GetMessageW,
    GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowArranged,
    IsWindowVisible, MSG, PostMessageW, RealGetWindowClassW, SendMessageW, SendNotifyMessageW,
    TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_NCDESTROY, WS_CHILD,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_WINDOWEDGE, WS_MAXIMIZE,
};
use windows::core::{BOOL, HRESULT, PWSTR};

use crate::APP_STATE;
use crate::config::{EnableMode, MatchKind, MatchStrategy, WindowRule};
use crate::event_hook::handle_foreground_event;
use crate::window_border::WindowBorder;

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
pub const WM_APP_SET_COLORS: u32 = WM_APP + 10;
pub const WM_APP_SET_WIDTH: u32 = WM_APP + 11;
pub const WM_APP_SET_OFFSET: u32 = WM_APP + 12;
pub const WM_APP_SET_RADIUS: u32 = WM_APP + 13;
pub const WM_APP_ACCENT_CHANGED: u32 = WM_APP + 14;

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

impl Drop for OwnedHWND {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.0) }
            .with_context(|| format!("could not destroy window for {:?}", self.0))
            .log_if_err();
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

// Get the window rule from 'window_rules' in the config
pub fn get_window_rule(hwnd: HWND) -> WindowRule {
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

    let config = APP_STATE.config.read().unwrap();

    for rule in config.window_rules.iter() {
        let window_name: &String = match rule.kind {
            Some(MatchKind::Title) => title_opt.get_or_insert_with(title),
            Some(MatchKind::Class) => class_opt.get_or_insert_with(class),
            Some(MatchKind::Process) => process_opt.get_or_insert_with(process),
            None => {
                error!("expected 'match' for window rule but None found!");
                continue;
            }
        };

        let Some(match_name) = &rule.name else {
            error!("expected `name` for window rule but None found!");
            continue;
        };

        // Check if the window rule matches the window
        let has_match = match rule.strategy {
            Some(MatchStrategy::Equals) | None => {
                window_name.to_lowercase().eq(&match_name.to_lowercase())
            }
            Some(MatchStrategy::Contains) => window_name
                .to_lowercase()
                .contains(&match_name.to_lowercase()),
            Some(MatchStrategy::Regex) => Regex::new(match_name)
                .unwrap()
                .captures(window_name)
                .is_some(),
        };

        // Return the first match
        if has_match {
            return rule.clone();
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

pub fn send_message_w(
    hwnd: HWND,
    msg: u32,
    wparam: Option<WPARAM>,
    lparam: Option<LPARAM>,
) -> LRESULT {
    unsafe { SendMessageW(hwnd, msg, wparam, lparam) }
}

pub fn send_notify_message_w(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> windows::core::Result<()> {
    unsafe { SendNotifyMessageW(hwnd, msg, wparam, lparam) }
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
    let tracking_window_isize = tracking_window.0 as isize;

    let _ = thread::spawn(move || {
        let tracking_window = HWND(tracking_window_isize as _);

        // Note: 'key' for the hashmap is the tracking window, 'value' is the border window
        let mut borders_hashmap = APP_STATE.borders.lock().unwrap();

        // Check to see if there is already a border for the given tracking window
        if borders_hashmap.contains_key(&tracking_window_isize) {
            return;
        }

        debug!("creating border for: {tracking_window:?}");

        // Otherwise, continue creating the border window
        let mut border = match WindowBorder::new(tracking_window) {
            Ok(border) => border,
            Err(err) => {
                error!("could not create window border for {tracking_window:?}: {err:#}");
                return;
            }
        };

        borders_hashmap.insert(tracking_window_isize, border.border_window.0.0 as isize);
        drop(borders_hashmap);

        // Drop these values (to save some RAM?) before calling init and entering a message loop
        let _ = tracking_window;
        let _ = tracking_window_isize;

        if let Err(err) = border.init(window_rule) {
            error!("could not initialize border: {err:#}");
        } else {
            // Window message loop
            unsafe {
                let mut message = MSG::default();
                while GetMessageW(&mut message, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        };

        // If the above loop exits, that means the border has been destroyed, so we should remove
        // it from the hashmap
        APP_STATE
            .borders
            .lock()
            .unwrap()
            .remove(&tracking_window_isize);

        debug!("exiting border thread for {:?}!", border.tracking_window);
    });
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

pub fn hiword(val: usize) -> u16 {
    ((val >> 16) & 0xFFFF) as u16
}

pub fn loword(val: usize) -> u16 {
    (val & 0xFFFF) as u16
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
    if let Some(&border_isize) = APP_STATE
        .borders
        .lock()
        .unwrap()
        .get(&(tracking_window.0 as isize))
    {
        let border_window = HWND(border_isize as _);

        send_notify_message_w(border_window, WM_NCDESTROY, WPARAM(0), LPARAM(0))
            .context("destroy_border_for_window")
            .log_if_err();
    }
}

pub fn get_border_for_window(hwnd: HWND) -> Option<HWND> {
    let borders_hashmap = APP_STATE.borders.lock().unwrap();

    let hwnd_isize = hwnd.0 as isize;
    let Some(border_isize) = borders_hashmap.get(&hwnd_isize) else {
        drop(borders_hashmap);
        return None;
    };

    let border_window: HWND = HWND(*border_isize as _);

    Some(border_window)
}

pub fn show_border_for_window(hwnd: HWND) {
    // If the border already exists, simply post a 'SHOW' message to its message queue. Otherwise,
    // create a new border.
    if let Some(border) = get_border_for_window(hwnd) {
        post_message_w(Some(border), WM_APP_SHOWUNCLOAKED, WPARAM(0), LPARAM(0))
            .context("show_border_for_window")
            .log_if_err();
    } else if is_window_top_level(hwnd) && is_window_visible(hwnd) && !is_window_cloaked(hwnd) {
        let window_rule = get_window_rule(hwnd);

        if window_rule.enabled == Some(EnableMode::Bool(false)) {
            info!("border is disabled for {hwnd:?}");
        } else if window_rule.enabled == Some(EnableMode::Bool(true)) || !has_filtered_style(hwnd) {
            create_border_for_window(hwnd, window_rule);
        }
    }
}

pub fn hide_border_for_window(hwnd: HWND) {
    let hwnd_isize = hwnd.0 as isize;

    // Spawn a new thread to guard against re-entrancy in the event hook, though it honestly isn't
    // that important for our purposes I think
    let _ = thread::spawn(move || {
        let hwnd = HWND(hwnd_isize as _);

        if let Some(border) = get_border_for_window(hwnd) {
            post_message_w(Some(border), WM_APP_HIDECLOAKED, WPARAM(0), LPARAM(0))
                .context("hide_border_for_window")
                .log_if_err();
        }
    });
}

/// Spawns a thread that polls to make up for unreliable events (e.g. EVENT_SYSTEM_FOREGROUND).
pub fn spawn_window_state_poller() {
    const POLL_DELAY: u64 = 100;

    let _ = thread::spawn(move || {
        loop {
            // Handle any changes in terms of which window is foreground/active
            let old_active_hwnd = HWND(*APP_STATE.active_window.lock().unwrap() as _);
            let new_active_hwnd = get_foreground_window();
            if new_active_hwnd != old_active_hwnd && !new_active_hwnd.is_invalid() {
                handle_foreground_event(new_active_hwnd, old_active_hwnd);
            }

            // Reap borders for windows that no longer exist
            let invalid_hwnds: Vec<HWND> = APP_STATE
                .borders
                .lock()
                .unwrap()
                .keys()
                .map(|tracking_isize| HWND(*tracking_isize as _))
                .filter(|tracking_hwnd| !is_window(Some(*tracking_hwnd)))
                .collect();
            for hwnd in invalid_hwnds {
                destroy_border_for_window(hwnd);
            }

            thread::sleep(time::Duration::from_millis(POLL_DELAY));
        }
    });
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
