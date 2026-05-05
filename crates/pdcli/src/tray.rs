use std::sync::atomic::{AtomicU8, Ordering};

use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    OpenFolder,
    ShowHideWindow,
    ToggleSyncPause,
    RetrySyncNow,
    Account,
    Settings,
    SignOut,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    SignedOut,
    Restoring,
    Online,
    Offline,
    Syncing,
    Paused,
    Error,
}

impl TrayState {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::SignedOut,
            1 => Self::Restoring,
            2 => Self::Online,
            3 => Self::Offline,
            4 => Self::Syncing,
            5 => Self::Paused,
            6 => Self::Error,
            _ => Self::Restoring,
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Self::SignedOut => "Proton Drive - signed out",
            Self::Restoring => "Proton Drive - restoring session",
            Self::Online => "Proton Drive - online",
            Self::Offline => "Proton Drive - offline",
            Self::Syncing => "Proton Drive - syncing",
            Self::Paused => "Proton Drive - sync paused",
            Self::Error => "Proton Drive - attention required",
        }
    }

    fn color(self) -> [u8; 4] {
        match self {
            Self::SignedOut => [0x88, 0x88, 0x88, 0xff],
            Self::Restoring => [0x6d, 0x4a, 0xff, 0xff],
            Self::Online => [0x28, 0xc7, 0x6f, 0xff],
            Self::Offline => [0xf0, 0x9a, 0x2a, 0xff],
            Self::Syncing => [0x3a, 0xa8, 0xff, 0xff],
            Self::Paused => [0xa0, 0xa0, 0xa0, 0xff],
            Self::Error => [0xe5, 0x42, 0x42, 0xff],
        }
    }
}

const OPEN_FOLDER_ID: &str = "pdcli.open_folder";
const SHOW_HIDE_ID: &str = "pdcli.show_hide";
const PAUSE_RESUME_ID: &str = "pdcli.pause_resume";
const RETRY_NOW_ID: &str = "pdcli.retry_now";
const ACCOUNT_ID: &str = "pdcli.account";
const SETTINGS_ID: &str = "pdcli.settings";
const SIGN_OUT_ID: &str = "pdcli.sign_out";
const QUIT_ID: &str = "pdcli.quit";

static LAST_STATE: AtomicU8 = AtomicU8::new(255);

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

fn state_icon(state: TrayState) -> Icon {
    let (w, h) = (16u32, 16u32);
    let base = state.color();
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let dx = x as i32 - 8;
            let dy = y as i32 - 8;
            if dx * dx + dy * dy <= 56 {
                rgba.extend_from_slice(&base);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    Icon::from_rgba(rgba, w, h).expect("failed to create state icon")
}

fn build_menu() -> Menu {
    let menu = Menu::new();
    let open_folder = MenuItem::with_id(
        MenuId::new(OPEN_FOLDER_ID),
        "Open ProtonDrive Folder",
        true,
        None,
    );
    let show_hide = MenuItem::with_id(MenuId::new(SHOW_HIDE_ID), "Open Window", true, None);
    let pause_resume = MenuItem::with_id(
        MenuId::new(PAUSE_RESUME_ID),
        "Pause/Resume Sync",
        true,
        None,
    );
    let retry_now = MenuItem::with_id(MenuId::new(RETRY_NOW_ID), "Retry Sync Now", true, None);
    let account = MenuItem::with_id(MenuId::new(ACCOUNT_ID), "Account", true, None);
    let settings = MenuItem::with_id(MenuId::new(SETTINGS_ID), "Settings", true, None);
    let sign_out = MenuItem::with_id(MenuId::new(SIGN_OUT_ID), "Sign Out", true, None);
    let quit = MenuItem::with_id(MenuId::new(QUIT_ID), "Quit", true, None);
    let _ = menu.append_items(&[
        &open_folder,
        &show_hide,
        &PredefinedMenuItem::separator(),
        &pause_resume,
        &retry_now,
        &PredefinedMenuItem::separator(),
        &account,
        &settings,
        &sign_out,
        &PredefinedMenuItem::separator(),
        &quit,
    ]);
    menu
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
            let tray = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip(TrayState::Restoring.tooltip())
                .with_icon(icon)
                .build()
                .unwrap();
            gtk::glib::timeout_add_seconds_local(1, move || {
                let state = TrayState::from_u8(LAST_STATE.load(Ordering::Relaxed));
                let _ = tray.set_tooltip(Some(state.tooltip()));
                let _ = tray.set_icon(Some(state_icon(state)));
                gtk::glib::ControlFlow::Continue
            });
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
                    .with_menu(Box::new(menu))
                    .with_tooltip(TrayState::Restoring.tooltip())
                    .with_icon(icon)
                    .build()
                    .expect("failed to build tray icon"),
            )
        }
    }
}

/// Call once per frame to drain tray-icon events.
pub fn poll_events() -> Vec<TrayAction> {
    use tray_icon::TrayIconEvent;

    while let Ok(event) = TrayIconEvent::receiver().try_recv() {
        tracing::debug!(?event, "tray event");
    }

    let mut actions = Vec::new();
    while let Ok(event) = MenuEvent::receiver().try_recv() {
        let action = match event.id().0.as_str() {
            OPEN_FOLDER_ID => Some(TrayAction::OpenFolder),
            SHOW_HIDE_ID => Some(TrayAction::ShowHideWindow),
            PAUSE_RESUME_ID => Some(TrayAction::ToggleSyncPause),
            RETRY_NOW_ID => Some(TrayAction::RetrySyncNow),
            ACCOUNT_ID => Some(TrayAction::Account),
            SETTINGS_ID => Some(TrayAction::Settings),
            SIGN_OUT_ID => Some(TrayAction::SignOut),
            QUIT_ID => Some(TrayAction::Quit),
            _ => None,
        };
        if let Some(action) = action {
            actions.push(action);
        }
    }
    actions
}

pub fn update_state(tray: Option<&TrayIcon>, state: TrayState) {
    if LAST_STATE.swap(state as u8, Ordering::Relaxed) == state as u8 {
        return;
    }

    if let Some(tray) = tray {
        let _ = tray.set_tooltip(Some(state.tooltip()));
        let _ = tray.set_icon(Some(state_icon(state)));
    }
}
