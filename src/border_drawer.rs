use anyhow::Context;
use std::time;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D1_COLOR_F, D2D1_COMPOSITE_MODE_SOURCE_OVER,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BRUSH_PROPERTIES, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_ROUNDED_RECT, ID2D1Brush,
    ID2D1Device2, ID2D1DeviceContext, ID2D1Multithread, ID2D1RenderTarget,
};
use windows::Win32::Graphics::DirectComposition::IDCompositionSurface;
use windows::Win32::Graphics::Dxgi::{IDXGIDevice3, IDXGISurface};
use windows::core::Interface;
use windows_numerics::Matrix3x2;

use crate::APP_STATE;
use crate::animations::{AnimType, Animations};
use crate::border_config::BorderConfig;
use crate::colors::ColorBrush;
use crate::effects::Effects;
use crate::render_backend::{
    D2DMultithreadGuard, RenderBackend, RenderBackendConfig, TARGET_BITMAP_PROPS,
};
use crate::utils::{
    StandaloneWindowsError, T_E_UNINIT, ToWindowsResult, WindowsCompatibleError,
    WindowsCompatibleResult, WindowsContext, WriteLockable,
};
use crate::window_border::WindowState;

struct D2DDrawGuard<'a> {
    target: &'a ID2D1RenderTarget,
    active: bool,
}

impl<'a> D2DDrawGuard<'a> {
    fn begin(target: &'a ID2D1RenderTarget) -> Self {
        unsafe { target.BeginDraw() };
        Self {
            target,
            active: true,
        }
    }

    fn finish(&mut self) -> windows::core::Result<()> {
        self.active = false;
        unsafe { self.target.EndDraw(None, None) }
    }
}

impl Drop for D2DDrawGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = unsafe { self.target.EndDraw(None, None) };
        }
    }
}

struct CompositionDrawGuard<'a> {
    surface: &'a IDCompositionSurface,
    context: &'a ID2D1DeviceContext,
    active: bool,
}

impl<'a> CompositionDrawGuard<'a> {
    fn begin(
        surface: &'a IDCompositionSurface,
        context: &'a ID2D1DeviceContext,
        point: &mut POINT,
    ) -> WindowsCompatibleResult<(IDXGISurface, Self)> {
        let target = unsafe { surface.BeginDraw(None, point) }.windows_context("dxgi_surface")?;
        Ok((
            target,
            Self {
                surface,
                context,
                active: true,
            },
        ))
    }

    fn finish(&mut self) -> windows::core::Result<()> {
        self.active = false;
        unsafe {
            self.context.SetTarget(None);
            self.surface.EndDraw()
        }
    }
}

impl Drop for CompositionDrawGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.finish();
        }
    }
}

#[derive(Debug, Default)]
pub struct BorderDrawer {
    pub stroke_width: i32,
    // This is WriteLockable so it doesn't accidentally change when the tracking window is in
    // the snapped/arranged state where the borders are supposed to remain square
    pub corner_radius: WriteLockable<f32>,
    pub render_backend: RenderBackend,
    pub active_color: ColorBrush,
    pub inactive_color: ColorBrush,
    pub animations: Animations,
    pub effects: Effects,
    pub last_render_time: Option<time::Instant>,
    pub last_anim_time: Option<time::Instant>,
}

impl BorderDrawer {
    pub(crate) fn release(mut self) -> WindowsCompatibleResult<()> {
        let detached = match &mut self.render_backend {
            RenderBackend::V2(backend) => backend.release_references(),
            _ => Ok(()),
        };
        let device = match &self.render_backend {
            RenderBackend::V2(backend) => Some(
                unsafe { backend.d2d_context.GetDevice() }
                    .windows_context("could not capture retired renderer device")?,
            ),
            _ => None,
        };
        let dxgi = if let Some(device) = &device {
            let device: ID2D1Device2 = device
                .cast()
                .windows_context("could not capture retired DXGI device")?;
            let dxgi: IDXGIDevice3 = unsafe { device.GetDxgiDevice() }
                .windows_context("could not obtain retired DXGI device")?
                .cast()
                .windows_context("could not capture DXGI trim interface")?;
            Some(dxgi)
        } else {
            None
        };
        drop(self);
        if let Some(device) = device {
            unsafe { device.ClearResources(0) };
        }
        if let Some(dxgi) = dxgi {
            unsafe { dxgi.Trim() };
        }
        detached
    }

    pub fn configure_appearance(&mut self, config: &BorderConfig, dpi: u32, tracking_window: HWND) {
        let stroke_width = config.width_at(dpi);
        let corner_radius = config.radius_at(stroke_width, dpi, tracking_window);

        self.stroke_width = stroke_width;
        self.corner_radius = WriteLockable::new(corner_radius);
        self.active_color = config.active_color.to_color_brush(true);
        self.inactive_color = config.inactive_color.to_color_brush(false);
        self.animations = config.animations.to_animations();
        self.effects = config.effects.to_effects(dpi);
    }

    pub fn init(
        &mut self,
        width: u32,
        height: u32,
        border_window: HWND,
        bounds: D2D_RECT_F,
        render_backend_config: RenderBackendConfig,
    ) -> WindowsCompatibleResult<()> {
        // Drop our current render backend to avoid issues with recreating existing resources when
        // calling init() multiple times in a row
        self.render_backend = RenderBackend::None;

        self.render_backend = render_backend_config
            .to_render_backend(width, height, border_window, self.effects.is_enabled())
            .windows_context("could not initialize render backend in init()")?;

        let renderer: &ID2D1RenderTarget = match self.render_backend {
            RenderBackend::V2(ref backend) => &backend.d2d_context,
            RenderBackend::Legacy(ref backend) => &backend.render_target,
            RenderBackend::None => {
                return Err(WindowsCompatibleError::Standalone(
                    StandaloneWindowsError::new(T_E_UNINIT, "render backend is None"),
                ));
            }
        };

        // We will adjust opacity later. For now, we set it to 0.
        let brush_properties = D2D1_BRUSH_PROPERTIES {
            opacity: 0.0,
            transform: Matrix3x2::identity(),
        };
        self.active_color
            .init_brush(renderer, &bounds, &brush_properties)?;
        self.inactive_color
            .init_brush(renderer, &bounds, &brush_properties)?;

        if self.render_backend.supports_effects() {
            self.effects
                .init_command_lists_if_enabled(&self.render_backend)
                .windows_context("could not initialize command list")?;
        }

        Ok(())
    }

    pub fn resize_renderer(&mut self, width: u32, height: u32) -> WindowsCompatibleResult<()> {
        self.render_backend
            .resize(width, height, self.effects.is_enabled())
            .windows_context("could not update render resources")?;

        if self.render_backend.supports_effects() {
            self.effects
                .init_command_lists_if_enabled(&self.render_backend)
                .context("could not initialize command lists")
                .to_windows_result(T_E_UNINIT)?;
        }

        Ok(())
    }

    /// Renders a border onto the internal bitmap along the inside edge of the bounds. Effects
    /// (e.g. glow) are drawn outside, so the caller should pad the bounds if needed to prevent clipping.
    ///
    /// NOTE: bound coordinates should be specified relative to the bitmap, not the screen.
    pub fn render(
        &mut self,
        bounds: D2D_RECT_F,
        window_state: WindowState,
    ) -> WindowsCompatibleResult<()> {
        self.last_render_time = Some(time::Instant::now());

        // Direct2D draws a stroke centered along a given rect, but we want it to be drawn on the
        // inside of 'bounds'. To achieve this, we pad it by half the stroke width.
        let half_stroke_width = self.stroke_width as f32 / 2.0;
        let stroke_rect = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F {
                left: bounds.left + half_stroke_width,
                top: bounds.top + half_stroke_width,
                right: bounds.right - half_stroke_width,
                bottom: bounds.bottom - half_stroke_width,
            },
            radiusX: *self.corner_radius.get(),
            radiusY: *self.corner_radius.get(),
        };

        // Note that Rust's borrow checker prevents passing the render backend from the match arm,
        // so I'll need to grab it from within the respective functions instead
        match self.render_backend {
            RenderBackend::V2(_) if self.effects.should_apply(window_state) => {
                self.render_v2_with_effects(stroke_rect, bounds, window_state)?
            }
            RenderBackend::V2(_) => self.render_v2(stroke_rect, bounds, window_state)?,
            RenderBackend::Legacy(_) => self.render_legacy(stroke_rect, bounds, window_state)?,
            RenderBackend::None => {
                return Err(WindowsCompatibleError::Standalone(
                    StandaloneWindowsError::new(T_E_UNINIT, "render_backend is None"),
                ));
            }
        }

        Ok(())
    }

    fn render_legacy(
        &mut self,
        stroke_rect: D2D1_ROUNDED_RECT,
        bounds: D2D_RECT_F,
        window_state: WindowState,
    ) -> WindowsCompatibleResult<()> {
        let RenderBackend::Legacy(ref backend) = self.render_backend else {
            return Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(
                    T_E_UNINIT,
                    "could not get render_backend within render()",
                ),
            ));
        };
        let render_target = &backend.render_target;

        unsafe {
            // Determine which color should be drawn on top (for color fade animation)
            let (bottom_color, top_color) = match window_state {
                WindowState::Active => (&self.inactive_color, &self.active_color),
                WindowState::Inactive => (&self.active_color, &self.inactive_color),
            };

            render_target.BeginDraw();
            render_target.Clear(None);

            if bottom_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = bottom_color {
                    gradient.update_start_end_points(&bounds);
                }

                match bottom_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.draw_rectangle(&stroke_rect, render_target, id2d1_brush)
                    }
                    None => debug!("ID2D1Brush for bottom_color has not been created yet"),
                }
            }
            if top_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = top_color {
                    gradient.update_start_end_points(&bounds);
                }

                match top_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.draw_rectangle(&stroke_rect, render_target, id2d1_brush)
                    }
                    None => debug!("ID2D1Brush for top_color has not been created yet"),
                }
            }

            render_target.EndDraw(None, None)?;
        }

        Ok(())
    }

    fn render_v2(
        &mut self,
        stroke_rect: D2D1_ROUNDED_RECT,
        bounds: D2D_RECT_F,
        window_state: WindowState,
    ) -> WindowsCompatibleResult<()> {
        let RenderBackend::V2(ref backend) = self.render_backend else {
            return Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(
                    T_E_UNINIT,
                    "could not get render_backend within render()",
                ),
            ));
        };
        let d2d_context = &backend.d2d_context;

        unsafe {
            // Determine which color should be drawn on top (for color fade animation)
            let (bottom_color, top_color) = match window_state {
                WindowState::Active => (&self.inactive_color, &self.active_color),
                WindowState::Inactive => (&self.active_color, &self.inactive_color),
            };

            // We're about to use DirectComposition which means we will be using the underlying
            // Direct3D objects without Direct2D's knowledge. To avoid resource access conflict, we
            // must explicitly acquire a lock. Read the following article for more info:
            // https://learn.microsoft.com/en-us/windows/win32/direct2d/multi-threaded-direct2d-apps
            let d2d_multithread: ID2D1Multithread = APP_STATE
                .render_factory
                .cast()
                .windows_context("d2d_multithread")?;
            let _multithread_guard = D2DMultithreadGuard::enter(d2d_multithread);
            #[cfg(test)]
            tests::inject_render_failure(tests::RenderFailureStage::Locked)?;

            // Set d2d_context's target back to the target_bitmap so we can draw to the display
            let mut point = POINT::default();
            let (dxgi_surface, mut surface_draw) =
                CompositionDrawGuard::begin(&backend.d_comp_surface, d2d_context, &mut point)?;
            let target_bitmap = d2d_context
                .CreateBitmapFromDxgiSurface(&dxgi_surface, Some(&TARGET_BITMAP_PROPS))
                .windows_context("target_bitmap")?;
            d2d_context.SetTarget(&target_bitmap);

            // Draw to the target_bitmap
            let mut draw = D2DDrawGuard::begin(d2d_context);
            #[cfg(test)]
            tests::inject_render_failure(tests::RenderFailureStage::Drawing)?;
            d2d_context.Clear(None);

            if bottom_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = bottom_color {
                    gradient.update_start_end_points(&bounds);
                }

                match bottom_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.draw_rectangle(&stroke_rect, d2d_context, id2d1_brush)
                    }
                    None => debug!("ID2D1Brush for bottom_color has not been created yet"),
                }
            }
            if top_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = top_color {
                    gradient.update_start_end_points(&bounds);
                }

                match top_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.draw_rectangle(&stroke_rect, d2d_context, id2d1_brush)
                    }
                    None => debug!("ID2D1Brush for top_color has not been created yet"),
                }
            }

            draw.finish()?;

            surface_draw
                .finish()
                .windows_context("d_comp_surface.EndDraw()")?;
            backend
                .d_comp_device
                .Commit()
                .windows_context("d_comp_device.Commit()")?;
        }

        Ok(())
    }

    fn render_v2_with_effects(
        &mut self,
        stroke_rect: D2D1_ROUNDED_RECT,
        bounds: D2D_RECT_F,
        window_state: WindowState,
    ) -> WindowsCompatibleResult<()> {
        let RenderBackend::V2(ref backend) = self.render_backend else {
            return Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(
                    T_E_UNINIT,
                    "could not get render_backend within render()",
                ),
            ));
        };
        let d2d_context = &backend.d2d_context;

        let half_stroke_width = stroke_rect.rect.left - bounds.left;

        unsafe {
            // Determine which color should be drawn on top (for color fade animation)
            let (bottom_color, top_color) = match window_state {
                WindowState::Active => (&self.inactive_color, &self.active_color),
                WindowState::Inactive => (&self.active_color, &self.inactive_color),
            };

            // Create a rect that covers up to the outer edge of the border
            let border_outer_rect = D2D1_ROUNDED_RECT {
                rect: bounds,
                radiusX: stroke_rect.radiusX + half_stroke_width,
                radiusY: stroke_rect.radiusY + half_stroke_width,
            };

            // Set the d2d_context target to the border_bitmap
            let border_bitmap = backend
                .border_bitmap
                .as_ref()
                .context("could not get border_bitmap")
                .to_windows_result(T_E_UNINIT)?;
            d2d_context.SetTarget(border_bitmap);

            // Draw to the border_bitmap
            let mut draw = D2DDrawGuard::begin(d2d_context);
            d2d_context.Clear(None);

            // We use filled rectangles here because it helps make the effects more visible.
            // Additionally, if someone sets the stroke width to 0, the effects will still be
            // visible (whereas they wouldn't be if we used a hollow rectangle).
            if bottom_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = bottom_color {
                    gradient.update_start_end_points(&bounds);
                }

                match bottom_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.fill_rectangle(&border_outer_rect, d2d_context, id2d1_brush)
                    }
                    None => debug!("ID2D1Brush for bottom_color has not been created yet"),
                }
            }
            if top_color.get_opacity().to_windows_result(T_E_UNINIT)? > 0.0 {
                if let ColorBrush::Gradient(gradient) = top_color {
                    gradient.update_start_end_points(&bounds);
                }

                match top_color.get_brush() {
                    Some(id2d1_brush) => {
                        self.fill_rectangle(&border_outer_rect, d2d_context, id2d1_brush)
                    }
                    None => debug!("ID2D1Brush for top_color has not been created yet"),
                }
            }

            draw.finish()?;
        }

        unsafe {
            // Set the d2d_context target to the mask_bitmap to create an alpha mask
            let mask_bitmap = backend
                .mask_bitmap
                .as_ref()
                .context("could not get mask_bitmap")
                .to_windows_result(T_E_UNINIT)?;
            d2d_context.SetTarget(mask_bitmap);

            // Create a rect that covers up to the inner edge of the border
            // This rect is used to mask out the inner portion of the border
            let border_inner_rect = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: stroke_rect.rect.left + half_stroke_width,
                    top: stroke_rect.rect.top + half_stroke_width,
                    right: stroke_rect.rect.right - half_stroke_width,
                    bottom: stroke_rect.rect.bottom - half_stroke_width,
                },
                radiusX: stroke_rect.radiusX - half_stroke_width,
                radiusY: stroke_rect.radiusY - half_stroke_width,
            };

            // Create a 100% opaque brush because our active/inactive colors' brushes might not be
            let opaque_brush = d2d_context.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
                None,
            )?;

            let mut draw = D2DDrawGuard::begin(d2d_context);
            d2d_context.Clear(None);

            self.fill_rectangle(&border_inner_rect, d2d_context, &opaque_brush);

            draw.finish()?;
        }

        unsafe {
            // We're about to use DirectComposition which means we will be using the underlying
            // Direct3D objects without Direct2D's knowledge. To avoid resource access conflict, we
            // must explicitly acquire a lock. Read the following article for more info:
            // https://learn.microsoft.com/en-us/windows/win32/direct2d/multi-threaded-direct2d-apps
            let d2d_multithread: ID2D1Multithread = APP_STATE
                .render_factory
                .cast()
                .windows_context("d2d_multithread")?;
            let _multithread_guard = D2DMultithreadGuard::enter(d2d_multithread);
            #[cfg(test)]
            tests::inject_render_failure(tests::RenderFailureStage::Locked)?;

            // Set d2d_context's target back to the target_bitmap so we can draw to the display
            let mut point = POINT::default();
            let (dxgi_surface, mut surface_draw) =
                CompositionDrawGuard::begin(&backend.d_comp_surface, d2d_context, &mut point)?;
            let target_bitmap = d2d_context
                .CreateBitmapFromDxgiSurface(&dxgi_surface, Some(&TARGET_BITMAP_PROPS))
                .windows_context("target_bitmap")?;
            d2d_context.SetTarget(&target_bitmap);

            // Retrieve our command list (includes border_bitmap, mask_bitmap, and effects)
            let command_list = self
                .effects
                .get_current_command_list(window_state)
                .to_windows_result(T_E_UNINIT)?;

            // Draw to the target_bitmap
            let mut draw = D2DDrawGuard::begin(d2d_context);
            #[cfg(test)]
            tests::inject_render_failure(tests::RenderFailureStage::Drawing)?;
            d2d_context.Clear(None);

            d2d_context.DrawImage(
                command_list,
                None,
                None,
                D2D1_INTERPOLATION_MODE_LINEAR,
                D2D1_COMPOSITE_MODE_SOURCE_OVER,
            );

            draw.finish()?;

            surface_draw
                .finish()
                .windows_context("d_comp_surface.EndDraw()")?;
            backend
                .d_comp_device
                .Commit()
                .windows_context("d_comp_device.Commit()")?;
        }

        Ok(())
    }

    // NOTE: ID2D1DeviceContext implements From<&ID2D1DeviceContext> for &ID2D1RenderTarget
    fn draw_rectangle(
        &self,
        stroke_rect: &D2D1_ROUNDED_RECT,
        renderer: &ID2D1RenderTarget,
        brush: &ID2D1Brush,
    ) {
        unsafe {
            match stroke_rect.radiusX {
                0.0 => {
                    renderer.DrawRectangle(&stroke_rect.rect, brush, self.stroke_width as f32, None)
                }
                _ => renderer.DrawRoundedRectangle(
                    stroke_rect,
                    brush,
                    self.stroke_width as f32,
                    None,
                ),
            }
        }
    }

    // NOTE: ID2D1DeviceContext implements From<&ID2D1DeviceContext> for &ID2D1RenderTarget
    fn fill_rectangle(
        &self,
        rounded_rect: &D2D1_ROUNDED_RECT,
        renderer: &ID2D1RenderTarget,
        brush: &ID2D1Brush,
    ) {
        unsafe {
            match rounded_rect.radiusX {
                0.0 => renderer.FillRectangle(&rounded_rect.rect, brush),
                _ => renderer.FillRoundedRectangle(rounded_rect, brush),
            }
        }
    }

    pub fn set_anims_timer_if_needed(
        &mut self,
        border_window: HWND,
    ) -> Result<(), crate::anim_timer::AnimationTimerExit> {
        self.animations
            .set_timer_if_needed(border_window, &mut self.last_anim_time)
    }

    pub fn destroy_anims_timer(&mut self) -> Result<(), crate::anim_timer::AnimationTimerExit> {
        self.animations.destroy_timer()
    }

    pub fn animate(&mut self, bounds: D2D_RECT_F, window_state: WindowState) -> anyhow::Result<()> {
        let anim_elapsed = self
            .last_anim_time
            .get_or_insert_with(time::Instant::now)
            .elapsed();
        let render_elapsed = self
            .last_render_time
            .get_or_insert_with(time::Instant::now)
            .elapsed();

        let mut update = false;

        for anim_params in self.animations.get_current(window_state).clone().iter() {
            match anim_params.anim_type {
                AnimType::Spiral | AnimType::ReverseSpiral => {
                    self.animations.animate_spiral(
                        &bounds,
                        &self.active_color,
                        &self.inactive_color,
                        &anim_elapsed,
                        anim_params,
                    );
                    update = true;
                }
                AnimType::Fade => {
                    let correct_active_opacity = if window_state == WindowState::Active {
                        1.0
                    } else {
                        0.0
                    };

                    if self.active_color.get_opacity()? != correct_active_opacity
                        || self.inactive_color.get_opacity()? != 1.0 - correct_active_opacity
                    {
                        self.animations.animate_fade(
                            window_state,
                            &self.active_color,
                            &self.inactive_color,
                            &anim_elapsed,
                            anim_params,
                        )?;
                        update = true;
                    }
                }
            }
        }

        self.last_anim_time = Some(time::Instant::now());

        let render_interval =
            crate::animations::frame_interval_for_fps(self.animations.fps)?.as_secs_f32();
        let time_diff = render_elapsed.as_secs_f32() - render_interval;
        if update && (time_diff.abs() <= 0.001 || time_diff >= 0.0) {
            self.render(bounds, window_state)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::OwnedHWND;
    use windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_U;
    use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, WINDOW_EX_STYLE, WS_POPUP};

    thread_local! {
        static RENDER_FAILURE: std::cell::Cell<Option<(RenderFailureStage, bool)>> = const { std::cell::Cell::new(None) };
    }

    #[derive(Clone, Copy, PartialEq)]
    pub(super) enum RenderFailureStage {
        Locked,
        Drawing,
    }

    pub(super) fn inject_render_failure(stage: RenderFailureStage) -> WindowsCompatibleResult<()> {
        let failure = RENDER_FAILURE.with(|failure| {
            let (expected, panicking) = failure.get()?;
            if expected != stage {
                return None;
            }
            failure.set(None);
            Some(panicking)
        });
        match failure {
            Some(true) => panic!("injected render unwind with factory lock held"),
            Some(false) => Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(
                    crate::utils::T_E_ERROR,
                    "injected render error with factory lock held",
                ),
            )),
            None => Ok(()),
        }
    }

    fn render_window() -> anyhow::Result<OwnedHWND> {
        Ok(OwnedHWND(unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                windows::core::w!("STATIC"),
                windows::core::w!("fix-01-shared-device-renderer"),
                WS_POPUP,
                0,
                0,
                64,
                64,
                None,
                None,
                None,
                None,
            )?
        }))
    }

    struct RenderFaultChild(std::process::Child);

    impl Drop for RenderFaultChild {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn v2_render_failures_release_factory_lock_in_bounded_child() -> anyhow::Result<()> {
        let mut child = RenderFaultChild(
            std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "border_drawer::tests::render_fault_helper",
                    "--ignored",
                    "--nocapture",
                ])
                .env("FIX01_RENDER_FAULT_CHILD", "1")
                .spawn()?,
        );
        let deadline = time::Instant::now() + time::Duration::from_secs(20);
        loop {
            if let Some(status) = child.0.try_wait()? {
                anyhow::ensure!(status.success(), "renderer fault helper failed: {status}");
                return Ok(());
            }
            anyhow::ensure!(
                time::Instant::now() < deadline,
                "renderer fault helper exceeded its external deadline"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    #[ignore = "native lock faults run only in an externally bounded child"]
    fn render_fault_helper() -> anyhow::Result<()> {
        anyhow::ensure!(std::env::var("FIX01_RENDER_FAULT_CHILD")? == "1");
        if APP_STATE.get_directx_devices_mut().is_none() {
            let devices = crate::DirectXDevices::new(APP_STATE.get_render_factory())?;
            *APP_STATE.get_directx_devices_mut() = Some(devices);
        }
        let bounds = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: 32.0,
            bottom: 32.0,
        };
        for effects in [false, true] {
            for stage in [RenderFailureStage::Locked, RenderFailureStage::Drawing] {
                for panicking in [false, true] {
                    let window = render_window()?;
                    let mut config = BorderConfig::default();
                    if effects {
                        config.effects = serde_json::from_value(serde_json::json!({
                            "enabled": true,
                            "active": [{"type": "Glow"}],
                            "inactive": [{"type": "Glow"}]
                        }))?;
                    }
                    let mut drawer = BorderDrawer::default();
                    drawer.configure_appearance(&config, 96, HWND::default());
                    drawer.init(64, 64, window.0, bounds, RenderBackendConfig::V2)?;
                    drawer.render(bounds, WindowState::Active)?;
                    RENDER_FAILURE.with(|failure| failure.set(Some((stage, panicking))));
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        drawer.render(bounds, WindowState::Active)
                    }));
                    if panicking {
                        assert!(result.is_err());
                        if let Err(payload) = result {
                            crate::border_registry::discard_panic(payload);
                        }
                    } else {
                        assert_eq!(result.unwrap().unwrap_err().code(), crate::utils::T_E_ERROR);
                    }
                    std::thread::spawn(move || -> anyhow::Result<()> {
                        let multithread: ID2D1Multithread = APP_STATE.render_factory.cast()?;
                        drop(D2DMultithreadGuard::enter(multithread));
                        let survivor_window = render_window()?;
                        let mut survivor = BorderDrawer::default();
                        survivor.configure_appearance(
                            &BorderConfig::default(),
                            96,
                            HWND::default(),
                        );
                        survivor.init(
                            64,
                            64,
                            survivor_window.0,
                            bounds,
                            RenderBackendConfig::V2,
                        )?;
                        survivor.render(bounds, WindowState::Active)?;
                        survivor.release()?;
                        Ok(())
                    })
                    .join()
                    .unwrap()?;
                    drawer.render(bounds, WindowState::Active)?;
                    drawer.release()?;
                }
            }
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn retiring_v2_drawer_preserves_live_shared_device_renderer() -> anyhow::Result<()> {
        if APP_STATE.get_directx_devices_mut().is_none() {
            let devices = crate::DirectXDevices::new(APP_STATE.get_render_factory())?;
            *APP_STATE.get_directx_devices_mut() = Some(devices);
        }
        let live_window = render_window()?;
        let retired_window = render_window()?;
        let bounds = D2D_RECT_F {
            left: 0.0,
            top: 0.0,
            right: 32.0,
            bottom: 32.0,
        };
        let mut live = BorderDrawer::default();
        live.configure_appearance(&BorderConfig::default(), 96, HWND::default());
        live.init(64, 64, live_window.0, bounds, RenderBackendConfig::V2)?;
        live.render(bounds, WindowState::default())?;
        let mut retired = BorderDrawer::default();
        retired.configure_appearance(&BorderConfig::default(), 96, HWND::default());
        retired.init(64, 64, retired_window.0, bounds, RenderBackendConfig::V2)?;
        match (&live.render_backend, &retired.render_backend) {
            (RenderBackend::V2(live), RenderBackend::V2(retired)) => {
                assert_eq!(unsafe { live.d2d_context.GetDevice()? }, unsafe {
                    retired.d2d_context.GetDevice()?
                });
            }
            _ => panic!("shared-device check requires V2 renderers"),
        }
        retired.release()?;
        drop(retired_window);
        live.resize_renderer(80, 80)?;
        live.render(bounds, WindowState::default())?;
        assert_eq!(
            live.render_backend.get_pixel_size()?,
            D2D_SIZE_U {
                width: 80,
                height: 80
            }
        );
        live.release()?;
        Ok(())
    }
}
