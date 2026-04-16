mod auth;
mod credentials;
mod db;
mod task;
mod tray;

use std::sync::Arc;

use proton_sdk_rs2::{
    cache::InMemoryCacheRepository,
    session::ProtonAPISession,
    AppVersionConfiguration,
};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "pdcli=info".into()),
        )
        .init();

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let native_options = eframe::NativeOptions::default();

    let icon_path = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/icon.png"));
    let create_tray = tray::init(&icon_path);

    eframe::run_native(
        "Proton Drive",
        native_options,
        Box::new(move |_| {
            let tray_handle = create_tray();
            Ok(Box::new(ProtonDrive::new(rt).with_tray(tray_handle)))
        }),
    )
    .unwrap();
}

enum AppState {
    /// Attempting to restore a session from stored credentials.
    Restoring(task::AsyncTask<anyhow::Result<ProtonAPISession>>),
    /// No valid session — show the login form.
    Login(auth::AuthScreen),
    /// Authenticated and ready.
    Authenticated(ProtonAPISession),
}

pub struct ProtonDrive {
    state: AppState,
    rt: tokio::runtime::Runtime,
    _tray: Option<tray_icon::TrayIcon>,
}

impl ProtonDrive {
    fn new(rt: tokio::runtime::Runtime) -> Self {
        let state = match credentials::load() {
            Some(cred) => {
                tracing::info!("found stored credentials, restoring session");
                let task = task::AsyncTask::spawn(rt.handle(), async move {
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

        Self { state, rt, _tray: None }
    }

    fn with_tray(mut self, tray: Option<tray_icon::TrayIcon>) -> Self {
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

                    if let Some(result) = task.poll() {
                        match result {
                            Ok(session) => {
                                tracing::info!(user = %session.username, "session restored");
                                self.state = AppState::Authenticated(session);
                            }
                            Err(ref e) => {
                                tracing::warn!(error = %e, "session restore failed, clearing credentials");
                                credentials::remove();
                                self.state = AppState::Login(auth::AuthScreen::new());
                            }
                        }
                    }
                }

                AppState::Login(screen) => {
                    if let Some(session) = screen.ui(ui, self.rt.handle()) {
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