use std::sync::Arc;

use poll_promise::Promise;
use proton_sdk_rs2::{
    cache::InMemoryCacheRepository,
    session::ProtonAPISession,
    AppVersionConfiguration,
};

use crate::{auth, credentials, tray};

enum AppState {
    Restoring(Promise<anyhow::Result<ProtonAPISession>>),
    Login(auth::AuthScreen),
    Authenticated(ProtonAPISession),
}

pub struct ProtonDrive {
    state: AppState,
    _tray: Option<tray_icon::TrayIcon>,
}

impl ProtonDrive {
    pub fn new() -> Self {
        let state = match credentials::load() {
            Some(cred) => {
                tracing::info!("found stored credentials, restoring session");
                let task = Promise::spawn_async(async move {
                    let session = ProtonAPISession::from_stored_credentials(
                        cred,
                        AppVersionConfiguration::new("pdcli", 0, 1, 0),
                        Arc::new(InMemoryCacheRepository::new()),
                    );
                    Ok(session)
                });
                AppState::Restoring(task)
            }
            None => {
                tracing::info!("no stored credentials, showing login");
                AppState::Login(auth::AuthScreen::new())
            }
        };

        Self { state, _tray: None }
    }

    pub fn with_tray(mut self, tray: Option<tray_icon::TrayIcon>) -> Self {
        self._tray = tray;
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
                                tracing::warn!(error = %e, "session restore failed, clearing credentials");
                                credentials::remove();
                                self.state = AppState::Login(auth::AuthScreen::new());
                            }
                        }
                    }
                }

                AppState::Login(screen) => {
                    if let Some(session) = screen.ui(ui) {
                        self.state = AppState::Authenticated(session);
                    }
                }

                AppState::Authenticated(session) => {
                    ui.heading(format!("Welcome, {}", session.username));

                    if ui.button("Sign out").clicked() {
                        tracing::info!(user = %session.username, "user signed out");
                        credentials::remove();
                        self.state = AppState::Login(auth::AuthScreen::new());
                    }
                }
            }
        });
    }
}