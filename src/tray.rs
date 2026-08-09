//! System-tray integration. Runs an OS-native event loop on the calling
//! thread (which MUST be the process's main thread on Windows and macOS),
//! shows a small antenna icon in the tray/menu-bar, and exposes a tiny
//! menu: "Open in browser" and "Quit propmonitor".
//!
//! If the OS doesn't have a tray (headless Linux, SSH session, no GUI
//! libs), [`run`] returns an error and the caller is expected to fall
//! back to a headless wait — the server keeps running fine without a tray.

use tao::event_loop::{ControlFlow, EventLoop};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder};

use crate::error::{Error, Result};

/// Build a tray icon + menu and run the OS event loop. Diverges: this
/// function only ever returns on `Quit` (via `process::exit`) or on a
/// tray-init failure.
pub fn run(open_url: String) -> Result<()> {
    let icon = make_antenna_icon();

    let menu = Menu::new();
    let open_item = MenuItem::new("Open in browser", true, None);
    let quit_item = MenuItem::new("Quit propmonitor", true, None);
    menu.append(&open_item).map_err(menu_err)?;
    menu.append(&PredefinedMenuItem::separator())
        .map_err(menu_err)?;
    menu.append(&quit_item).map_err(menu_err)?;

    let open_id = open_item.id().clone();
    let quit_id = quit_item.id().clone();

    let event_loop = EventLoop::new();

    let _tray = TrayIconBuilder::new()
        .with_tooltip(format!("propmonitor — {}", open_url))
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .build()
        .map_err(|e| Error::msg(format!("tray init: {}", e)))?;

    let menu_channel = MenuEvent::receiver();
    let url = open_url;

    // The tray-icon crate delivers menu events through its own crossbeam
    // channel, not through tao's event stream. So we wake the event loop
    // every 100 ms to drain it. (100 ms feels instant for menu clicks
    // while keeping CPU at sleep otherwise.)
    event_loop.run(move |_event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(100),
        );
        while let Ok(event) = menu_channel.try_recv() {
            if event.id == open_id {
                let _ = webbrowser::open(&url);
            } else if event.id == quit_id {
                std::process::exit(0);
            }
        }
    });
}

fn menu_err(e: tray_icon::menu::Error) -> Error {
    Error::msg(format!("menu build: {}", e))
}

/// Hand-rendered 32×32 antenna icon. RGBA bytes, no PNG decoder needed.
///
/// The shape: a vertical mast, an X-shaped flare at the top suggesting an
/// antenna tip, three small "signal" notches at the upper sides, and a
/// triangular base — all in a cool blue against transparent background.
fn make_antenna_icon() -> Icon {
    const W: usize = 32;
    const H: usize = 32;
    let mut rgba = vec![0u8; W * H * 4];
    let fg: [u8; 4] = [180, 230, 255, 255];
    let dim: [u8; 4] = [120, 180, 220, 255];

    fn set(buf: &mut [u8], x: i32, y: i32, c: [u8; 4]) {
        if (0..W as i32).contains(&x) && (0..H as i32).contains(&y) {
            let i = ((y as usize) * W + x as usize) * 4;
            buf[i] = c[0];
            buf[i + 1] = c[1];
            buf[i + 2] = c[2];
            buf[i + 3] = c[3];
        }
    }

    // Mast (2px wide for crispness on the tray).
    for y in 6..26 {
        set(&mut rgba, 15, y, fg);
        set(&mut rgba, 16, y, fg);
    }

    // Antenna tip — small X flare above the mast.
    set(&mut rgba, 14, 5, dim);
    set(&mut rgba, 17, 5, dim);
    set(&mut rgba, 13, 4, dim);
    set(&mut rgba, 18, 4, dim);
    set(&mut rgba, 15, 5, fg);
    set(&mut rgba, 16, 5, fg);

    // Cross arm near the top.
    for x in 11..22 {
        set(&mut rgba, x, 9, fg);
    }

    // Three "signal" notches on each side of the mast, fanning up.
    for r in 0..3i32 {
        let off_x = 3 + r * 3;
        let off_y = r;
        set(&mut rgba, 15 - off_x, 9 + off_y, dim);
        set(&mut rgba, 15 - off_x, 10 + off_y, dim);
        set(&mut rgba, 16 + off_x, 9 + off_y, dim);
        set(&mut rgba, 16 + off_x, 10 + off_y, dim);
    }

    // V-shaped base spreading from the foot of the mast.
    for d in 0..5i32 {
        set(&mut rgba, 15 - d, 26 + d, dim);
        set(&mut rgba, 16 + d, 26 + d, dim);
    }
    // Ground line.
    for x in 10..22 {
        set(&mut rgba, x, 30, fg);
    }

    Icon::from_rgba(rgba, W as u32, H as u32).expect("32x32 RGBA -> Icon")
}
