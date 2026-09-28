use anyhow::{Context, anyhow};
use std::ptr;
use std::thread;
use std::time;
use windows::Win32::Foundation::{
    COLORREF, D2DERR_RECREATE_TARGET, FALSE, HWND, LPARAM, LRESULT, RECT, TRUE, WPARAM,
};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D_SIZE_U};
use windows::Win32::Graphics::Direct2D::{D2D1_BRUSH_PROPERTIES, ID2D1RenderTarget};
use windows::Win32::Graphics::Dwm::{
    DWM_BB_BLURREGION, DWM_BB_ENABLE, DWM_BLURBEHIND, DWMWA_EXTENDED_FRAME_BOUNDS,
    DwmEnableBlurBehindWindow, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_ERROR_DEVICE_REMOVED,
    DXGI_GPU_PREFERENCE_UNSPECIFIED, IDXGIAdapter, IDXGIFactory6,
};
use windows::Win32::Graphics::Gdi::{CreateRectRgn, HMONITOR, ValidateRect};
use windows::Win32::UI::HiDpi::MDT_DEFAULT;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DBT_DEVNODES_CHANGED, DefWindowProcW,
    GW_HWNDNEXT, GW_HWNDPREV, GWLP_USERDATA, GetSystemMetrics, GetWindow, GetWindowLongPtrW,
    HWND_TOP, KillTimer, LWA_ALPHA, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND,
    PBT_APMSUSPEND, PM_REMOVE, PeekMessageW, PostQuitMessage, SET_WINDOW_POS_FLAGS,
    SM_CXVIRTUALSCREEN, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOREDRAW, SWP_NOSENDCHANGING,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW,
    SetWindowPos, WM_CREATE, WM_DEVICECHANGE, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_NCDESTROY,
    WM_PAINT, WM_POWERBROADCAST, WM_TIMER, WM_WINDOWPOSCHANGED, WM_WINDOWPOSCHANGING, WS_DISABLED,
    WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::animations::{AnimType, AnimVec};
use crate::border_config::BorderConfig;
use crate::border_drawer::BorderDrawer;
use crate::colors::ColorBrushConfig;
use crate::config::{Offset, WindowRule, ZOrderMode};
use crate::ipc::{
    IpcPayload, IpcSetColorsPayload, IpcSetOffsetPayload, IpcSetRadiusPayload, IpcSetWidthPayload,
};
use crate::komorebi::WindowKind;
use crate::render_backend::{RenderBackend, RenderBackendConfig};
use crate::utils::{
    LogIfErr, OwnedHWND, ReentrancyBlocker, ReentrancyBlockerExt, StandaloneWindowsError,
    T_E_ERROR, T_E_REENTRANCY, T_E_UNINIT, ToWindowsResult, WM_APP_ACCENT_CHANGED, WM_APP_ANIMATE,
    WM_APP_FOREGROUND, WM_APP_HIDECLOAKED, WM_APP_KOMOREBI, WM_APP_LOCATIONCHANGE,
    WM_APP_MINIMIZEEND, WM_APP_MINIMIZESTART, WM_APP_RECREATE_DRAWER, WM_APP_REORDER,
    WM_APP_SHOWUNCLOAKED, WindowsCompatibleError, WindowsCompatibleResult, WindowsContext,
    are_rects_same_size, get_dpi_for_monitor, get_monitor_info, get_window_rule, get_window_title,
    has_native_border, is_window, is_window_arranged, is_window_cloaked, is_window_minimized,
    is_window_visible, loword, monitor_from_window, post_message_w,
};
use crate::{APP_STATE, BG_SERVICES};

const REORDER_TIMER_ID: usize = 0;

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub enum WindowState {
    #[default]
    Active,
    Inactive,
}

impl WindowState {
    pub fn update(&mut self, self_hwnd: isize, active_hwnd: isize) {
        if self_hwnd == active_hwnd {
            *self = WindowState::Active;
        } else {
            *self = WindowState::Inactive;
        }
    }
}

#[derive(Debug)]
pub struct WindowBorder {
    pub border_window: OwnedHWND,
    pub tracking_window: HWND,
    window_state: WindowState,
    window_rect: RECT,
    border_offset: Offset,
    border_padding: i32, // padding to accommodate things like shadow/glow effects
    current_monitor: HMONITOR,
    current_dpi: u32,
    drawer: BorderDrawer,
    config: BorderConfig, // cached config values
    is_paused: bool,
    last_reorder_time: Option<time::Instant>,
    is_debouncing_reorder: bool,
    consecutive_reorders: u64,
    arranged_override_active: bool, // radius override for arranged/snapped tracking window
}

impl WindowBorder {
    pub fn new(tracking_window: HWND) -> WindowsCompatibleResult<Box<Self>> {
        // We wrap this in a Box because the window procedure uses a pointer to the struct, and
        // using a Box ensures its memory address remains fixed during its lifetime.
        let mut this = Box::new(Self {
            border_window: OwnedHWND(HWND::default()), // temporary null hwnd; should be harmless
            tracking_window,
            window_state: Default::default(),
            window_rect: Default::default(),
            border_offset: Default::default(),
            border_padding: Default::default(),
            current_monitor: Default::default(),
            current_dpi: Default::default(),
            drawer: Default::default(),
            config: Default::default(),
            is_paused: Default::default(),
            last_reorder_time: None,
            is_debouncing_reorder: false,
            consecutive_reorders: 0,
            arranged_override_active: false,
        });

        this.create_window()
            .windows_context("could not create border window")?;

        Ok(this)
    }

    fn create_window(&mut self) -> WindowsCompatibleResult<()> {
        let title: Vec<u16> = format!(
            "tacky-border | {} | {:?}\0",
            get_window_title(self.tracking_window).unwrap_or_default(),
            self.tracking_window
        )
        .encode_utf16()
        .collect();

        unsafe {
            self.border_window.0 = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT,
                w!("border"),
                PCWSTR(title.as_ptr()),
                WS_POPUP | WS_DISABLED,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                None,
                None,
                Some(ptr::addr_of!(*self) as _),
            )?;
        }

        Ok(())
    }

    pub fn init(&mut self, window_rule: WindowRule) -> anyhow::Result<()> {
        self.current_monitor = monitor_from_window(self.tracking_window);
        self.current_dpi =
            get_dpi_for_monitor(self.current_monitor, MDT_DEFAULT).map_err(|err| {
                self.cleanup_and_queue_exit();
                anyhow!("could not get dpi for {:?}: {}", self.current_monitor, err)
            })?;
        self.load_from_config(window_rule, self.current_dpi)?;

        // Delay the border while the tracking window is in its creation animation
        thread::sleep(time::Duration::from_millis(self.config.initialize_delay));

        unsafe {
            // Make the window transparent (stole the code from PowerToys; dunno how it works).
            let pos: i32 = -GetSystemMetrics(SM_CXVIRTUALSCREEN) - 8;
            let hrgn = CreateRectRgn(pos, 0, pos + 1, 1);
            let mut bh: DWM_BLURBEHIND = Default::default();
            if !hrgn.is_invalid() {
                bh = DWM_BLURBEHIND {
                    dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
                    fEnable: TRUE,
                    hRgnBlur: hrgn,
                    fTransitionOnMaximized: FALSE,
                };
            }
            DwmEnableBlurBehindWindow(self.border_window.0, &bh)
                .context("could not make window transparent")?;
            SetLayeredWindowAttributes(self.border_window.0, COLORREF(0x00000000), 255, LWA_ALPHA)
                .context("could not set LWA_ALPHA")?;

            self.init_drawer()
                .context("could not initialize border drawer in init()")?;
            self.init_border()
                .context("could not initialize border in init()")?;
        }

        Ok(())
    }

    fn init_border(&mut self) -> anyhow::Result<()> {
        self.update_color(Some(self.config.initialize_delay));
        self.update_window_rect().log_if_err();
        if self.should_show_border() {
            self.update_position(Some(SWP_SHOWWINDOW)).log_if_err();
            self.render().log_if_err();

            // TODO: sometimes, the border doesn't show up on the first try. So, we just wait
            // 5ms and call render() again. This seems to be an issue with the visibility of
            // the window itself.
            thread::sleep(time::Duration::from_millis(5));
            self.update_position(Some(SWP_SHOWWINDOW)).log_if_err();
            self.render().log_if_err();

            self.drawer.set_anims_timer_if_needed(self.border_window.0);
        }

        // Handle the edge case where the tracking window is already minimized
        if is_window_minimized(self.tracking_window) {
            post_message_w(
                Some(self.border_window.0),
                WM_APP_MINIMIZESTART,
                WPARAM(0),
                LPARAM(0),
            )
            .context("could not post WM_APP_MINIMIZESTART message in init()")?;
        }

        {
            let services = BG_SERVICES.lock().unwrap();
            if let Some(komorebi_integration) = services.komorebi_integration.as_ref() {
                let self_focus_state = komorebi_integration
                    .focus_state
                    .lock()
                    .unwrap()
                    .get(&(self.tracking_window.0 as isize))
                    .copied();

                // Handle the edge case where the focus state is already komorebi-specific upon border creation
                if !matches!(
                    self_focus_state,
                    None | Some(WindowKind::Single) | Some(WindowKind::Unfocused)
                ) {
                    post_message_w(
                        Some(self.border_window.0),
                        WM_APP_KOMOREBI,
                        WPARAM(0),
                        LPARAM(0),
                    )
                    .context("could not post WM_APP_KOMOREBI message in init()")
                    .log_if_err();
                }
            }
        }

        Ok(())
    }

    pub fn load_from_config(&mut self, window_rule: WindowRule, dpi: u32) -> anyhow::Result<()> {
        let app_config = APP_STATE.config.read().unwrap();
        let is_initial_window = APP_STATE
            .initial_windows
            .lock()
            .unwrap()
            .contains(&(self.tracking_window.0 as isize));

        self.config = BorderConfig::resolve(
            &window_rule,
            &app_config.global,
            app_config.render_backend,
            is_initial_window,
        );
        self.drawer
            .configure_appearance(&self.config, dpi, self.tracking_window);

        self.border_offset = self.config.offset_at(dpi);
        self.border_padding = self.config.border_padding(&self.drawer);
        self.current_dpi = dpi;

        // Handle edge case where window is arranged at start
        if is_window_arranged(self.tracking_window) {
            self.sync_border_radius();
        }

        Ok(())
    }

    // The V2 renderer uses DirectComposition which makes it easy to create a large persistent
    // graphics buffer and let the compositor handle the positioning of the graphics content.
    fn calculate_target_v2_renderer_size(&self) -> WindowsCompatibleResult<D2D_SIZE_U> {
        let monitor_info =
            get_monitor_info(self.current_monitor).windows_context("could not get monitor info")?;
        let monitor_width = monitor_info.rcMonitor.right - monitor_info.rcMonitor.left;
        let monitor_height = monitor_info.rcMonitor.bottom - monitor_info.rcMonitor.top;

        let stroke_width = self.drawer.stroke_width;
        let border_offset = self.border_offset;
        let border_padding = self.border_padding;

        Ok(D2D_SIZE_U {
            width: (monitor_width as i32
                + stroke_width * 2
                + border_offset.left
                + border_offset.right
                + border_padding * 2)
                .max(1) as u32,
            height: (monitor_height as i32
                + stroke_width * 2
                + border_offset.top
                + border_offset.bottom
                + border_padding * 2)
                .max(1) as u32, // size must be at least 1 otherwise resize/init renderer fails
        })
    }

    // The legacy renderer uses an ID2D1HwndRenderTarget which scales the graphics buffer and its
    // content to fit inside the window rect. To avoid unwanted scaling, it's easiest to make the
    // buffer the same size as the window.
    fn calculate_target_legacy_renderer_size(&self) -> D2D_SIZE_U {
        D2D_SIZE_U {
            width: (self.window_rect.right - self.window_rect.left) as u32,
            height: (self.window_rect.bottom - self.window_rect.top) as u32,
        }
    }

    fn calculate_target_renderer_size(&self) -> WindowsCompatibleResult<D2D_SIZE_U> {
        match self.config.render_backend {
            RenderBackendConfig::V2 => self.calculate_target_v2_renderer_size(),
            RenderBackendConfig::Legacy => Ok(self.calculate_target_legacy_renderer_size()),
        }
    }

    fn raw_init_drawer(&mut self) -> WindowsCompatibleResult<()> {
        let renderer_size = self
            .calculate_target_renderer_size()
            .windows_context("could not calculate target renderer size")?;

        self.drawer
            .init(
                renderer_size.width,
                renderer_size.height,
                self.border_window.0,
                self.compute_border_bounds(),
                self.config.render_backend,
            )
            .windows_context("could not initialize border drawer")?;

        Ok(())
    }

    fn init_drawer(&mut self) -> WindowsCompatibleResult<()> {
        self.raw_init_drawer()
            .or_else(|err| {
                self.handle_directx_errors(err)?;
                self.raw_init_drawer()
            })
            .inspect_err(|err| {
                if err.code() != T_E_REENTRANCY {
                    self.cleanup_and_queue_exit();
                }
            })
    }

    fn needs_drawer_recreation(&self) -> WindowsCompatibleResult<bool> {
        match self.drawer.render_backend {
            // With the V2 backend, we use the stored adapter LUID to check whether our backend is
            // still using the primary display adapter.
            RenderBackend::V2(ref backend) => {
                let dxgi_factory: IDXGIFactory6 =
                    unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS::default()) }
                        .windows_context(
                            "could not create dxgi_factory to check for GPU adapter changes",
                        )?;

                let new_dxgi_adapter: IDXGIAdapter = unsafe {
                    dxgi_factory.EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_UNSPECIFIED)?
                };
                let new_adapter_desc = unsafe { new_dxgi_adapter.GetDesc() }
                    .windows_context("could not get new_adapter_desc")?;

                Ok(backend.adapter_luid != new_adapter_desc.AdapterLuid)
            }
            // With the Legacy backend, we check whether the underlying display adapter is still
            // valid. This does not guarantee that it is the primary adapter.
            RenderBackend::Legacy(ref backend) => unsafe {
                backend.render_target.BeginDraw();
                Ok(backend.render_target.EndDraw(None, None).is_err())
            },
            RenderBackend::None => Ok(true),
        }
    }

    fn recreate_drawer_if_needed(&mut self) -> WindowsCompatibleResult<()> {
        if self
            .needs_drawer_recreation()
            .windows_context("could not check if border drawer needs to be recreated")?
        {
            self.init_drawer()
                .windows_context("could not recreate border drawer")?;
            self.update_color(None);
            self.render().windows_context("could not render")?;
        }

        Ok(())
    }

    fn should_show_border(&self) -> bool {
        (!self.config.follow_native_border || has_native_border(self.tracking_window))
            && is_window_visible(self.tracking_window)
            && !is_window_cloaked(self.tracking_window)
            && !is_window_minimized(self.tracking_window)
    }

    // NOTE: Avoid calling this function + self.render() while the tracking window is minimized
    // as its coordinates will be meaningless and will lead to an oddly sized border.
    fn update_window_rect(&mut self) -> anyhow::Result<()> {
        if let Err(e) = unsafe {
            DwmGetWindowAttribute(
                self.tracking_window,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                ptr::addr_of_mut!(self.window_rect) as _,
                size_of::<RECT>() as u32,
            )
            .context(format!(
                "could not get window rect for {:?}",
                self.tracking_window
            ))
        } {
            self.cleanup_and_queue_exit();
            return Err(e);
        }

        let stroke_width = self.drawer.stroke_width;
        let border_padding = self.border_padding;
        // Make space for the border + padding
        self.window_rect.top -= stroke_width + self.border_offset.top + border_padding;
        self.window_rect.left -= stroke_width + self.border_offset.left + border_padding;
        self.window_rect.right += stroke_width + self.border_offset.right + border_padding;
        self.window_rect.bottom += stroke_width + self.border_offset.bottom + border_padding;

        Ok(())
    }

    // NOTE: SetWindowPos is asynchronous, meaning it sends a request to the DWM but doesn't wait
    // for it to complete. Keep this in mind when ordering function calls elsewhere in the code.
    fn update_position(&mut self, other_flags: Option<SET_WINDOW_POS_FLAGS>) -> anyhow::Result<()> {
        unsafe {
            let mut swp_flags = SWP_NOSENDCHANGING
                | SWP_NOACTIVATE
                | SWP_NOREDRAW
                | other_flags.unwrap_or_default();

            let hwndinsertafter = match self.config.z_order {
                ZOrderMode::AboveWindow => {
                    // Get the hwnd above the tracking hwnd so we can place the border window in between
                    let hwnd_above_tracking = GetWindow(self.tracking_window, GW_HWNDPREV);

                    // If hwnd_above_tracking is the window border itself, we have what we want and there's
                    // no need to change the z-order (plus it results in an error if we try it).
                    if hwnd_above_tracking == Ok(self.border_window.0) {
                        swp_flags |= SWP_NOZORDER;
                    }

                    hwnd_above_tracking.unwrap_or(HWND_TOP)
                }
                ZOrderMode::BelowWindow => self.tracking_window,
            };

            if let Err(e) = SetWindowPos(
                self.border_window.0,
                Some(hwndinsertafter),
                self.window_rect.left,
                self.window_rect.top,
                self.window_rect.right - self.window_rect.left,
                self.window_rect.bottom - self.window_rect.top,
                swp_flags,
            )
            .context(format!(
                "could not set window position for {:?}",
                self.tracking_window
            )) {
                self.cleanup_and_queue_exit();
                return Err(e);
            }
        }

        Ok(())
    }

    fn update_color(&mut self, check_delay: Option<u64>) {
        self.window_state.update(
            self.tracking_window.0 as isize,
            *APP_STATE.active_window.lock().unwrap(),
        );

        match self
            .drawer
            .animations
            .get_current(self.window_state)
            .contains_type(AnimType::Fade)
        {
            false => self.update_brush_opacities(),
            true if check_delay == Some(0) => {
                self.update_brush_opacities();
                self.drawer
                    .animations
                    .update_fade_progress(self.window_state)
            }
            true => {} // We will rely on the animations callback to update color
        }
    }

    fn update_brush_opacities(&mut self) {
        let (top_color, bottom_color) = match self.window_state {
            WindowState::Active => (
                &mut self.drawer.active_color,
                &mut self.drawer.inactive_color,
            ),
            WindowState::Inactive => (
                &mut self.drawer.inactive_color,
                &mut self.drawer.active_color,
            ),
        };
        top_color.set_opacity(1.0).log_if_err();
        bottom_color.set_opacity(0.0).log_if_err();
    }

    /// Updates the cached color config and reinitializes the corresponding brush.
    fn update_color_brush(&mut self, is_active: bool, config: ColorBrushConfig) {
        let config = match is_active {
            true => {
                self.config.active_color = config;
                self.config.active_color.clone()
            }
            false => {
                self.config.inactive_color = config;
                self.config.inactive_color.clone()
            }
        };

        self.refresh_color_brush(is_active, &config);
    }

    /// Re-resolves a color config into its brush and reinitializes it, preserving the
    /// current opacity and transform (e.g. fade animation progress) of the old brush.
    /// Unlike `update_color_brush`, this leaves the cached config untouched, so colors
    /// that are resolved lazily (like "accent") are re-read without losing any
    /// config value that the border was created with.
    fn refresh_color_brush(&mut self, is_active: bool, config: &ColorBrushConfig) {
        let bounds = self.compute_border_bounds();

        let brush = match is_active {
            true => &mut self.drawer.active_color,
            false => &mut self.drawer.inactive_color,
        };

        let old_opacity = brush.get_opacity().unwrap_or_default();
        let old_transform = brush.get_transform().unwrap_or_default();

        *brush = config.to_color_brush(is_active);

        let renderer: &ID2D1RenderTarget = match self.drawer.render_backend {
            RenderBackend::V2(ref backend) => &backend.d2d_context,
            RenderBackend::Legacy(ref backend) => &backend.render_target,
            RenderBackend::None => {
                error!("render backend is None in refresh_color_brush()");
                return;
            }
        };
        let brush_properties = D2D1_BRUSH_PROPERTIES {
            opacity: old_opacity,
            transform: old_transform,
        };

        brush
            .init_brush(renderer, &bounds, &brush_properties)
            .log_if_err();
    }

    /// Re-resolves this border's own color configs so that any color depending on the
    /// current system state (e.g. "accent") is refreshed. This is sent by the
    /// AccentWatcher when the user changes their Windows accent color.
    /// Returns whether any brush was reinitialized, so callers can skip rendering
    /// borders whose colors cannot depend on the accent color.
    fn refresh_accent_dependent_colors(&mut self) -> bool {
        let mut refreshed = false;

        let active_color = self.config.active_color.clone();
        if active_color.is_accent_aware() {
            self.refresh_color_brush(true, &active_color);
            refreshed = true;
        }

        let inactive_color = self.config.inactive_color.clone();
        if inactive_color.is_accent_aware() {
            self.refresh_color_brush(false, &inactive_color);
            refreshed = true;
        }

        refreshed
    }

    fn handle_directx_errors(
        &mut self,
        err: WindowsCompatibleError,
    ) -> WindowsCompatibleResult<()> {
        thread_local! {
            static REENTRANCY_BLOCKER: ReentrancyBlocker = ReentrancyBlocker::new();
        }

        if err.code() == D2DERR_RECREATE_TARGET || err.code() == DXGI_ERROR_DEVICE_REMOVED {
            let _guard = REENTRANCY_BLOCKER
                .enter()
                .context("handle_directx_errors")
                .to_windows_result(T_E_REENTRANCY)?;

            if let Some(directx_devices) = APP_STATE.directx_devices.write().unwrap().as_mut() {
                directx_devices
                    .recreate_if_needed()
                    .windows_context("could not recreate directx devices if needed")?;
            }
            self.recreate_drawer_if_needed()
                .windows_context("could not recreate border drawer if needed")?;
        } else if err.code() == T_E_UNINIT {
            // Functions like render() may be called via callback functions before init()
            // completes, leading to errors due to uninitialized objects. This is likely only
            // temporary, so I'll just use debug! instead of logging it as a full error.
            debug!("an object is currently unitialized: {err:#}");
        } else {
            return Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(
                    T_E_ERROR,
                    format!("a directx operation failed; exiting thread: {err:#}"),
                ),
            ));
        }

        Ok(())
    }

    /// Computes the rect to pass into BorderDrawer::render()
    fn compute_border_bounds(&self) -> D2D_RECT_F {
        let window_rect = self.window_rect;
        let border_padding = self.border_padding as f32;

        D2D_RECT_F {
            left: border_padding,
            top: border_padding,
            right: (window_rect.right - window_rect.left) as f32 - border_padding,
            bottom: (window_rect.bottom - window_rect.top) as f32 - border_padding,
        }
    }

    fn raw_render(&mut self) -> WindowsCompatibleResult<()> {
        // The legacy renderer's size requires an update everytime self.window_rect updates
        if let RenderBackend::Legacy(ref backend) = self.drawer.render_backend {
            let renderer_size = self.calculate_target_legacy_renderer_size();
            backend.resize(renderer_size.width, renderer_size.height)?;
        }
        let bounds = self.compute_border_bounds();

        self.drawer.render(bounds, self.window_state)
    }

    pub fn render(&mut self) -> WindowsCompatibleResult<()> {
        self.raw_render()
            .or_else(|err| {
                self.handle_directx_errors(err)?;
                self.raw_render()
            })
            .inspect_err(|err| {
                if err.code() != T_E_REENTRANCY {
                    self.cleanup_and_queue_exit();
                }
            })
    }

    fn rescale_border(&mut self, new_dpi: u32) {
        self.drawer.stroke_width = self.config.width_at(new_dpi);
        self.border_offset = self.config.offset_at(new_dpi);
        self.drawer.effects = self.config.effects.to_effects(new_dpi);
        self.border_padding = self.config.border_padding(&self.drawer);
        self.current_dpi = new_dpi;
        self.sync_border_radius();

        if self.drawer.render_backend.supports_effects() {
            self.drawer
                .effects
                .init_command_lists_if_enabled(&self.drawer.render_backend)
                .log_if_err();
        }
    }

    fn needs_renderer_resize(&self) -> anyhow::Result<bool> {
        let correct_renderer_size = self
            .calculate_target_renderer_size()
            .context("could not calculate target renderer size")?;
        let actual_renderer_size = self
            .drawer
            .render_backend
            .get_pixel_size()
            .context("could not get actual renderer size")?;

        Ok(correct_renderer_size != actual_renderer_size)
    }

    fn raw_resize_renderer(&mut self) -> WindowsCompatibleResult<()> {
        let renderer_size = self
            .calculate_target_renderer_size()
            .windows_context("could not calculate target renderer size")?;
        self.drawer
            .resize_renderer(renderer_size.width, renderer_size.height)
            .windows_context("could not update renderer")
    }

    fn resize_renderer(&mut self) -> WindowsCompatibleResult<()> {
        self.raw_resize_renderer()
            .or_else(|err| {
                self.handle_directx_errors(err)?;
                self.raw_resize_renderer()
            })
            .inspect_err(|err| {
                if err.code() != T_E_REENTRANCY {
                    self.cleanup_and_queue_exit();
                }
            })
    }

    fn rescale_border_and_resize_renderer_if_needed(
        &mut self,
        new_monitor: HMONITOR,
    ) -> anyhow::Result<bool> {
        let mut is_updated = false;

        let new_dpi =
            get_dpi_for_monitor(new_monitor, MDT_DEFAULT).context("could not get new_dpi")?;
        if new_dpi != self.current_dpi {
            self.current_dpi = new_dpi;
            debug!("dpi has changed! new dpi: {new_dpi}");
            is_updated = true;

            self.rescale_border(new_dpi);
        }

        if self
            .needs_renderer_resize()
            .context("could not check if renderer needs resizing")?
        {
            self.resize_renderer()?;
            is_updated = true;
        }

        Ok(is_updated)
    }

    /// Syncs the border's radius based on the tracking window's state (snapped/arranged or not),
    /// the cached radius config, and other current border parameters.
    ///
    /// Returns a bool indicating whether the radius was updated.
    //
    // TODO: Make it harder to directly set self.drawer.corner_radius in other parts of
    // the code (we should use this function instead)
    fn sync_border_radius(&mut self) -> bool {
        let mut is_updated = false;

        let is_radius_config_auto = self.config.is_radius_auto();
        let is_tracking_window_arranged = is_window_arranged(self.tracking_window);

        if is_radius_config_auto && is_tracking_window_arranged {
            if *self.drawer.corner_radius.get() != 0.0 {
                is_updated = self
                    .drawer
                    .corner_radius
                    .set(0.0)
                    .inspect_err(|err| error!("could not set corner_radius: {err:#}"))
                    .is_ok();
            }
            self.drawer.corner_radius.lock_writes();
            self.arranged_override_active = true;
        } else {
            self.drawer.corner_radius.unlock_writes();
            let radius = self.config.radius_at(
                self.drawer.stroke_width,
                self.current_dpi,
                self.tracking_window,
            );
            if *self.drawer.corner_radius.get() != radius {
                is_updated = self
                    .drawer
                    .corner_radius
                    .set(radius)
                    .inspect_err(|err| error!("could not set corner_radius: {err:#}")) // err should be unreachable
                    .is_ok();
            }
            self.arranged_override_active = false;
        }

        is_updated
    }

    fn cleanup_and_queue_exit(&mut self) {
        self.is_paused = true;
        self.drawer.destroy_anims_timer();
        unsafe { PostQuitMessage(0) };
    }

    /// # Safety
    ///
    /// This is only here because clippy is throwing warnings at me lol. It's just a window
    /// procedure; don't use it for other things.
    pub unsafe extern "system" fn s_wnd_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // Retrieve the pointer to this WindowBorder struct using GWLP_USERDATA
        let mut border_pointer: *mut WindowBorder =
            unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as _;

        // If a pointer has not yet been assigned to GWLP_USERDATA, assign it here using the LPARAM
        // from CreateWindowExW
        if border_pointer.is_null() && message == WM_CREATE {
            let create_struct: *mut CREATESTRUCTW = lparam.0 as *mut _;
            border_pointer = unsafe { (*create_struct).lpCreateParams } as *mut _;
            unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, border_pointer as _) };
        }

        match !border_pointer.is_null() {
            true => unsafe { (*border_pointer).wnd_proc(window, message, wparam, lparam) },
            false => unsafe { DefWindowProcW(window, message, wparam, lparam) },
        }
    }

    unsafe fn wnd_proc(
        &mut self,
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            // EVENT_OBJECT_LOCATIONCHANGE
            WM_APP_LOCATIONCHANGE => {
                // This is here to prevent LOCATIONCHANGE events from being handled before
                // MINIMIZEEND or SHOW/UNCLOAKED events.
                if self.is_paused {
                    return LRESULT(0);
                }

                // Check if tracking window still exists to avoid ghost borders
                if !is_window(Some(self.tracking_window)) {
                    self.cleanup_and_queue_exit();
                    return LRESULT(0);
                }

                // This is mainly here to handle cases where a window is made borderless fullscreen
                // (if 'follow_native_border' is set to true in the config), which doesn't have any
                // dedicated events like MINIMIZEEND. Note that we don't set 'is_paused' to true as
                // doing so would prevent us from handling the transition back to a regular window.
                if !self.should_show_border() {
                    self.update_position(Some(SWP_HIDEWINDOW)).log_if_err();
                    self.drawer.destroy_anims_timer();

                    return LRESULT(0);
                }

                let mut needs_render = false;

                let new_monitor = monitor_from_window(self.tracking_window);
                if new_monitor != self.current_monitor {
                    self.current_monitor = new_monitor;
                    debug!("monitor has changed! new monitor: {new_monitor:?}");

                    needs_render |=
                        match self.rescale_border_and_resize_renderer_if_needed(new_monitor) {
                            Ok(is_updated) => is_updated,
                            Err(err) => {
                                error!("could not update appearance and renderer: {err:#}");
                                return LRESULT(0);
                            }
                        };
                }

                // If radius config is Auto, sync radius when the snap/arranged state changes
                // so that the border is square when arranged, and restored to normal after.
                if self.config.is_radius_auto()
                    && is_window_arranged(self.tracking_window) != self.arranged_override_active
                {
                    needs_render |= self.sync_border_radius();
                }

                let prev_rect = self.window_rect;
                self.update_window_rect().log_if_err();
                needs_render |= !are_rects_same_size(&self.window_rect, &prev_rect);

                let update_pos_flags =
                    (!is_window_visible(self.border_window.0)).then_some(SWP_SHOWWINDOW);
                self.update_position(update_pos_flags).log_if_err();

                if needs_render {
                    self.render().log_if_err();
                }

                self.drawer.set_anims_timer_if_needed(self.border_window.0);
            }
            // EVENT_OBJECT_REORDER
            WM_APP_REORDER => {
                // First check if the tracking window still exists to avoid ghost borders
                if !is_window(Some(self.tracking_window)) {
                    self.cleanup_and_queue_exit();
                    return LRESULT(0);
                }

                // Drain any pending WM_APP_REORDER messages from the queue
                while unsafe {
                    PeekMessageW(
                        &mut MSG::default(),
                        Some(self.border_window.0),
                        WM_APP_REORDER,
                        WM_APP_REORDER,
                        PM_REMOVE,
                    )
                    .as_bool()
                } {
                    // Intentionally empty.
                }

                // Allows the immediate processing of some reorder messages, then debounces after
                const NUM_REORDERS_BEFORE_DEBOUNCE: u64 = 8;
                const DEBOUNCE_DELAY: time::Duration = time::Duration::from_millis(16);

                if let Some(last_reorder_time) = self.last_reorder_time
                    && let time_since_reorder = last_reorder_time.elapsed()
                    && time_since_reorder < DEBOUNCE_DELAY
                {
                    self.consecutive_reorders += 1;
                    if self.consecutive_reorders >= NUM_REORDERS_BEFORE_DEBOUNCE {
                        if !self.is_debouncing_reorder {
                            // Set a timer which fires a WM_TIMER message when it expires
                            unsafe {
                                SetTimer(
                                    Some(self.border_window.0),
                                    REORDER_TIMER_ID,
                                    (DEBOUNCE_DELAY - time_since_reorder).as_millis() as u32,
                                    None,
                                )
                            };
                            self.is_debouncing_reorder = true;
                        }

                        return LRESULT(0);
                    }
                } else {
                    self.consecutive_reorders = 0;
                }

                self.handle_reorder();
            }
            WM_TIMER => {
                // WPARAM contains the nIDEvent used in SetTimer
                if wparam.0 == REORDER_TIMER_ID {
                    unsafe { KillTimer(Some(window), REORDER_TIMER_ID) }.log_if_err();
                    self.is_debouncing_reorder = false;
                    self.consecutive_reorders = 0;

                    self.handle_reorder();
                }
            }
            // EVENT_SYSTEM_FOREGROUND
            WM_APP_FOREGROUND => {
                // Check if tracking window still exists to avoid ghost borders
                if !is_window(Some(self.tracking_window)) {
                    self.cleanup_and_queue_exit();
                    return LRESULT(0);
                }

                self.update_color(None);
                self.update_position(None).log_if_err();
                self.render().log_if_err();
            }
            // EVENT_OBJECT_SHOW / EVENT_OBJECT_UNCLOAKED
            // NOTE: This message can still be sent while the window is minimized.
            WM_APP_SHOWUNCLOAKED => {
                if !is_window_visible(self.tracking_window)
                    || is_window_cloaked(self.tracking_window)
                    || is_window_minimized(self.tracking_window)
                {
                    return LRESULT(0);
                }

                if self.should_show_border() {
                    self.update_color(None);
                    self.update_window_rect().log_if_err();
                    self.update_position(Some(SWP_SHOWWINDOW)).log_if_err();
                    self.render().log_if_err();

                    self.drawer.set_anims_timer_if_needed(self.border_window.0);
                }

                self.is_paused = false;
            }
            // EVENT_OBJECT_HIDE / EVENT_OBJECT_CLOAKED
            WM_APP_HIDECLOAKED => {
                self.update_position(Some(SWP_HIDEWINDOW)).log_if_err();
                self.drawer.destroy_anims_timer();
                self.is_paused = true;
            }
            // EVENT_OBJECT_MINIMIZESTART
            WM_APP_MINIMIZESTART => {
                self.update_position(Some(SWP_HIDEWINDOW)).log_if_err();

                // Needed for the fade animation to work correctly when window is unminimized
                self.drawer.active_color.set_opacity(0.0).log_if_err();
                self.drawer.inactive_color.set_opacity(0.0).log_if_err();

                self.drawer.destroy_anims_timer();
                self.is_paused = true;
            }
            // EVENT_SYSTEM_MINIMIZEEND
            WM_APP_MINIMIZEEND => {
                if !is_window_visible(self.tracking_window)
                    || is_window_cloaked(self.tracking_window)
                    || is_window_minimized(self.tracking_window)
                {
                    return LRESULT(0);
                }

                // Keep the border hidden while the tracking window is in its unminimize animation
                thread::sleep(time::Duration::from_millis(self.config.unminimize_delay));

                if self.should_show_border() {
                    self.update_color(Some(self.config.unminimize_delay));
                    self.update_window_rect().log_if_err();
                    self.update_position(Some(SWP_SHOWWINDOW)).log_if_err();
                    self.render().log_if_err();

                    self.drawer.set_anims_timer_if_needed(self.border_window.0);
                }

                self.is_paused = false;
            }
            WM_APP_ANIMATE => {
                if self.is_paused {
                    return LRESULT(0);
                }

                let bounds = self.compute_border_bounds();
                self.drawer.animate(bounds, self.window_state).log_if_err();
            }
            WM_APP_KOMOREBI => {
                let window_rule = get_window_rule(self.tracking_window);
                let global = &APP_STATE.config.read().unwrap().global;

                // Exit if komorebi colors are disabled for this tracking window
                // TODO: it might be better to store komorebi_colors in this WindowBorder struct
                if !window_rule
                    .komorebi_colors
                    .as_ref()
                    .map(|komocolors| komocolors.enabled)
                    .unwrap_or(global.komorebi_colors.enabled)
                {
                    return LRESULT(0);
                }

                let window_kind = {
                    let services = BG_SERVICES.lock().unwrap();
                    let Some(komorebi_integration) = services.komorebi_integration.as_ref() else {
                        return LRESULT(0);
                    };
                    let focus_state = komorebi_integration.focus_state.lock().unwrap();

                    *focus_state
                        .get(&(self.tracking_window.0 as isize))
                        .unwrap_or_else(|| {
                            error!("could not get window_kind for komorebi integration");
                            &WindowKind::Single
                        })
                };

                // Ignore Unfocused window kind because we already have inactive_color for that
                if window_kind == WindowKind::Unfocused {
                    return LRESULT(0);
                }

                // Komorebi's Single window kind just corresponds to a normal active window
                let single_color_config = window_rule
                    .active_color
                    .as_ref()
                    .unwrap_or(&global.active_color)
                    .clone();
                let komorebi_colors_config = window_rule
                    .komorebi_colors
                    .as_ref()
                    .unwrap_or(&global.komorebi_colors);

                let active_color_config = match window_kind {
                    WindowKind::Single => single_color_config,
                    WindowKind::Stack => komorebi_colors_config
                        .stack_color
                        .clone()
                        .unwrap_or(single_color_config),
                    WindowKind::Monocle => komorebi_colors_config
                        .monocle_color
                        .clone()
                        .unwrap_or(single_color_config),
                    WindowKind::Floating => komorebi_colors_config
                        .floating_color
                        .clone()
                        .unwrap_or(single_color_config),
                    WindowKind::Unfocused => {
                        debug!("what."); // It shouldn't be possible to reach this match branch
                        return LRESULT(0);
                    }
                };

                self.update_color_brush(true, active_color_config);
                self.render().log_if_err();
            }
            // This message is sent by the AccentWatcher when the Windows accent color changes
            WM_APP_ACCENT_CHANGED => {
                if self.refresh_accent_dependent_colors() {
                    self.render().log_if_err();
                }
            }
            // This message (and other "set" messages) are sent by the IPC server
            IpcSetColorsPayload::WND_MSG => {
                let payload = unsafe { Box::from_raw(lparam.0 as *mut IpcSetColorsPayload) };

                if let Some(active_config) = payload.active_color {
                    self.update_color_brush(true, active_config);
                }
                if let Some(inactive_config) = payload.inactive_color {
                    self.update_color_brush(false, inactive_config);
                }

                self.render().log_if_err();
            }
            IpcSetWidthPayload::WND_MSG => {
                let payload = unsafe { Box::from_raw(lparam.0 as *mut IpcSetWidthPayload) };

                self.config.width = payload.width_config;
                self.drawer.stroke_width = self.config.width_at(self.current_dpi);
                self.sync_border_radius();

                self.resize_renderer().log_if_err();
                self.update_window_rect().log_if_err();
                self.update_position(None).log_if_err();
                self.render().log_if_err();
            }
            IpcSetOffsetPayload::WND_MSG => {
                let payload = unsafe { Box::from_raw(lparam.0 as *mut IpcSetOffsetPayload) };

                self.config.offset = payload.offset_config;
                self.border_offset = self.config.offset_at(self.current_dpi);

                self.resize_renderer().log_if_err();
                self.update_window_rect().log_if_err();
                self.update_position(None).log_if_err();
                self.render().log_if_err();
            }
            IpcSetRadiusPayload::WND_MSG => {
                let payload = unsafe { Box::from_raw(lparam.0 as *mut IpcSetRadiusPayload) };

                self.config.radius = payload.radius_config;
                self.sync_border_radius();

                self.render().log_if_err();
            }
            WM_PAINT => {
                let _ = unsafe { ValidateRect(Some(window), None) };
            }
            WM_NCDESTROY => {
                // We'll set GWLP_USERDATA to 0 so that the window procedure can't find the
                // border's pointer anymore, making it stop processing our custom messages.
                unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
                self.cleanup_and_queue_exit();
            }
            // This message is sent when a display setting has changed (e.g. resolution change). It
            // is not sent when the window moves to a different monitor.
            WM_DISPLAYCHANGE => {
                // The LPARAM supposedly will contain the new? resolution of the primary display,
                // but it may not be relevant to our border window in a multi-monitor setup, so
                // we'll run our own tests to determine whether we actually need to update anything.
                let needs_render =
                    match self.rescale_border_and_resize_renderer_if_needed(self.current_monitor) {
                        Ok(is_updated) => is_updated,
                        Err(err) => {
                            error!("could not update appearance and renderer: {err:#}");
                            return LRESULT(0);
                        }
                    };

                if needs_render && is_window_visible(self.border_window.0) {
                    self.render().log_if_err();
                }
            }
            // Although we already check for DPI changes when the window moves between monitors,
            // it's possible for the DPI to change without moving to a different monitor, or
            // without even moving at all. That's why we still handle this message.
            WM_DPICHANGED => {
                // According to MSDN, the X-axis and Y-axis values for the new dpi should be
                // identical for Windows apps, so we'll just grab the X-axis value here
                let new_dpi = loword(wparam.0) as u32;
                if new_dpi != self.current_dpi {
                    self.current_dpi = new_dpi;
                    debug!("dpi has changed! new dpi: {new_dpi}");

                    // TODO: Maybe call update_window_rect and update_position, though they get
                    // handled on location change anyways
                    self.rescale_border(new_dpi);
                    self.resize_renderer().log_if_err();
                    self.render().log_if_err();
                }
            }
            // This message is sent when a device is added or removed to the system. AFAIK, it
            // doesn't directly have anything to do with GPU adapters, but we can still use it to
            // help detect adapter changes in specific scenarios (e.g. when a monitor is
            // connected/disconnected on an NVIDIA Optimus-supported laptop).
            WM_DEVICECHANGE if wparam.0 as u32 == DBT_DEVNODES_CHANGED => {
                if let Some(directx_devices) = APP_STATE.directx_devices.write().unwrap().as_mut()
                    && let Err(err) = directx_devices.recreate_if_needed()
                {
                    error!("could not recreate directx devices if needed: {err:#}");
                    self.cleanup_and_queue_exit();
                    return LRESULT(0);
                }
                if let Err(err) = self.recreate_drawer_if_needed() {
                    error!("could not recreate border drawer if needed: {err:#}");
                    self.cleanup_and_queue_exit();
                    return LRESULT(0);
                }
            }
            // This message is sent by the DisplayAdaptersWatcher
            WM_APP_RECREATE_DRAWER => {
                if let Err(err) = self.recreate_drawer_if_needed() {
                    error!("could not recreate border drawer if needed: {err:#}");
                    self.cleanup_and_queue_exit();
                }
            }
            // This message should let us know when the system enters/leaves sleep/hibernation
            WM_POWERBROADCAST => match wparam.0 as u32 {
                PBT_APMSUSPEND => {
                    debug!("system is suspending; uninitializing border drawer");
                    self.drawer.destroy_anims_timer();
                    self.drawer.uninit();
                }
                PBT_APMRESUMESUSPEND | PBT_APMRESUMEAUTOMATIC
                    if matches!(self.drawer.render_backend, RenderBackend::None) =>
                {
                    debug!("system is resuming; reinitializing border drawer");
                    if let Err(err) = self.init_drawer() {
                        error!("could not initialize border drawer in WM_POWERBROADCAST: {err:#}");
                        self.cleanup_and_queue_exit();
                        return LRESULT(0);
                    };
                    self.init_border().log_if_err();
                }
                _ => {}
            },
            // Ignore these window position messages
            WM_WINDOWPOSCHANGING | WM_WINDOWPOSCHANGED => {}
            _ => {
                return unsafe { DefWindowProcW(window, message, wparam, lparam) };
            }
        }
        LRESULT(0)
    }

    fn handle_reorder(&mut self) {
        match self.config.z_order {
            ZOrderMode::AboveWindow => {
                // When the tracking window reorders its contents, it may change the z-order. So,
                // we first check whether the border is still above the tracking window, and if
                // not, we must update its position and place it back on top
                if unsafe { GetWindow(self.tracking_window, GW_HWNDPREV) }
                    != Ok(self.border_window.0)
                {
                    self.update_position(None).log_if_err();
                }
            }
            ZOrderMode::BelowWindow => {
                if unsafe { GetWindow(self.tracking_window, GW_HWNDNEXT) }
                    != Ok(self.border_window.0)
                {
                    self.update_position(None).log_if_err();
                }
            }
        }

        self.last_reorder_time = Some(time::Instant::now());
    }
}
