use tray_icon::{TrayIconBuilder, TrayIcon, Icon, menu::{Menu, MenuItem}};

/// Creates a [`tray_icon::Icon`] from an RGBA pixel buffer.
///
/// If `path` points to a valid PNG file, it is loaded and converted;
/// otherwise a tiny 16×16 blue fallback icon is generated.
fn load_icon(path: &std::path::Path) -> Icon {
    if let Ok(img) = image::open(path) {
        let rgba = img.into_rgba8();
        let (w, h) = rgba.dimensions();
        return Icon::from_rgba(rgba.into_raw(), w, h).expect("failed to create icon from image");
    }

    // 16×16 solid blue fallback
    let (w, h) = (16u32, 16u32);
    let rgba = vec![0x6D, 0x4A, 0xFF, 0xFF].repeat((w * h) as usize);
    Icon::from_rgba(rgba, w, h).expect("failed to create fallback icon")
}

/// The ID we assign to the "Quit" menu item so we can recognise it in events.
const QUIT_MENU_ID: &str = "quit";
const SHOW_MENU_ID: &str = "show";

/// Actions returned by polling tray events.
pub struct TrayAction {
    pub quit: bool,
    pub show: bool,
}

/// Initialise the system-tray icon.
///
/// On Linux/BSD the tray is created on a dedicated GTK thread (required by
/// `libappindicator`).  On other platforms the returned [`TrayIcon`] handle
/// must be kept alive for the icon to remain visible; it is created lazily
/// inside the eframe creation callback so that a winit event-loop is
/// already running.
///
/// Returns a closure that **must** be called from inside the
/// `eframe::run_native` app-creation callback on non-Linux platforms.
pub fn init(icon_path: &std::path::Path) -> impl FnOnce() -> Option<TrayIcon> {
    let icon = load_icon(icon_path);

    // On Linux / BSD: spawn a gtk thread immediately and return a no-op
    // closure because the tray icon lives on that thread.
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    {
        std::thread::spawn(move || {
            gtk::init().unwrap();
            let menu = build_menu();
            let _tray = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("Proton Drive")
                .with_icon(icon)
                .build()
                .unwrap();
            gtk::main();
        });
        || None
    }

    // On Windows / macOS: return a closure that creates the icon once the
    // event-loop is available.
    #[cfg(not(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    {
        move || {
            let menu = build_menu();
            Some(
                TrayIconBuilder::new()
                    .with_tooltip("Proton Drive")
                    .with_icon(icon)
                    .with_menu(Box::new(menu))
                    .build()
                    .expect("failed to build tray icon"),
            )
        }
    }
}

fn build_menu() -> Menu {
    let menu = Menu::new();
    let show_item = MenuItem::with_id(SHOW_MENU_ID, "Show", true, None);
    let quit_item = MenuItem::with_id(QUIT_MENU_ID, "Quit", true, None);
    menu.append(&show_item).ok();
    menu.append(&quit_item).ok();
    menu
}

/// Call once per frame to drain tray-icon and menu events.
pub fn poll_events() -> TrayAction {
    use tray_icon::TrayIconEvent;
    use tray_icon::menu::MenuEvent;

    let mut action = TrayAction { quit: false, show: false };

    while let Ok(event) = TrayIconEvent::receiver().try_recv() {
        tracing::debug!(?event, "tray event");
    }

    while let Ok(event) = MenuEvent::receiver().try_recv() {
        if event.id.0 == QUIT_MENU_ID {
            action.quit = true;
        } else if event.id.0 == SHOW_MENU_ID {
            action.show = true;
        }
    }

    action
}
