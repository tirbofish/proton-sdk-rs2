use std::f32::consts::TAU;
use std::sync::{Mutex, OnceLock};

use image::RgbaImage;
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

use crate::transfer::{TransferDirection, TransferEntry, format_bytes};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    OpenFolder,
    ShowHideWindow,
    ShowTransfers,
    ToggleSyncPause,
    RetrySyncNow,
    Account,
    Settings,
    SignOut,
    Quit,
}

// The daemon does not report signed-out or error states yet.
#[allow(dead_code)]
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
const TRANSFERS_ID: &str = "pdcli.transfers";
const PAUSE_RESUME_ID: &str = "pdcli.pause_resume";
const RETRY_NOW_ID: &str = "pdcli.retry_now";
const ACCOUNT_ID: &str = "pdcli.account";
const SETTINGS_ID: &str = "pdcli.settings";
const SIGN_OUT_ID: &str = "pdcli.sign_out";
const QUIT_ID: &str = "pdcli.quit";

/// Transfers listed in the menu; the rest are counted in the summary line.
const MENU_TRANSFER_ROWS: usize = 8;
const ICON_SIZE: u32 = 64;
const LOGO_SIZE: u32 = 46;
const RING_OUTER: f32 = 31.5;
const RING_WIDTH: f32 = 5.0;
/// Arc positions per turn, so the icon is only re-sent when the ring visibly moves.
const RING_STEPS: f32 = 64.0;

/// What the tray shows, written by the daemon (or `pdcli tray`) and drawn by
/// the tray's own GTK thread.
#[derive(Debug, Clone, PartialEq)]
struct TrayView {
    state: TrayState,
    transfers: Vec<TransferEntry>,
}

static VIEW: Mutex<Option<TrayView>> = Mutex::new(None);

/// Overall progress of the active transfers: `None` when idle, `Some(None)`
/// when no transfer knows its size yet.
fn overall_progress(transfers: &[TransferEntry]) -> Option<Option<f32>> {
    if transfers.is_empty() {
        return None;
    }
    let (done, total) = transfers.iter().filter(|entry| entry.total_bytes > 0).fold(
        (0i64, 0i64),
        |(done, total), entry| {
            (
                done + entry.bytes_transferred.clamp(0, entry.total_bytes),
                total + entry.total_bytes,
            )
        },
    );
    Some((total > 0).then(|| done as f32 / total as f32))
}

fn logo() -> &'static RgbaImage {
    static LOGO: OnceLock<RgbaImage> = OnceLock::new();
    LOGO.get_or_init(|| {
        let image = image::load_from_memory(include_bytes!("../icon.png"))
            .map(|image| image.into_rgba8())
            .unwrap_or_else(|_| RgbaImage::from_pixel(1, 1, image::Rgba([0x6d, 0x4a, 0xff, 0xff])));
        image::imageops::resize(
            &image,
            LOGO_SIZE,
            LOGO_SIZE,
            image::imageops::FilterType::Lanczos3,
        )
    })
}

/// The app logo inside a ring. Idle, the ring is the connection-state colour;
/// during transfers it fills clockwise from the top with overall progress, and
/// a short arc spins while sizes are unknown. `spin` is the animation step.
fn render_icon(state: TrayState, progress: Option<Option<f32>>, spin: u32) -> RgbaImage {
    let mut icon = RgbaImage::new(ICON_SIZE, ICON_SIZE);
    let offset = (ICON_SIZE - LOGO_SIZE) / 2;
    image::imageops::overlay(&mut icon, logo(), offset.into(), offset.into());

    let track = [0x80, 0x80, 0x80, 0x70];
    let active = TrayState::Syncing.color();
    let (arc_start, arc_end) = match progress {
        None => (0.0, 1.0),
        Some(Some(fraction)) => (0.0, (fraction * RING_STEPS).floor() / RING_STEPS),
        Some(None) => {
            let start = (spin % 16) as f32 / 16.0;
            (start, start + 0.25)
        }
    };
    let arc_color = if progress.is_none() {
        state.color()
    } else {
        active
    };
    let center = ICON_SIZE as f32 / 2.0;
    for (x, y, pixel) in icon.enumerate_pixels_mut() {
        let dx = x as f32 + 0.5 - center;
        let dy = y as f32 + 0.5 - center;
        let distance = (dx * dx + dy * dy).sqrt();
        // Anti-aliased band: full coverage inside, fading over one pixel at each edge.
        let coverage = (RING_OUTER - distance)
            .min(distance - (RING_OUTER - RING_WIDTH))
            .clamp(-0.5, 0.5)
            + 0.5;
        if coverage <= 0.0 {
            continue;
        }
        // Clockwise turns from 12 o'clock.
        let turn = (dx.atan2(-dy) / TAU).rem_euclid(1.0);
        let in_arc =
            (arc_start..arc_end).contains(&turn) || (arc_start..arc_end).contains(&(turn + 1.0));
        let color = if in_arc { arc_color } else { track };
        let alpha = color[3] as f32 / 255.0 * coverage;
        let below = pixel.0;
        let blend =
            |top: u8, bottom: u8| (top as f32 * alpha + bottom as f32 * (1.0 - alpha)) as u8;
        *pixel = image::Rgba([
            blend(color[0], below[0]),
            blend(color[1], below[1]),
            blend(color[2], below[2]),
            ((alpha + below[3] as f32 / 255.0 * (1.0 - alpha)) * 255.0) as u8,
        ]);
    }
    icon
}

fn to_icon(image: RgbaImage) -> Icon {
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).expect("icon buffer matches its size")
}

fn truncated(name: &str, limit: usize) -> String {
    if name.chars().count() <= limit {
        return name.to_owned();
    }
    let mut short: String = name.chars().take(limit - 1).collect();
    short.push('…');
    short
}

fn transfer_label(entry: &TransferEntry) -> String {
    let arrow = match entry.direction {
        TransferDirection::Download => '↓',
        TransferDirection::Upload => '↑',
    };
    let name = truncated(&entry.filename, 40);
    if entry.total_bytes > 0 {
        format!(
            "{arrow} {name} — {:.0}% ({} of {})",
            entry.progress_fraction().clamp(0.0, 1.0) * 100.0,
            format_bytes(entry.bytes_transferred),
            format_bytes(entry.total_bytes)
        )
    } else {
        format!("{arrow} {name} — {}", format_bytes(entry.bytes_transferred))
    }
}

fn transfers_summary(transfers: &[TransferEntry]) -> String {
    let count = |direction| {
        transfers
            .iter()
            .filter(|entry| entry.direction == direction)
            .count()
    };
    let parts: Vec<String> = [
        (count(TransferDirection::Download), "downloading"),
        (count(TransferDirection::Upload), "uploading"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, verb)| format!("{count} {verb}"))
    .collect();
    if parts.is_empty() {
        return "No active transfers".into();
    }
    let mut summary = parts.join(", ");
    if let Some(Some(fraction)) = overall_progress(transfers) {
        summary.push_str(&format!(" — {:.0}%", fraction * 100.0));
    }
    summary
}

fn tooltip(view: &TrayView) -> String {
    if view.transfers.is_empty() {
        view.state.tooltip().into()
    } else {
        format!(
            "{}\n{}",
            view.state.tooltip(),
            transfers_summary(&view.transfers)
        )
    }
}

/// Menu items that change while the tray runs. They are GTK-backed, so they
/// live on the tray thread.
struct TrayMenu {
    menu: Menu,
    summary: MenuItem,
    rows: Vec<MenuItem>,
}

impl TrayMenu {
    fn new() -> Self {
        let menu = Menu::new();
        let summary =
            MenuItem::with_id(MenuId::new(TRANSFERS_ID), "No active transfers", true, None);
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
            &summary,
            &PredefinedMenuItem::separator(),
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
        Self {
            menu,
            summary,
            rows: Vec::new(),
        }
    }

    /// Rows are added or removed only when the count changes; otherwise their
    /// text is updated in place so an open menu does not flicker or close.
    fn show(&mut self, transfers: &[TransferEntry]) {
        let shown = &transfers[..transfers.len().min(MENU_TRANSFER_ROWS)];
        while self.rows.len() > shown.len() {
            let row = self.rows.pop().expect("checked non-empty");
            let _ = self.menu.remove(&row);
        }
        while self.rows.len() < shown.len() {
            let id = format!("{TRANSFERS_ID}.{}", self.rows.len());
            let row = MenuItem::with_id(MenuId::new(id), "", true, None);
            // Below the summary line, in transfer order.
            let _ = self.menu.insert(&row, 1 + self.rows.len());
            self.rows.push(row);
        }
        for (row, entry) in self.rows.iter().zip(shown) {
            row.set_text(transfer_label(entry));
        }
        let mut summary = transfers_summary(transfers);
        if transfers.len() > shown.len() {
            summary.push_str(&format!(" (showing {})", shown.len()));
        }
        if !transfers.is_empty() {
            summary.push_str(" — Show All…");
        }
        self.summary.set_text(summary);
    }
}

/// Initialise the system-tray icon.
///
/// On Linux/BSD the tray is created on a dedicated GTK thread (required by
/// `libappindicator`), isolated from the GTK4 desktop process. On other
/// platforms the returned [`TrayIcon`] handle must be kept alive.
pub fn init(_icon_path: &std::path::Path) -> impl FnOnce() -> Option<TrayIcon> {
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
            if let Err(e) = gtk::init() {
                tracing::warn!(error = %e, "gtk init failed, tray disabled");
                return;
            }
            let mut menu = TrayMenu::new();
            let tray = match TrayIconBuilder::new()
                .with_menu(Box::new(menu.menu.clone()))
                .with_tooltip(TrayState::Restoring.tooltip())
                .with_icon(to_icon(render_icon(TrayState::Restoring, None, 0)))
                .build()
            {
                Ok(tray) => tray,
                Err(e) => {
                    tracing::warn!(error = %e, "tray icon failed, continuing without tray");
                    return;
                }
            };
            let mut shown: Option<TrayView> = None;
            let mut drawn_icon: Option<RgbaImage> = None;
            let mut spin = 0u32;
            gtk::glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
                let Some(view) = VIEW.lock().unwrap().clone() else {
                    return gtk::glib::ControlFlow::Continue;
                };
                let progress = overall_progress(&view.transfers);
                if progress == Some(None) {
                    spin = spin.wrapping_add(1);
                }
                let icon = render_icon(view.state, progress, spin);
                if drawn_icon.as_ref() != Some(&icon) {
                    let _ = tray.set_icon(Some(to_icon(icon.clone())));
                    drawn_icon = Some(icon);
                }
                if shown.as_ref() != Some(&view) {
                    let _ = tray.set_tooltip(Some(tooltip(&view)));
                    menu.show(&view.transfers);
                    shown = Some(view);
                }
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
            let menu = TrayMenu::new();
            Some(
                TrayIconBuilder::new()
                    .with_menu(Box::new(menu.menu))
                    .with_tooltip(TrayState::Restoring.tooltip())
                    .with_icon(to_icon(render_icon(TrayState::Restoring, None, 0)))
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
            id if id.starts_with(TRANSFERS_ID) => Some(TrayAction::ShowTransfers),
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

/// Publishes the connection state and active transfers to the tray. On Linux
/// the tray thread picks them up; elsewhere `tray` is updated directly.
pub fn update(tray: Option<&TrayIcon>, state: TrayState, transfers: Vec<TransferEntry>) {
    let view = TrayView { state, transfers };
    let mut current = VIEW.lock().unwrap();
    if current.as_ref() == Some(&view) {
        return;
    }
    if let Some(tray) = tray {
        let _ = tray.set_tooltip(Some(tooltip(&view)));
        let progress = overall_progress(&view.transfers);
        let _ = tray.set_icon(Some(to_icon(render_icon(state, progress, 0))));
    }
    *current = Some(view);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(direction: TransferDirection, done: i64, total: i64) -> TransferEntry {
        TransferEntry {
            id: 0,
            filename: "clip.mp4".into(),
            direction,
            bytes_transferred: done,
            total_bytes: total,
            cancellable: false,
        }
    }

    #[test]
    fn overall_progress_weights_by_size_and_reports_unknown_sizes() {
        assert_eq!(overall_progress(&[]), None);
        assert_eq!(
            overall_progress(&[
                entry(TransferDirection::Download, 50, 100),
                entry(TransferDirection::Upload, 300, 300),
            ]),
            Some(Some(0.875))
        );
        assert_eq!(
            overall_progress(&[entry(TransferDirection::Download, 10, 0)]),
            Some(None)
        );
    }

    #[test]
    fn ring_fills_clockwise_from_the_top() {
        let half = render_icon(TrayState::Online, Some(Some(0.5)), 0);
        let active = TrayState::Syncing.color();
        // Right side of the ring (3 o'clock) is inside the first half; the left is not.
        assert_eq!(half.get_pixel(61, 32).0[..3], active[..3]);
        assert_ne!(half.get_pixel(2, 32).0[..3], active[..3]);
    }

    #[test]
    fn summary_counts_directions_and_menu_labels_show_progress() {
        let transfers = [
            entry(TransferDirection::Download, 1024, 2048),
            entry(TransferDirection::Upload, 0, 0),
        ];
        assert_eq!(
            transfers_summary(&transfers),
            "1 downloading, 1 uploading — 50%"
        );
        assert_eq!(
            transfer_label(&transfers[0]),
            "↓ clip.mp4 — 50% (1.0 KB of 2.0 KB)"
        );
    }
}
