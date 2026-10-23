use anyhow::Context;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use windows::Win32::UI::Accessibility::HWINEVENTHOOK;
use windows::Win32::UI::WindowsAndMessaging::PostQuitMessage;

use crate::auto_start::{is_autostart_enabled, toggle_autostart};
use crate::config::Config;
use crate::utils::LogIfErr;
use crate::{APP_STATE, HostAction, submit_host_action};

pub fn create_tray_icon(_hwineventhook: HWINEVENTHOOK) -> anyhow::Result<TrayIcon> {
    let icon = match Icon::from_resource(1, Some((64, 64))) {
        Ok(icon) => icon,
        Err(err) => {
            error!("could not retrieve icon from tacky-borders.exe for tray menu: {err:#}");

            // If we could not retrieve an icon from the exe, then try to create an empty icon. If
            // even that fails, then we'll just return an Error.
            let rgba: Vec<u8> = vec![0, 0, 0, 0];
            Icon::from_rgba(rgba, 1, 1).context("could not create empty tray icon")?
        }
    };

    let auto_enabled = is_autostart_enabled()?;

    let tray_menu = Menu::new();
    tray_menu.append_items(&[
        &MenuItem::with_id("0", "Show Config", true, None),
        &CheckMenuItem::with_id("1", "Auto Start", true, auto_enabled, None),
        &MenuItem::with_id("2", "Reload", true, None),
        &MenuItem::with_id("3", "Close", true, None),
    ])?;

    let tooltip = format!("{}{}", "tacky-borders v", env!("CARGO_PKG_VERSION"));

    let tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip(tooltip)
        .with_icon(icon)
        .build();

    // Handle tray icon events (i.e. clicking on the menu items)
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| match event.id.0.as_str() {
        // Show Config
        "0" => match Config::get_dir() {
            Ok(dir) => {
                open::that(dir).log_if_err();
            }
            Err(err) => error!("{err:#}"),
        },
        // Auto Start
        "1" => {
            if let Err(err) = toggle_autostart() {
                error!("{err:#}")
            }
        }
        // Reload
        "2" => {
            submit_host_action(HostAction::ReloadConfig {
                origin: crate::ReloadOrigin::Tray,
                response: None,
                context: None,
            })
            .log_if_err();
        }
        // Close
        "3" => {
            if let Ok(owner) = APP_STATE.lifecycle() {
                owner.request_shutdown();
            }
            unsafe { PostQuitMessage(0) };
        }
        _ => {}
    }));

    tray_icon.map_err(anyhow::Error::new)
}
