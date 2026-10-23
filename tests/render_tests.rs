#[path = "test_support.rs"]
mod test_support;

use tacky_borders::border_config::BorderConfig;
use tacky_borders::border_drawer::BorderDrawer;
use tacky_borders::config::{OffsetConfig, RadiusConfig, WidthConfig};
use tacky_borders::render_backend::{RenderBackend, RenderBackendConfig};
use tacky_borders::window_border::{WindowBorder, WindowState};
use tacky_borders::{APP_STATE, DirectXDevices, register_border_window_class};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D_SIZE_U};

fn prepare_v2_render_backend() -> anyhow::Result<()> {
    test_support::initialize();
    APP_STATE.update_config(|config| config.render_backend = RenderBackendConfig::V2)?;
    let render_factory = APP_STATE.get_render_factory();
    *APP_STATE.get_directx_devices_mut() = Some(DirectXDevices::new(render_factory)?);

    Ok(())
}

#[test]
fn test_legacy_render_backend() -> anyhow::Result<()> {
    test_support::initialize();
    register_border_window_class()?;
    let border = WindowBorder::new(HWND::default())?;
    let mut backend =
        RenderBackendConfig::Legacy.to_render_backend(640, 480, border.border_window.0, false)?;
    assert!(matches!(backend, RenderBackend::Legacy(_)));
    backend.resize(640, 480, false)?;
    assert_eq!(
        backend.get_pixel_size()?,
        D2D_SIZE_U {
            width: 640,
            height: 480
        }
    );
    Ok(())
}

#[test]
fn test_render_backend_v2_with_extra_bitmaps() -> anyhow::Result<()> {
    test_support::initialize();
    register_border_window_class()?;
    let border = WindowBorder::new(HWND::default())?;
    let hwnd = border.border_window.0;
    prepare_v2_render_backend()?;

    let render_backend = RenderBackendConfig::V2.to_render_backend(1920, 1080, hwnd, true)?;
    if let RenderBackend::V2(ref backend) = render_backend {
        assert!(backend.mask_bitmap.is_some());
        assert!(backend.border_bitmap.is_some());
        assert!(
            render_backend.get_pixel_size()?
                == D2D_SIZE_U {
                    width: 1920,
                    height: 1080
                }
        );
    } else {
        panic!("created incorrect render backend");
    }

    Ok(())
}

#[test]
fn test_render_backend_v2_without_extra_bitmaps() -> anyhow::Result<()> {
    test_support::initialize();
    register_border_window_class()?;
    let border = WindowBorder::new(HWND::default())?;
    let hwnd = border.border_window.0;
    prepare_v2_render_backend()?;

    let render_backend = RenderBackendConfig::V2.to_render_backend(1920, 1080, hwnd, false)?;
    if let RenderBackend::V2(ref backend) = render_backend {
        assert!(backend.mask_bitmap.is_none());
        assert!(backend.border_bitmap.is_none());
        assert!(
            render_backend.get_pixel_size()?
                == D2D_SIZE_U {
                    width: 1920,
                    height: 1080
                }
        );
    } else {
        panic!("created incorrect render backend");
    }

    Ok(())
}

#[test]
fn test_border_drawer_update() -> anyhow::Result<()> {
    test_support::initialize();
    register_border_window_class()?;
    let border = WindowBorder::new(HWND::default())?;
    let hwnd = border.border_window.0;
    prepare_v2_render_backend()?;

    let mut border_drawer = BorderDrawer::default();
    let config = BorderConfig {
        width: WidthConfig::new(4.0),
        offset: OffsetConfig::new(-1),
        radius: RadiusConfig::Custom(8.0),
        ..BorderConfig::default()
    };

    border_drawer.configure_appearance(&config, 96, HWND::default());
    border_drawer.init(
        1920,
        1080,
        hwnd,
        D2D_RECT_F::default(),
        RenderBackendConfig::V2,
    )?;

    assert!(
        border_drawer
            .render(
                D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: 400.0,
                    bottom: 400.0
                },
                WindowState::default()
            )
            .is_ok()
    );
    assert!(border_drawer.resize_renderer(1280, 720).is_ok());
    assert!(
        border_drawer.render_backend.get_pixel_size()?
            == D2D_SIZE_U {
                width: 1280,
                height: 720
            }
    );
    assert!(
        border_drawer
            .render(
                D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: 400.0,
                    bottom: 400.0
                },
                WindowState::default()
            )
            .is_ok()
    );

    Ok(())
}
