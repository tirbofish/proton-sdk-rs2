use poll_promise::Promise;
use proton_sdk_rs2::{
    AppVersionConfiguration, client::ProtonClientOptions, session::ProtonAPISession,
};

use crate::{
    auth, credentials, daemon, flags, pdignore,
    transfer::{TransferDirection, TransferTracker, format_bytes},
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
    pub(crate) fuse_session: Option<fuser::BackgroundSession>,
    daemon_started: bool,
    daemon_error: Option<String>,
    pdignore_text: String,
    pdignore_status: Option<String>,
}

impl ProtonDrive {
    pub fn new(flags: flags::ClientFlags) -> Self {
        let force_offline = flags.force_offline;

        let state = match credentials::load() {
            Some(cred) => {
                tracing::info!("found stored credentials, restoring session");
                let task = Promise::spawn_async(async move {
                    let (entity_cache, secret_cache) = credentials::open_session_caches()?;

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
                    if let Ok(cred) = session.to_stored_credentials_with_latest_tokens().await {
                        crate::credentials::save(&cred).ok();
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

        let active_page = flags
            .page
            .as_deref()
            .and_then(MenuPage::from_flag)
            .unwrap_or(MenuPage::Status);

        Self {
            state,
            flags,
            active_page,
            transfer_tracker: TransferTracker::new(),
            fuse_session: None,
            daemon_started: false,
            daemon_error: None,
            pdignore_text: pdignore::load_global_text(),
            pdignore_status: None,
        }
    }
}

impl eframe::App for ProtonDrive {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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
                            credentials::save_session_tokens_on_refresh(session);
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

impl MenuPage {
    fn from_flag(value: &str) -> Option<Self> {
        match value {
            "status" => Some(Self::Status),
            "computers" => Some(Self::Computers),
            "mount" => Some(Self::Mount),
            "about" => Some(Self::About),
            "account" => Some(Self::Account),
            "settings" => Some(Self::Settings),
            _ => None,
        }
    }
}

impl ProtonDrive {
    fn ensure_daemon_running(&mut self) -> bool {
        if daemon::is_running() {
            self.daemon_started = true;
            self.daemon_error = None;
            return true;
        }

        if self.daemon_started {
            return self.daemon_error.is_none();
        }

        self.daemon_started = true;
        match daemon::ensure_running(self.flags.force_offline, !self.flags.no_tray) {
            Ok(()) => {
                self.daemon_error = None;
                true
            }
            Err(e) => {
                let error = e.to_string();
                tracing::warn!(error = %error, "failed to start pdcli daemon");
                self.daemon_error = Some(error);
                false
            }
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
                    ui.add_space(8.0);
                    if ui.button("Mount Proton Drive").clicked() {
                        self.daemon_started = false;
                        self.ensure_daemon_running();
                    }
                    if let Some(error) = &self.daemon_error {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(format!("Mount failed: {error}"))
                                .color(ui.visuals().error_fg_color),
                        );
                    }
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
                ui.separator();
                ui.label("Global .pdignore");
                ui.label(
                    egui::RichText::new(pdignore::global_path().display().to_string())
                        .small()
                        .color(ui.visuals().weak_text_color()),
                );
                ui.add_space(6.0);
                ui.add(
                    egui::TextEdit::multiline(&mut self.pdignore_text)
                        .desired_rows(18)
                        .code_editor()
                        .lock_focus(true)
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        match pdignore::save_global_text(&self.pdignore_text) {
                            Ok(()) => {
                                self.pdignore_status = Some("Saved global .pdignore".into());
                            }
                            Err(e) => {
                                self.pdignore_status = Some(format!("Save failed: {e}"));
                            }
                        }
                    }
                    if ui.button("Reset defaults").clicked() {
                        self.pdignore_text = pdignore::DEFAULT_GLOBAL_PDIGNORE.to_string();
                        self.pdignore_status = Some("Defaults loaded. Save to apply them.".into());
                    }
                    if ui.button("Reload").clicked() {
                        self.pdignore_text = pdignore::load_global_text();
                        self.pdignore_status = Some("Reloaded global .pdignore".into());
                    }
                });
                if let Some(status) = &self.pdignore_status {
                    ui.add_space(6.0);
                    ui.label(status);
                }
            }
        });

        if sign_out {
            tracing::info!(user = %username, "user signed out");
            daemon::request_quit().ok();
            self.daemon_started = false;
            self.daemon_error = None;
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
