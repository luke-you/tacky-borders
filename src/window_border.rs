use anyhow::{Context, anyhow};
use std::ptr;
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
use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, HMONITOR, HRGN};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, GW_HWNDPREV, GetSystemMetrics, GetWindow, HWND_TOP, LWA_ALPHA,
    SET_WINDOW_POS_FLAGS, SM_CXVIRTUALSCREEN, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOREDRAW, SWP_NOSENDCHANGING, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
    SetLayeredWindowAttributes, SetWindowPos, WS_DISABLED, WS_EX_LAYERED, WS_EX_TOOLWINDOW,
    WS_EX_TRANSPARENT, WS_POPUP,
};
use windows::core::{PCWSTR, w};

use crate::animations::{AnimType, AnimVec};
use crate::border_config::BorderConfig;
use crate::border_drawer::BorderDrawer;
use crate::colors::ColorBrushConfig;
use crate::config::{Offset, ZOrderMode};
use crate::komorebi::WindowKind;
use crate::render_backend::{RenderBackend, RenderBackendConfig};
use crate::utils::{
    LogIfErr, OwnedHWND, ReentrancyBlocker, ReentrancyBlockerExt, StandaloneWindowsError,
    T_E_ERROR, T_E_REENTRANCY, T_E_UNINIT, ToWindowsResult, WindowsCompatibleError,
    WindowsCompatibleResult, WindowsContext, get_monitor_info, get_window_rule_from_config,
    get_window_title, has_native_border, is_window_arranged, is_window_cloaked,
    is_window_minimized, is_window_visible, monitor_from_window,
};
use crate::{APP_STATE, BG_SERVICES};

struct BlurRegion(HRGN);

impl Drop for BlurRegion {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { DeleteObject(self.0.into()) };
        }
    }
}

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
    drawer: BorderDrawer,
    pub border_window: OwnedHWND,
    pub tracking_window: HWND,
    window_state: WindowState,
    window_rect: RECT,
    border_offset: Offset,
    border_padding: i32, // padding to accommodate things like shadow/glow effects
    current_monitor: HMONITOR,
    current_dpi: u32,
    config: BorderConfig, // cached config values
    is_paused: bool,
    visibility_timing: crate::border_runtime::VisibilityTiming,
}

impl Drop for WindowBorder {
    fn drop(&mut self) {
        crate::border_runtime::clear_binding(self.border_window.0);
        if self.drawer.destroy_anims_timer().is_err() {
            crate::border_runtime::record_timer_cleanup_failure();
        }
        let _ = std::mem::take(&mut self.drawer).release();
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreparationStep {
    Geometry,
    CompositionAttributes,
    Drawer,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeStep {
    Render,
    Position,
    Device,
    DrawerRecreation,
}

#[cfg(test)]
thread_local! {
    static FAIL_PREPARATION: std::cell::Cell<Option<PreparationStep>> = const { std::cell::Cell::new(None) };
    static FAIL_RUNTIME: std::cell::Cell<Option<RuntimeStep>> = const { std::cell::Cell::new(None) };
}

impl WindowBorder {
    #[cfg(test)]
    pub(crate) fn fail_runtime_for_test(step: RuntimeStep) {
        FAIL_RUNTIME.set(Some(step));
    }

    #[cfg(test)]
    fn runtime_checkpoint(step: RuntimeStep) -> WindowsCompatibleResult<()> {
        if FAIL_RUNTIME.get() == Some(step) {
            FAIL_RUNTIME.set(None);
            return Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(
                    T_E_ERROR,
                    format!("injected runtime checkpoint {step:?}"),
                ),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn fail_preparation_for_test(step: PreparationStep) {
        FAIL_PREPARATION.set(Some(step));
    }

    #[cfg(test)]
    fn preparation_checkpoint(step: PreparationStep) -> anyhow::Result<()> {
        if FAIL_PREPARATION.get() == Some(step) {
            FAIL_PREPARATION.set(None);
            anyhow::bail!("injected startup checkpoint {step:?}");
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn prepared_config(&self) -> &BorderConfig {
        &self.config
    }

    pub(crate) fn destroy_owned(&mut self) -> Vec<String> {
        self.is_paused = true;
        let mut failures = Vec::new();
        if !self.border_window.0.is_invalid()
            && let Err(error) = unsafe {
                SetWindowPos(
                    self.border_window.0,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_HIDEWINDOW
                        | SWP_NOMOVE
                        | SWP_NOSIZE
                        | SWP_NOACTIVATE
                        | SWP_NOZORDER
                        | SWP_NOSENDCHANGING,
                )
            }
        {
            failures.push(format!("hide before destruction: {error:#}"));
        }
        let timer_panicked = self
            .drawer
            .animations
            .timer
            .take()
            .is_some_and(|mut timer| {
                matches!(
                    timer.stop_and_join(),
                    crate::anim_timer::AnimationTimerExit::Panicked
                )
            });
        if timer_panicked {
            failures.push("animation worker panicked during cleanup".into());
        }
        if let Err(error) = std::mem::take(&mut self.drawer).release() {
            failures.push(format!("renderer release: {error:#}"));
        }
        if let Err(error) = self.border_window.destroy() {
            failures.push(format!("native destruction: {error:#}"));
        }
        failures
    }

    pub(crate) fn animation_failure(&mut self) -> Option<crate::anim_timer::AnimationTimerExit> {
        let timer = self.drawer.animations.timer.as_mut()?;
        match timer.poll_completion()? {
            crate::anim_timer::AnimationTimerExit::Stopped => None,
            failure => Some(failure.clone()),
        }
    }

    pub(crate) fn apply_updates(
        &mut self,
        pending: &crate::border_updates::PendingIpcUpdates,
    ) -> anyhow::Result<()> {
        let mut config = self.config.clone();
        pending.apply_to_config(&mut config);
        if config.active_color != self.config.active_color {
            self.update_color_brush(true, config.active_color.clone())?;
        }
        if config.inactive_color != self.config.inactive_color {
            self.update_color_brush(false, config.inactive_color.clone())?;
        }
        self.config = config;
        self.drawer.stroke_width = self.config.width_at(self.current_dpi);
        self.border_offset = self.config.offset_at(self.current_dpi);
        self.sync_border_radius();
        if self.is_paused || !self.should_show_border() {
            return Ok(());
        }
        self.resize_renderer()?;
        self.update_window_rect()?;
        self.update_position(None)?;
        self.render()?;
        Ok(())
    }

    pub fn new(tracking_window: HWND) -> WindowsCompatibleResult<Self> {
        let this = Self {
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
            visibility_timing: Default::default(),
        };

        crate::border_runtime::construct(this, || Self::create_window(tracking_window))
            .windows_context("could not create border window")
    }

    fn create_window(tracking_window: HWND) -> WindowsCompatibleResult<HWND> {
        let title: Vec<u16> = format!(
            "tacky-border | {} | {:?}\0",
            get_window_title(tracking_window).unwrap_or_default(),
            tracking_window
        )
        .encode_utf16()
        .collect();

        unsafe {
            Ok(CreateWindowExW(
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
                None,
            )?)
        }
    }

    pub(crate) fn prepare(&mut self, config: BorderConfig) -> anyhow::Result<()> {
        self.current_monitor = monitor_from_window(self.tracking_window);
        self.current_dpi = self.tracking_dpi()?;
        self.configure(config, self.current_dpi);
        self.update_window_rect()?;
        #[cfg(test)]
        Self::preparation_checkpoint(PreparationStep::Geometry)?;

        unsafe {
            // Make the window transparent (stole the code from PowerToys; dunno how it works).
            let pos: i32 = -GetSystemMetrics(SM_CXVIRTUALSCREEN) - 8;
            let region = BlurRegion(CreateRectRgn(pos, 0, pos + 1, 1));
            let mut bh: DWM_BLURBEHIND = Default::default();
            if !region.0.is_invalid() {
                bh = DWM_BLURBEHIND {
                    dwFlags: DWM_BB_ENABLE | DWM_BB_BLURREGION,
                    fEnable: TRUE,
                    hRgnBlur: region.0,
                    fTransitionOnMaximized: FALSE,
                };
            }
            let blur = DwmEnableBlurBehindWindow(self.border_window.0, &bh);
            drop(region);
            blur.context("could not make window transparent")?;
            SetLayeredWindowAttributes(self.border_window.0, COLORREF(0x00000000), 255, LWA_ALPHA)
                .context("could not set LWA_ALPHA")?;
            #[cfg(test)]
            Self::preparation_checkpoint(PreparationStep::CompositionAttributes)?;

            self.init_drawer()
                .context("could not initialize border drawer in init()")?;
            #[cfg(test)]
            Self::preparation_checkpoint(PreparationStep::Drawer)?;
        }

        Ok(())
    }

    pub(crate) fn activate(&mut self, hidden: bool) -> anyhow::Result<()> {
        self.is_paused = true;
        self.service_work(
            crate::border_runtime::WORK_VISIBILITY | crate::border_runtime::WORK_RENDER,
            hidden,
            time::Instant::now(),
        )
    }

    fn configure(&mut self, config: BorderConfig, dpi: u32) {
        self.config = config;
        self.drawer
            .configure_appearance(&self.config, dpi, self.tracking_window);

        self.border_offset = self.config.offset_at(dpi);
        self.border_padding = self.config.border_padding(&self.drawer);
        self.current_dpi = dpi;

        // Handle edge case where window is arranged at start
        if is_window_arranged(self.tracking_window) {
            self.sync_border_radius();
        }
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
        #[cfg(test)]
        Self::runtime_checkpoint(RuntimeStep::DrawerRecreation)?;
        if self
            .needs_drawer_recreation()
            .windows_context("could not check if border drawer needs to be recreated")?
        {
            self.init_drawer()
                .windows_context("could not recreate border drawer")?;
            self.update_color(None);
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
        #[cfg(test)]
        Self::runtime_checkpoint(RuntimeStep::Position)?;
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
            crate::utils::get_foreground_window().0 as isize,
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
    fn update_color_brush(
        &mut self,
        is_active: bool,
        config: ColorBrushConfig,
    ) -> WindowsCompatibleResult<()> {
        let config = match is_active {
            true => {
                self.config.active_color = config;
                &self.config.active_color
            }
            false => {
                self.config.inactive_color = config;
                &self.config.inactive_color
            }
        };

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
                return Err(WindowsCompatibleError::Standalone(
                    StandaloneWindowsError::new(T_E_UNINIT, "color renderer unavailable"),
                ));
            }
        };
        let brush_properties = D2D1_BRUSH_PROPERTIES {
            opacity: old_opacity,
            transform: old_transform,
        };

        brush.init_brush(renderer, &bounds, &brush_properties)
    }

    fn refresh_shared_devices(&mut self) -> WindowsCompatibleResult<()> {
        #[cfg(test)]
        Self::runtime_checkpoint(RuntimeStep::Device)?;
        let snapshot = APP_STATE
            .directx_devices
            .try_read()
            .map_err(crate::utils::shared_snapshot_error)?
            .clone();
        let Some(snapshot) = snapshot else {
            return Ok(());
        };
        if !snapshot.needs_recreation()? {
            return Ok(());
        }
        let replacement = crate::DirectXDevices::new(&APP_STATE.render_factory)?;
        let mut devices = APP_STATE
            .directx_devices
            .try_write()
            .map_err(crate::utils::shared_snapshot_error)?;
        if devices
            .as_ref()
            .is_some_and(|current| current.dxgi_device == snapshot.dxgi_device)
        {
            *devices = Some(replacement);
        }
        Ok(())
    }

    fn handle_directx_errors(
        &mut self,
        err: WindowsCompatibleError,
    ) -> WindowsCompatibleResult<()> {
        if err.code() == T_E_REENTRANCY {
            return Err(err);
        }
        thread_local! {
            static REENTRANCY_BLOCKER: ReentrancyBlocker = ReentrancyBlocker::new();
        }

        if err.code() == D2DERR_RECREATE_TARGET || err.code() == DXGI_ERROR_DEVICE_REMOVED {
            let _guard = REENTRANCY_BLOCKER
                .enter()
                .context("handle_directx_errors")
                .to_windows_result(T_E_REENTRANCY)?;

            self.refresh_shared_devices()?;
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
        #[cfg(test)]
        Self::runtime_checkpoint(RuntimeStep::Render)?;
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

    fn tracking_dpi(&self) -> anyhow::Result<u32> {
        let dpi = unsafe { GetDpiForWindow(self.tracking_window) };
        if dpi == 0 {
            return Err(anyhow!("tracking window DPI unavailable"));
        }
        Ok(dpi)
    }

    fn rescale_border_and_resize_renderer_if_needed(&mut self) -> anyhow::Result<bool> {
        let mut is_updated = false;

        let new_dpi = self.tracking_dpi()?;
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
        }

        is_updated
    }

    fn cleanup_and_queue_exit(&mut self) {
        crate::border_runtime::record_runtime_failure();
        self.is_paused = true;
    }

    #[cfg(test)]
    pub(crate) fn remove_running_renderer_for_test(&mut self) {
        self.config.follow_native_border = false;
        self.is_paused = false;
        self.drawer.render_backend = RenderBackend::None;
    }

    #[cfg(test)]
    pub(crate) fn install_animation_timer_for_test(
        &mut self,
        timer: crate::anim_timer::AnimationTimer,
    ) {
        self.drawer.animations.timer = Some(timer);
    }

    #[cfg(test)]
    pub(crate) fn enable_animation_for_test(&mut self) {
        self.config.follow_native_border = false;
        self.is_paused = false;
        self.drawer.animations = serde_json::from_value::<crate::animations::AnimationsConfig>(
            serde_json::json!({ "active": [{ "type": "Fade" }], "fps": 60, "enabled": true }),
        )
        .unwrap()
        .to_animations();
    }

    #[cfg(test)]
    pub(crate) fn set_restore_delay_for_test(&mut self, delay: u64) {
        self.config.follow_native_border = false;
        self.config.unminimize_delay = delay;
    }

    #[cfg(test)]
    pub(crate) fn observed_state_for_test(&self) -> (u32, WindowState) {
        (self.current_dpi, self.window_state)
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
        crate::border_runtime::window_proc(window, message, wparam, lparam)
    }

    pub(crate) fn restore_deadline(&self) -> Option<time::Instant> {
        self.visibility_timing.deadline()
    }

    pub(crate) fn service_work(
        &mut self,
        bits: u8,
        hidden: bool,
        now: time::Instant,
    ) -> anyhow::Result<()> {
        use crate::border_runtime::{
            VisibilityAction, WORK_ANIMATION, WORK_DISPLAY, WORK_DPI, WORK_GEOMETRY, WORK_KOMOREBI,
            WORK_RENDER, WORK_Z_ORDER,
        };
        let minimized = is_window_minimized(self.tracking_window);
        let visible = !hidden && self.should_show_border();
        let action = self
            .visibility_timing
            .observe(
                now,
                visible,
                minimized,
                time::Duration::from_millis(self.config.unminimize_delay),
            )
            .map_err(|error| anyhow!("visibility deadline: {error:?}"))?;
        if action != VisibilityAction::Visible {
            if !self.is_paused || is_window_visible(self.border_window.0) {
                self.update_position(Some(SWP_HIDEWINDOW))?;
                if minimized {
                    self.drawer.active_color.set_opacity(0.0)?;
                    self.drawer.inactive_color.set_opacity(0.0)?;
                }
            }
            self.drawer.destroy_anims_timer()?;
            self.is_paused = true;
            return Ok(());
        }
        let becoming_visible = self.is_paused;
        let mut needs_render = becoming_visible;
        if becoming_visible || bits & WORK_DISPLAY != 0 {
            self.refresh_shared_devices()?;
            if crate::border_runtime::stopped() {
                return Ok(());
            }
            self.recreate_drawer_if_needed()?;
            needs_render = true;
        }
        if crate::border_runtime::stopped() {
            return Ok(());
        }
        if becoming_visible || bits & (WORK_GEOMETRY | WORK_DPI | WORK_DISPLAY) != 0 {
            self.current_monitor = monitor_from_window(self.tracking_window);
            self.update_window_rect()?;
            self.rescale_border_and_resize_renderer_if_needed()?;
            self.sync_border_radius();
            self.update_position(becoming_visible.then_some(SWP_SHOWWINDOW))?;
            needs_render = true;
        } else if bits & WORK_Z_ORDER != 0 {
            self.update_position(None)?;
        }
        if bits & WORK_KOMOREBI != 0 {
            self.apply_komorebi_color()?;
            needs_render = true;
        }
        if crate::border_runtime::stopped() {
            return Ok(());
        }
        if becoming_visible || bits & WORK_RENDER != 0 {
            self.update_color(becoming_visible.then_some(self.config.unminimize_delay));
            needs_render = true;
        }
        self.is_paused = false;
        if bits & WORK_ANIMATION != 0 {
            self.drawer
                .animate(self.compute_border_bounds(), self.window_state)?;
        } else if needs_render {
            self.render()?;
        }
        self.drawer
            .set_anims_timer_if_needed(self.border_window.0)?;
        Ok(())
    }

    fn apply_komorebi_color(&mut self) -> anyhow::Result<()> {
        let config = APP_STATE
            .config
            .try_read()
            .map_err(crate::utils::shared_snapshot_error)?
            .clone();
        let rule = get_window_rule_from_config(self.tracking_window, &config);
        let colors = rule
            .komorebi_colors
            .as_ref()
            .unwrap_or(&config.global.komorebi_colors);
        if !colors.enabled {
            return Ok(());
        }
        let kind = {
            let services = BG_SERVICES
                .try_lock()
                .map_err(crate::utils::shared_snapshot_error)?;
            let Some(integration) = services.komorebi_integration.as_ref() else {
                return Ok(());
            };
            let focus = integration
                .focus_state
                .try_lock()
                .map_err(crate::utils::shared_snapshot_error)?;
            focus
                .get(&(self.tracking_window.0 as isize))
                .copied()
                .unwrap_or(WindowKind::Single)
        };
        let single = rule
            .active_color
            .as_ref()
            .unwrap_or(&config.global.active_color);
        let color = match kind {
            WindowKind::Single => single,
            WindowKind::Stack => colors.stack_color.as_ref().unwrap_or(single),
            WindowKind::Monocle => colors.monocle_color.as_ref().unwrap_or(single),
            WindowKind::Floating => colors.floating_color.as_ref().unwrap_or(single),
            WindowKind::Unfocused => return Ok(()),
        };
        self.update_color_brush(true, color.clone())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::mpsc;

    struct TimerExitProbe {
        hwnd: isize,
        result: mpsc::SyncSender<bool>,
    }

    impl Drop for TimerExitProbe {
        fn drop(&mut self) {
            let alive = unsafe {
                windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(HWND(self.hwnd as _)))
            }
            .as_bool();
            let _ = self.result.try_send(alive);
        }
    }

    #[test]
    #[serial_test::serial]
    fn unwind_joins_animation_before_native_window_destruction() {
        crate::register_border_window_class().unwrap();
        let (result, observed) = mpsc::sync_channel(1);
        let mut hwnd = 0;
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let mut border = WindowBorder::new(HWND::default()).unwrap();
            hwnd = border.border_window.0.0 as isize;
            let probe = TimerExitProbe { hwnd, result };
            border.drawer.animations.timer = Some(
                crate::anim_timer::AnimationTimer::spawn(
                    time::Duration::from_secs(600),
                    move || {
                        let _ = &probe;
                        Ok(())
                    },
                )
                .unwrap(),
            );
            panic!("injected panic before timer shutdown");
        }));
        assert!(panic.is_err());
        assert!(
            observed.recv_timeout(time::Duration::from_secs(2)).unwrap(),
            "native window was destroyed before timer exit"
        );
        assert!(
            !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(HWND(hwnd as _))) }
                .as_bool()
        );
    }
}
