use std::sync::Arc;

use poll_promise::Promise;
use proton_drive_sdk::cache::sqlite::SqliteCacheRepository;
use proton_sdk_rs2::{
    AppVersionConfiguration, cache::{CacheRepository}, client::ProtonClientOptions, session::ProtonAPISession
};

use crate::{auth, credentials, flags, transfer::{TransferDirection, TransferTracker, format_bytes}, tray};

enum AppState {
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
    flags: flags::ClientFlags,

    state: AppState,
    active_page: MenuPage,
    transfer_tracker: TransferTracker,
    _tray: Option<tray_icon::TrayIcon>,
}

impl ProtonDrive {
    pub fn new() -> Self {
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
                        }
                    );
                    session.ensure_authenticated().await?;
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
            flags: flags::ClientFlags::default(),
            active_page: MenuPage::Status,
            transfer_tracker: TransferTracker::new(),
            _tray: None,
        }
    }

    pub fn with_tray(mut self, tray: Option<tray_icon::TrayIcon>) -> Self {
        self._tray = tray;
        self
    }

    pub fn handle_flags(mut self) -> Self {
        self.flags.apply_flags();
        self
    }
}

impl eframe::App for ProtonDrive {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        tray::poll_events();

        egui::CentralPanel::default().show_inside(ui, |ui| {
            match &mut self.state {
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
                    self.show_authenticated(ui);
                    self.mount_fuse();
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
            }
        });
    }
}

impl ProtonDrive {
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
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Self::sidebar_button(ui, active_page, "Settings", MenuPage::Settings);
                    Self::sidebar_button(ui, active_page, "Account", MenuPage::Account);
                    ui.separator();
                });
            });

        let page = self.active_page;
        let mut sign_out = false;

        egui::CentralPanel::default().show_inside(ui, |ui| {
            match page {
                MenuPage::Status => {
                    ui.heading("Status");
                    ui.separator();

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
                                                TransferDirection::Upload => (
                                                    "⬆",
                                                    egui::Color32::from_rgb(100, 180, 255),
                                                ),
                                                TransferDirection::Download => (
                                                    "⬇",
                                                    egui::Color32::from_rgb(100, 220, 130),
                                                ),
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
                    ui.label("FUSE mount configuration will go here.");
                }
                MenuPage::About => {
                    ui.heading("About");
                    ui.label(format!("pdcli {} - Proton Drive for Linux", env!("CARGO_PKG_VERSION")));
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
            }
        });

        if sign_out {
            tracing::info!(user = %username, "user signed out");
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