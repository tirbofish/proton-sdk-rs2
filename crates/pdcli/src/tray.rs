use std::path::Path;

use ksni::{
    Category, OfflineReason, Status, ToolTip, Tray, TrayMethods,
    menu::{MenuItem, StandardItem},
};
use tokio::sync::mpsc;

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
    Restoring,
    Online,
    Offline,
    Paused,
}

impl TrayState {
    fn tooltip(self) -> &'static str {
        match self {
            Self::Restoring => "Proton Drive - restoring session",
            Self::Online => "Proton Drive - online",
            Self::Offline => "Proton Drive - offline",
            Self::Paused => "Proton Drive - sync paused",
        }
    }

    fn color(self) -> [u8; 4] {
        match self {
            Self::Restoring => [0x6d, 0x4a, 0xff, 0xff],
            Self::Online => [0x28, 0xc7, 0x6f, 0xff],
            Self::Offline => [0xf0, 0x9a, 0x2a, 0xff],
            Self::Paused => [0xa0, 0xa0, 0xa0, 0xff],
        }
    }
}

pub struct TrayHandle {
    handle: ksni::Handle<ProtonDriveTray>,
}

impl TrayHandle {
    async fn set_state(&self, state: TrayState) {
        if self.handle.is_closed() {
            return;
        }

        let _ = self
            .handle
            .update(|tray| {
                let changed = tray.state != state;
                tray.state = state;
                changed
            })
            .await;
    }

    pub async fn shutdown(self) {
        self.handle.shutdown().await;
    }
}

struct ProtonDriveTray {
    state: TrayState,
    notifier: mpsc::UnboundedSender<TrayAction>,
}

impl ProtonDriveTray {
    fn notify(&self, action: TrayAction) {
        if self.notifier.send(action).is_err() {
            tracing::debug!(?action, "tray action receiver is no longer available");
        }
    }
}

impl Tray for ProtonDriveTray {
    fn id(&self) -> String {
        "pdcli".into()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.notify(TrayAction::ShowHideWindow);
    }

    fn category(&self) -> Category {
        Category::ApplicationStatus
    }

    fn title(&self) -> String {
        "Proton Drive".into()
    }

    fn status(&self) -> Status {
        Status::Active
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![state_icon(self.state)]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "Proton Drive".into(),
            description: self.state.tooltip().into(),
            icon_pixmap: self.icon_pixmap(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            menu_item("Open ProtonDrive Folder", TrayAction::OpenFolder),
            menu_item("Open Window", TrayAction::ShowHideWindow),
            MenuItem::Separator,
            menu_item(
                match self.state {
                    TrayState::Paused => "Resume Sync",
                    _ => "Pause Sync",
                },
                TrayAction::ToggleSyncPause,
            ),
            menu_item("Retry Sync Now", TrayAction::RetrySyncNow),
            MenuItem::Separator,
            menu_item("Account", TrayAction::Account),
            menu_item("Settings", TrayAction::Settings),
            menu_item("Sign Out", TrayAction::SignOut),
            MenuItem::Separator,
            menu_item("Quit", TrayAction::Quit),
        ]
    }

    fn watcher_online(&self) {
        tracing::info!("StatusNotifierWatcher is online; tray icon should be visible");
    }

    fn watcher_offline(&self, reason: OfflineReason) -> bool {
        tracing::warn!(
            ?reason,
            "StatusNotifierWatcher is offline; keeping tray service alive"
        );
        true
    }
}

fn menu_item(label: &str, action: TrayAction) -> MenuItem<ProtonDriveTray> {
    StandardItem {
        label: label.into(),
        activate: Box::new(move |tray: &mut ProtonDriveTray| {
            tray.notify(action);
        }),
        ..Default::default()
    }
    .into()
}

fn state_icon(state: TrayState) -> ksni::Icon {
    let (width, height) = (16_i32, 16_i32);
    let [r, g, b, a] = state.color();
    let mut data = Vec::with_capacity((width * height * 4) as usize);

    for y in 0..height {
        for x in 0..width {
            let dx = x - 8;
            let dy = y - 8;

            if dx * dx + dy * dy <= 56 {
                // ksni expects ARGB32, network byte order.
                data.extend_from_slice(&[a, r, g, b]);
            } else {
                data.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }

    ksni::Icon {
        width,
        height,
        data,
    }
}

/// Initialise the system-tray icon using the freedesktop/KDE
/// StatusNotifierItem protocol over D-Bus.
///
/// The current fallback icon is state-generated, so `icon_path` is kept only to
/// preserve the old caller API and to make it easy to wire a branded PNG back
/// in later.
pub async fn init(
    _icon_path: &Path,
) -> anyhow::Result<(TrayHandle, mpsc::UnboundedReceiver<TrayAction>)> {
    let (notifier, receiver) = mpsc::unbounded_channel();
    let tray = ProtonDriveTray {
        state: TrayState::Restoring,
        notifier,
    };

    let handle = tray
        .assume_sni_available(true)
        .spawn()
        .await
        .map_err(|e| anyhow::anyhow!("failed to start StatusNotifierItem tray: {e}"))?;

    Ok((TrayHandle { handle }, receiver))
}

pub async fn update_state(tray: &TrayHandle, state: TrayState) {
    tray.set_state(state).await;
}
