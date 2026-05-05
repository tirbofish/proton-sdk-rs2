use std::sync::Arc;

use poll_promise::Promise;
use proton_drive_sdk::cache::sqlite::SqliteCacheRepository;
use proton_sdk_rs2::{
    AppVersionConfiguration, cache::CacheRepository, client::ProtonClientOptions,
    session::ProtonAPISession,
};

use crate::{
    auth, credentials, daemon, flags,
    transfer::{TransferDirection, TransferTracker, format_bytes},
    tray,
};

pub(crate) enum AppState {
    Restoring(Promise<anyhow::Result<ProtonAPISession>>),
    Login(auth::AuthScreen),
    Authenticated(ProtonAPISession),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuPage {
    Status,
    Computers,
    Mount,
    About,
    Account,
    Settings,
}

pub struct ProtonDrive {
    pub(crate) flags: flags::ClientFlags,

    pub(crate) state: AppState,
    active_page: MenuPage,
    pub(crate) transfer_tracker: TransferTracker,
    _tray: Option<tray_icon::TrayIcon>,
    pub(crate) fuse_session: Option<fuser::BackgroundSession>,
    daemon_started: bool,
    window_hidden: bool,
    allow_quit: bool,
}

impl ProtonDrive {
    pub fn new() -> Self {
        let mut flags = flags::ClientFlags::default();
        flags.apply_flags();
        let force_offline = flags.force_offline;

        let state = match credentials::load() {
            Some(cred) => {
                tracing::info!("found stored credentials, restoring session");
                let task = Promise::spawn_async(async move {
                    let config_dir = platform_dirs::AppDirs::new(Some("pdcli"), false)
                        .expect("failed to resolve config directory")
                        .config_dir;
                    std::fs::create_dir_all(&config_dir).ok();
                    let cache_db_path = config_dir.join("cache.db");

                    let entity_cache: Arc<dyn CacheRepository> = Arc::new(
                        SqliteCacheRepository::open_file(&cache_db_path, Some(10_000))
                            .expect("failed to open entity cache"),
                    );
                    let secret_cache: Arc<dyn CacheRepository> = Arc::new(
                        SqliteCacheRepository::open_file(&cache_db_path, Some(5_000))
                            .expect("failed to open secret cache"),
                    );

                    let mut session = ProtonAPISession::from_stored_credentials(
                        cred,
                        AppVersionConfiguration::new("pdcli", 0, 1, 0),
                        ProtonClientOptions {
                            entity_cache_repository: Some(entity_cache),
                            secret_cache_repository: Some(secret_cache),
                            ..Default::default()
                        },
                    );
                    if !force_offline {
                        session.ensure_authenticated().await?;
                    }
                    Ok(session)
                });
                AppState::Restoring(task)
            }
            None => {
                tracing::info!("no stored credentials, showing login");
                AppState::Login(auth::AuthScreen::new())
            }
        };

        Self {
            state,
            flags,
            active_page: MenuPage::Status,
            transfer_tracker: TransferTracker::new(),
            _tray: None,
            fuse_session: None,
            daemon_started: false,
            window_hidden: false,
            allow_quit: false,
        }
    }

    pub fn with_tray(mut self, tray: Option<tray_icon::TrayIcon>) -> Self {
        self._tray = tray;
        self
    }

    pub fn handle_flags(self) -> Self {
        self
    }
}

impl eframe::App for ProtonDrive {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if ui.ctx().input(|i| i.viewport().close_requested()) && !self.allow_quit {
            self.window_hidden = true;
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }

        for action in tray::poll_events() {
            self.handle_tray_action(action, ui.ctx());
        }
        tray::update_state(self._tray.as_ref(), self.tray_state());

        egui::CentralPanel::default().show_inside(ui, |ui| match &mut self.state {
            AppState::Restoring(task) => {
                ui.vertical_centered(|ui| {
                    ui.add_space(60.0);
                    ui.spinner();
                    ui.label("Restoring session…");
                });
                ui.ctx().request_repaint();

                if let Some(result) = task.ready() {
                    match result {
                        Ok(session) => {
                            tracing::info!(user = %session.username, "session restored");
                            self.state = AppState::Authenticated(session.clone());
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "session restore failed");
                            self.state = AppState::Error(e.to_string());
                        }
                    }
                }
            }

            AppState::Login(screen) => {
                if let Some(session) = screen.ui(ui) {
                    self.state = AppState::Authenticated(session);
                }
            }

            AppState::Authenticated(_) => {
                self.ensure_daemon_running();
                self.show_authenticated(ui);
            }

            AppState::Error(error) => {
                let error = error.clone();
                ui.vertical_centered(|ui| {
                    ui.add_space(60.0);
                    ui.heading("Something went wrong");
                    ui.add_space(8.0);
                    ui.label(&error);
                    ui.add_space(16.0);
                    if ui.button("Log out").clicked() {
                        tracing::info!("user logged out from error screen");
                        credentials::remove();
                        self.state = AppState::Login(auth::AuthScreen::new());
                    }
                });
            }
        });
    }
}

impl ProtonDrive {
    fn tray_state(&self) -> tray::TrayState {
        match &self.state {
            AppState::Restoring(_) => tray::TrayState::Restoring,
            AppState::Login(_) => tray::TrayState::SignedOut,
            AppState::Error(_) => tray::TrayState::Error,
            AppState::Authenticated(_) => match daemon::status() {
                Some(daemon::DaemonStatus::Paused) => tray::TrayState::Paused,
                Some(daemon::DaemonStatus::Offline) => tray::TrayState::Offline,
                Some(daemon::DaemonStatus::Online) => {
                    if !self.transfer_tracker.snapshot().is_empty() {
                        tray::TrayState::Syncing
                    } else {
                        tray::TrayState::Online
                    }
                }
                None => tray::TrayState::Offline,
            },
        }
    }

    fn handle_tray_action(&mut self, action: tray::TrayAction, ctx: &egui::Context) {
        match action {
            tray::TrayAction::OpenFolder => {
                if let Some(path) = dirs::home_dir().map(|h| h.join("ProtonDrive").join("MyFiles"))
                {
                    #[cfg(target_os = "macos")]
                    let opener = "open";
                    #[cfg(not(target_os = "macos"))]
                    let opener = "xdg-open";
                    let _ = std::process::Command::new(opener).arg(path).spawn();
                }
            }
            tray::TrayAction::ShowHideWindow => {
                self.window_hidden = !self.window_hidden;
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(self.window_hidden));
            }
            tray::TrayAction::ToggleSyncPause => match daemon::request_toggle_pause() {
                Ok(status) => tracing::info!(?status, "sync pause toggled from tray"),
                Err(e) => tracing::warn!(error = %e, "failed to toggle daemon sync pause"),
            },
            tray::TrayAction::RetrySyncNow => match daemon::request_retry_sync_now() {
                Ok(()) => tracing::info!("manual sync retry requested from tray"),
                Err(e) => tracing::warn!(error = %e, "failed to request daemon sync retry"),
            },
            tray::TrayAction::Account => {
                self.active_page = MenuPage::Account;
                self.window_hidden = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            }
            tray::TrayAction::SignOut => {
                daemon::request_quit().ok();
                self.daemon_started = false;
                credentials::remove();
                self.state = AppState::Login(auth::AuthScreen::new());
            }
            tray::TrayAction::Quit => {
                daemon::request_quit().ok();
                self.allow_quit = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn ensure_daemon_running(&mut self) {
        if self.daemon_started {
            return;
        }

        self.daemon_started = true;
        match daemon::ensure_running(self.flags.force_offline) {
            Ok(()) => {}
            Err(e) => tracing::warn!(error = %e, "failed to start pdcli daemon"),
        }
    }

    pub fn show_authenticated(&mut self, ui: &mut egui::Ui) {
        let username = if let AppState::Authenticated(s) = &self.state {
            s.username.clone()
        } else {
            return;
        };

        let active_page = &mut self.active_page;

        egui::Panel::left("sidebar")
            .default_size(ui.available_width() * 0.25)
            .show_inside(ui, |ui| {
                ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                    Self::sidebar_button(ui, active_page, "Status", MenuPage::Status);
                    Self::sidebar_button(ui, active_page, "Computers", MenuPage::Computers);
                    Self::sidebar_button(ui, active_page, "Mount", MenuPage::Mount);
                    Self::sidebar_button(ui, active_page, "About", MenuPage::About);
                });

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.add_space(8.0);
                    if ui.button("Quit").clicked() {
                        daemon::request_quit().ok();
                        self.allow_quit = true;
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Self::sidebar_button(ui, active_page, "Settings", MenuPage::Settings);
                    Self::sidebar_button(ui, active_page, "Account", MenuPage::Account);
                    ui.separator();
                });
            });

        let page = self.active_page;
        let mut sign_out = false;

        egui::CentralPanel::default().show_inside(ui, |ui| match page {
            MenuPage::Status => {
                ui.heading("Status");
                ui.separator();
                ui.horizontal(|ui| {
                    let daemon_status = daemon::status();
                    let state_text = match daemon_status {
                        Some(daemon::DaemonStatus::Paused) => "Sync paused",
                        Some(daemon::DaemonStatus::Online) => "Online",
                        Some(daemon::DaemonStatus::Offline) | None => "Offline",
                    };
                    ui.label(state_text);
                    if ui
                        .button(if daemon_status == Some(daemon::DaemonStatus::Paused) {
                            "Resume sync"
                        } else {
                            "Pause sync"
                        })
                        .clicked()
                    {
                        daemon::request_toggle_pause().ok();
                    }
                    if ui.button("Retry now").clicked() {
                        daemon::request_retry_sync_now().ok();
                    }
                });
                ui.add_space(8.0);

                let events = daemon::recent_events();
                if !events.is_empty() {
                    ui.heading("Recent events");
                    egui::ScrollArea::vertical()
                        .max_height(140.0)
                        .show(ui, |ui| {
                            for event in events {
                                let time = chrono::DateTime::from_timestamp(event.created_at, 0)
                                    .map(|t| t.format("%H:%M:%S").to_string())
                                    .unwrap_or_else(|| "--:--:--".to_string());
                                let name = event
                                    .name
                                    .or(event.detail)
                                    .unwrap_or_else(|| "Unknown item".into());
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(time)
                                            .small()
                                            .color(ui.visuals().weak_text_color()),
                                    );
                                    ui.label(
                                        egui::RichText::new(event.source)
                                            .small()
                                            .color(ui.visuals().weak_text_color()),
                                    );
                                    ui.label(format!("{} {}", event.event_type, name));
                                });
                            }
                        });
                    ui.add_space(8.0);
                }

                let transfers = self.transfer_tracker.snapshot();
                if transfers.is_empty() {
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("No active transfers")
                                .color(ui.visuals().weak_text_color()),
                        );
                    });
                } else {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for entry in &transfers {
                            let pct = (entry.progress_fraction() * 100.0) as u32;

                            egui::Frame::group(ui.style())
                                .inner_margin(8.0)
                                .outer_margin(2.0)
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());

                                    ui.horizontal(|ui| {
                                        let (icon, color) = match entry.direction {
                                            TransferDirection::Upload => {
                                                ("⬆", egui::Color32::from_rgb(100, 180, 255))
                                            }
                                            TransferDirection::Download => {
                                                ("⬇", egui::Color32::from_rgb(100, 220, 130))
                                            }
                                        };
                                        ui.label(egui::RichText::new(icon).color(color).strong());
                                        ui.label(
                                            egui::RichText::new(&entry.filename)
                                                .strong()
                                                .text_style(egui::TextStyle::Body),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                ui.label(
                                                    egui::RichText::new(format!("{}%", pct))
                                                        .color(ui.visuals().weak_text_color())
                                                        .small(),
                                                );
                                            },
                                        );
                                    });

                                    ui.add(
                                        egui::ProgressBar::new(entry.progress_fraction())
                                            .desired_height(6.0),
                                    );

                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{} / {}",
                                                format_bytes(entry.bytes_transferred),
                                                format_bytes(entry.total_bytes),
                                            ))
                                            .small()
                                            .color(ui.visuals().weak_text_color()),
                                        );
                                    });
                                });
                        }
                    });
                    ui.ctx().request_repaint();
                }
            }
            MenuPage::Computers => {
                ui.heading("Computers");
                ui.label("No computers linked yet.");
            }
            MenuPage::Mount => {
                ui.heading("Mount");
                ui.separator();
                if daemon::is_running() {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("●").color(egui::Color32::from_rgb(100, 220, 130)),
                        );
                        ui.label("FUSE filesystem mounted");
                    });
                    ui.add_space(8.0);
                    let mount_path = dirs::home_dir()
                        .map(|h| h.join("ProtonDrive").join("MyFiles").display().to_string())
                        .unwrap_or_else(|| "~/ProtonDrive/MyFiles".into());
                    ui.label(format!("Mount point: {}", mount_path));
                } else {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("●").color(egui::Color32::from_rgb(200, 80, 80)),
                        );
                        ui.label("FUSE filesystem not mounted");
                    });
                }
            }
            MenuPage::About => {
                ui.heading("About");
                ui.label(format!(
                    "pdcli {} - Proton Drive for Linux",
                    env!("CARGO_PKG_VERSION")
                ));
            }
            MenuPage::Account => {
                ui.heading("Account");
                ui.label(format!("Username: {}", username));
                ui.add_space(16.0);
                if ui.button("Sign out").clicked() {
                    sign_out = true;
                }
            }
            MenuPage::Settings => {
                ui.heading("Settings");
                ui.label("Settings will go here.");
            }
        });

        if sign_out {
            tracing::info!(user = %username, "user signed out");
            daemon::request_quit().ok();
            self.daemon_started = false;
            credentials::remove();
            self.state = AppState::Login(auth::AuthScreen::new());
        }
    }

    fn sidebar_button(ui: &mut egui::Ui, active_page: &mut MenuPage, label: &str, page: MenuPage) {
        let selected = *active_page == page;
        if ui.selectable_label(selected, label).clicked() {
            *active_page = page;
        }
    }
}
