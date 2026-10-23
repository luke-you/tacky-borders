use anyhow::Context;
use serde::Deserialize;
use std::mem::ManuallyDrop;
use windows::Win32::Foundation::{HWND, LUID};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS, D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_DEVICE_CONTEXT_OPTIONS_NONE,
    D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_IMMEDIATELY,
    D2D1_PRESENT_OPTIONS_RETAIN_CONTENTS, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, ID2D1Bitmap1, ID2D1DeviceContext, ID2D1HwndRenderTarget,
    ID2D1Multithread,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice3, IDCompositionDesktopDevice, IDCompositionDevice3,
    IDCompositionSurface, IDCompositionTarget, IDCompositionVisual2,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
};
use windows::core::Interface;

use crate::APP_STATE;
use crate::utils::{
    LogIfErr, StandaloneWindowsError, T_E_ERROR, T_E_REENTRANCY, T_E_UNINIT, ToWindowsResult,
    WindowsCompatibleError, WindowsCompatibleResult, WindowsContext,
};

pub(crate) struct D2DMultithreadGuard(ID2D1Multithread);

impl D2DMultithreadGuard {
    pub(crate) fn enter(multithread: ID2D1Multithread) -> Self {
        unsafe { multithread.Enter() };
        Self(multithread)
    }
}

impl Drop for D2DMultithreadGuard {
    fn drop(&mut self) {
        unsafe { self.0.Leave() };
    }
}

pub const TARGET_BITMAP_PROPS: D2D1_BITMAP_PROPERTIES1 = D2D1_BITMAP_PROPERTIES1 {
    bitmapOptions: D2D1_BITMAP_OPTIONS(
        D2D1_BITMAP_OPTIONS_TARGET.0 | D2D1_BITMAP_OPTIONS_CANNOT_DRAW.0,
    ),
    pixelFormat: D2D1_PIXEL_FORMAT {
        format: DXGI_FORMAT_B8G8R8A8_UNORM,
        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
    },
    // TODO: Test scaling using these dpiX dpiY instead of manually scaling stroke width myself.
    // If I remember correctly, using these could lead to blurry edges, but I want to try again.
    dpiX: 96.0,
    dpiY: 96.0,
    colorContext: ManuallyDrop::new(None),
};
pub const EXTRA_BITMAP_PROPS: D2D1_BITMAP_PROPERTIES1 = D2D1_BITMAP_PROPERTIES1 {
    bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
    pixelFormat: D2D1_PIXEL_FORMAT {
        format: DXGI_FORMAT_B8G8R8A8_UNORM,
        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
    },
    dpiX: 96.0,
    dpiY: 96.0,
    colorContext: ManuallyDrop::new(None),
};

#[derive(Debug, Default, Clone, Copy, Deserialize, PartialEq)]
pub enum RenderBackendConfig {
    #[default]
    V2,
    Legacy,
}

#[derive(Debug, Default, Clone)]
pub enum RenderBackend {
    V2(V2RenderBackend),
    Legacy(LegacyRenderBackend),
    #[default]
    None,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct V2RenderBackend {
    pub d2d_context: ID2D1DeviceContext,
    pub d_comp_device: IDCompositionDevice3,
    pub d_comp_target: IDCompositionTarget,
    pub d_comp_visual: IDCompositionVisual2,
    // I might be doing something wrong, but it seems like 'd_comp_surface' MUST be dropped before
    // 'd_comp_target' or else we will have lingering resources. Thus, I'll wrap it in ManuallyDrop
    // to let me do so in the Drop impl (could also use Option, but that has worse ergonomics)
    pub d_comp_surface: ManuallyDrop<IDCompositionSurface>,
    pub surface_size: D2D_SIZE_U,
    pub border_bitmap: Option<ID2D1Bitmap1>,
    pub mask_bitmap: Option<ID2D1Bitmap1>,
    pub adapter_luid: LUID,
}

#[derive(Debug, Clone)]
pub struct LegacyRenderBackend {
    pub render_target: ID2D1HwndRenderTarget,
}

impl RenderBackendConfig {
    pub fn to_render_backend(
        self,
        width: u32,
        height: u32,
        border_window: HWND,
        create_extra_bitmaps: bool,
    ) -> WindowsCompatibleResult<RenderBackend> {
        match self {
            RenderBackendConfig::V2 => Ok(RenderBackend::V2(V2RenderBackend::new(
                width,
                height,
                border_window,
                create_extra_bitmaps,
            )?)),
            RenderBackendConfig::Legacy => Ok(RenderBackend::Legacy(LegacyRenderBackend::new(
                border_window,
            )?)),
        }
    }
}

impl RenderBackend {
    pub fn resize(
        &mut self,
        width: u32,
        height: u32,
        create_extra_bitmaps: bool,
    ) -> WindowsCompatibleResult<()> {
        match self {
            RenderBackend::V2(backend) => {
                backend.resize(width, height, create_extra_bitmaps)?;
            }
            RenderBackend::Legacy(backend) => {
                backend.resize(width, height)?;
            }
            RenderBackend::None => {
                return Err(WindowsCompatibleError::Standalone(
                    StandaloneWindowsError::new(T_E_UNINIT, "render backend is None"),
                ));
            }
        }

        Ok(())
    }

    pub fn get_pixel_size(&self) -> WindowsCompatibleResult<D2D_SIZE_U> {
        match self {
            RenderBackend::V2(backend) => Ok(backend.surface_size),
            RenderBackend::Legacy(backend) => {
                let pixel_size = unsafe { backend.render_target.GetPixelSize() };
                Ok(pixel_size)
            }
            RenderBackend::None => Err(WindowsCompatibleError::Standalone(
                StandaloneWindowsError::new(T_E_UNINIT, "render backend is None"),
            )),
        }
    }

    pub fn supports_effects(&self) -> bool {
        !matches!(self, RenderBackend::Legacy(_) | RenderBackend::None)
    }
}

impl V2RenderBackend {
    pub fn new(
        width: u32,
        height: u32,
        border_window: HWND,
        create_extra_bitmaps: bool,
    ) -> WindowsCompatibleResult<Self> {
        let (d2d_device, dxgi_device) = {
            let directx_devices_opt = APP_STATE.directx_devices.try_read().map_err(|error| {
                let code = match error {
                    std::sync::TryLockError::WouldBlock => T_E_REENTRANCY,
                    std::sync::TryLockError::Poisoned(_) => T_E_ERROR,
                };
                WindowsCompatibleError::Standalone(StandaloneWindowsError::new(
                    code,
                    "device snapshot unavailable",
                ))
            })?;
            let directx_devices = directx_devices_opt
                .as_ref()
                .context("could not get direct_devices")
                .to_windows_result(T_E_UNINIT)?;
            (
                directx_devices.d2d_device.clone(),
                directx_devices.dxgi_device.clone(),
            )
        };

        let d2d_context =
            unsafe { d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE) }
                .windows_context("d2d_context")?;

        unsafe {
            d2d_context.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);

            // Acquire a lock to prevent resource access conflict
            let d2d_multithread: ID2D1Multithread = APP_STATE
                .render_factory
                .cast()
                .windows_context("d2d_multithread")?;
            d2d_multithread.Enter();

            let dxgi_adapter = dxgi_device.GetAdapter().windows_context("dxgi_adapter")?;

            // Not only does IDCompositionDevice3 not implement CreateTargetForHwnd, but you can't
            // even create one using DCompositionCreateDevice3 without casting (but you can with
            // IDCompositionDevice4... why?). Instead, we'll create IDCompositionDesktopDevice
            // first, which does implement CreateTargetForHwnd, then cast() it.
            let d_comp_desktop_device: IDCompositionDesktopDevice =
                DCompositionCreateDevice3(&dxgi_device).windows_context("d_comp_desktop_device")?;
            let d_comp_target = d_comp_desktop_device
                .CreateTargetForHwnd(border_window, true)
                .windows_context("d_comp_target")?;

            let d_comp_device: IDCompositionDevice3 = d_comp_desktop_device
                .cast()
                .windows_context("d_comp_desktop_device.cast()")?;

            let d_comp_visual = d_comp_device
                .CreateVisual()
                .windows_context("d_comp_visual")?;
            let d_comp_surface = d_comp_device
                .CreateSurface(
                    width,
                    height,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_ALPHA_MODE_PREMULTIPLIED,
                )
                .windows_context("d_comp_surface")?;
            d_comp_visual
                .SetContent(&d_comp_surface)
                .windows_context("d_comp_visual.SetContent()")?;
            d_comp_target
                .SetRoot(&d_comp_visual)
                .windows_context("d_comp_target.SetRoot()")?;
            d_comp_device
                .Commit()
                .windows_context("d_comp_device.Commit()")?;

            d2d_multithread.Leave();

            let (border_bitmap_opt, mask_bitmap_opt) = if create_extra_bitmaps {
                let (border_bitmap, mask_bitmap) =
                    Self::create_extra_bitmaps(&d2d_context, width, height)?;
                (Some(border_bitmap), Some(mask_bitmap))
            } else {
                (None, None)
            };

            // The LUID identifies the GPU adapter this render backend was initialized with. It's
            // used to help determine when the primary GPU adapter of the system has changed.
            let adapter_desc = dxgi_adapter.GetDesc().windows_context("adapter_desc")?;
            let adapter_luid = adapter_desc.AdapterLuid;

            Ok(Self {
                d2d_context,
                d_comp_device,
                d_comp_target,
                d_comp_visual,
                d_comp_surface: ManuallyDrop::new(d_comp_surface),
                surface_size: D2D_SIZE_U { width, height },
                border_bitmap: border_bitmap_opt,
                mask_bitmap: mask_bitmap_opt,
                adapter_luid,
            })
        }
    }

    fn create_extra_bitmaps(
        d2d_context: &ID2D1DeviceContext,
        width: u32,
        height: u32,
    ) -> WindowsCompatibleResult<(ID2D1Bitmap1, ID2D1Bitmap1)> {
        // We need border_bitmap because you cannot directly apply effects on target_bitmap
        // due to its D2D1_BITMAP_OPTIONS, so we'll apply effects on border_bitmap instead
        let border_bitmap = unsafe {
            d2d_context.CreateBitmap(D2D_SIZE_U { width, height }, None, 0, &EXTRA_BITMAP_PROPS)
        }
        .windows_context("border_bitmap")?;

        // Aaaand we need yet another bitmap to serve as a mask for the border_bitmap
        let mask_bitmap = unsafe {
            d2d_context.CreateBitmap(D2D_SIZE_U { width, height }, None, 0, &EXTRA_BITMAP_PROPS)
        }
        .windows_context("mask_bitmap")?;

        Ok((border_bitmap, mask_bitmap))
    }

    pub fn release_references(&mut self) -> WindowsCompatibleResult<()> {
        unsafe {
            let d2d_multithread: ID2D1Multithread = APP_STATE
                .render_factory
                .cast()
                .windows_context("d2d_multithread")?;
            let _multithread_guard = D2DMultithreadGuard::enter(d2d_multithread);

            self.d2d_context.SetTarget(None);

            self.d_comp_visual
                .SetContent(None)
                .windows_context("d_comp_visual.SetContent()")?;
            self.d_comp_target
                .SetRoot(None)
                .windows_context("d_comp_target.SetRoot()")?;
            self.d_comp_device
                .Commit()
                .windows_context("d_comp_device.Commit()")?;
        }

        Ok(())
    }

    // NOTE: after updating resources, we also need to update anything that relies on references to
    // the resources (e.g. border effects)
    pub fn resize(
        &mut self,
        width: u32,
        height: u32,
        create_extra_bitmaps: bool,
    ) -> WindowsCompatibleResult<()> {
        self.release_references()
            .windows_context("could not release renderer references")?;
        self.border_bitmap = None;
        self.mask_bitmap = None;

        unsafe {
            // Acquire a lock to prevent resource access conflict
            let d2d_multithread: ID2D1Multithread = APP_STATE
                .render_factory
                .cast()
                .windows_context("d2d_multithread")?;
            d2d_multithread.Enter();

            *self.d_comp_surface = self
                .d_comp_device
                .CreateSurface(
                    width,
                    height,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_ALPHA_MODE_PREMULTIPLIED,
                )
                .windows_context("d_comp_surface")?;
            self.d_comp_visual
                .SetContent(&*self.d_comp_surface)
                .windows_context("d_comp_visual.SetContent()")?;
            self.d_comp_target
                .SetRoot(&self.d_comp_visual)
                .windows_context("d_comp_visual.SetContent()")?;
            self.d_comp_device
                .Commit()
                .windows_context("d_comp_device.Commit()")?;

            d2d_multithread.Leave();
        }
        self.surface_size = D2D_SIZE_U { width, height };

        (self.border_bitmap, self.mask_bitmap) = if create_extra_bitmaps {
            let (border_bitmap, mask_bitmap) =
                Self::create_extra_bitmaps(&self.d2d_context, width, height)?;
            (Some(border_bitmap), Some(mask_bitmap))
        } else {
            (None, None)
        };

        Ok(())
    }
}

impl Drop for V2RenderBackend {
    fn drop(&mut self) {
        self.release_references()
            .context("could not drop V2RenderBackend: could not release renderer references")
            .log_if_err();

        // Like mentioned in a comment near the struct declaration, 'd_comp_surface' MUST be
        // dropped before other struct fields, so we will do it here.
        unsafe { ManuallyDrop::drop(&mut self.d_comp_surface) }
    }
}

impl LegacyRenderBackend {
    pub fn new(border_window: HWND) -> WindowsCompatibleResult<Self> {
        let render_target_properties = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_UNKNOWN,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };
        let hwnd_render_target_properties = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd: border_window,
            pixelSize: Default::default(),
            presentOptions: D2D1_PRESENT_OPTIONS_RETAIN_CONTENTS | D2D1_PRESENT_OPTIONS_IMMEDIATELY,
        };

        unsafe {
            let render_target = APP_STATE
                .render_factory
                .CreateHwndRenderTarget(&render_target_properties, &hwnd_render_target_properties)
                .windows_context("render_target")?;

            render_target.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);

            Ok(Self { render_target })
        }
    }

    pub fn resize(&self, width: u32, height: u32) -> WindowsCompatibleResult<()> {
        let pixel_size = D2D_SIZE_U { width, height };
        unsafe { self.render_target.Resize(&pixel_size) }
            .windows_context("could not resize render_target")?;

        Ok(())
    }
}
