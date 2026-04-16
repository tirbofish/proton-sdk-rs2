mod auth;
mod credentials;
mod db;
mod task;
mod tray;
mod mount;
mod pages;
mod prefs;

use std::sync::Arc;

use proton_drive_sdk::client::ProtonDriveClient;
use proton_sdk_rs2::{
    session::ProtonAPISession,
    AppVersionConfiguration,
};
use tokio_util::sync::CancellationToken;

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
    /// Authenticated and FUSE mounted.
    Ready {
        session: ProtonAPISession,
        drive_client: ProtonDriveClient,
        mount_path: std::path::PathBuf,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Status,
    Account,
    Settings,
}

pub struct ProtonDrive {
    state: AppState,
    rt: tokio::runtime::Runtime,
    _tray: Option<tray_icon::TrayIcon>,
    page: Page,
    prefs: prefs::Preferences,
    cache: Arc<db::SQLIndexedCache>,
    shutdown: CancellationToken,
    transfers: Vec<pages::TransferItem>,
}

impl ProtonDrive {
    fn new(rt: tokio::runtime::Runtime) -> Self {
        let cache = Arc::new(
            db::SQLIndexedCache::open().expect("failed to open indexed cache"),
        );

        let state = match credentials::load() {
            Some(cred) => {
                tracing::info!("found stored credentials, restoring session");
                let entity_repo = cache.entity_repository();
                let task = task::AsyncTask::spawn(rt.handle(), async move {
                    let session = ProtonAPISession::from_stored_credentials(
                        cred,
                        AppVersionConfiguration::new("pdcli", 0, 1, 0),
                        entity_repo,
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

        let shutdown = CancellationToken::new();

        // ctrl+c triggers shutdown
        let shutdown_on_signal = shutdown.clone();
        rt.handle().spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("received ctrl+c, shutting down...");
                shutdown_on_signal.cancel();
            }
        });

        Self { state, rt, _tray: None, page: Page::Status, prefs: prefs::load(), cache, shutdown, transfers: Vec::new() }
    }

    fn with_tray(mut self, tray: Option<tray_icon::TrayIcon>) -> Self {
        self._tray = tray;
        self
    }

    fn start_mount(&self, session: ProtonAPISession) -> AppState {
        let drive_client = match ProtonDriveClient::new(&session, None) {
            Ok(client) => {
                tracing::info!("ProtonDriveClient initialised");
                client
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to create ProtonDriveClient");
                return AppState::Login(auth::AuthScreen::new());
            }
        };

        let mount_path = self.prefs.mount_path.clone();
        let path = mount_path.clone();
        let shutdown = self.shutdown.clone();

        // spawn fuse
        self.rt.handle().spawn(async move {
            let mut mount_handle = match mount::mount(path).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!(error = %e, "FUSE mount failed");
                    return;
                }
            };

            tokio::select! {
                res = &mut mount_handle => {
                    if let Err(e) = res {
                        tracing::error!(error = %e, "FUSE mount exited with error");
                    }
                },
                _ = shutdown.cancelled() => {
                    tracing::info!("shutdown requested, unmounting...");
                }
            }

            if let Err(e) = mount_handle.unmount().await {
                tracing::error!(error = %e, "failed to unmount");
            }
            tracing::info!("FUSE unmounted");
        });
        AppState::Ready { session, drive_client, mount_path }
    }
}

impl eframe::App for ProtonDrive {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        tray::poll_events();

        // If shutdown was requested (e.g. ctrl+c), close the window
        if self.shutdown.is_cancelled() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

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
                                self.state = self.start_mount(session);
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
                    if let Some(session) = screen.ui(ui, self.rt.handle(), self.cache.entity_repository()) {
                        self.state = self.start_mount(session);
                    }
                }

                AppState::Ready { session, drive_client: _, mount_path } => {
                    let username = session.username.clone();
                    let mp = mount_path.display().to_string();

                    egui::Panel::left("sidebar")
                        .resizable(false)
                        .default_size(160.0)
                        .show_inside(ui, |ui| {
                            ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                                ui.selectable_value(&mut self.page, Page::Status, "📊 Status");

                                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                                    ui.selectable_value(&mut self.page, Page::Settings, "⚙ Settings");
                                    ui.selectable_value(&mut self.page, Page::Account, "👤 Account");
                                });
                            });
                        });

                    let mut sign_out = false;

                    egui::CentralPanel::default().show_inside(ui, |ui| {
                        match self.page {
                            Page::Status => {
                                self.status_page(ui);
                            }
                            Page::Account => {
                                ProtonDrive::account_page(ui, &mut sign_out, &username, &mp);
                            }
                            Page::Settings => {
                                self.settings_page(ui);
                            }
                        }
                    });

                    if sign_out {
                        tracing::info!(user = %username, "user signed out");
                        credentials::remove();
                        self.state = AppState::Login(auth::AuthScreen::new());
                    }
                }
            }
        });
    }
}

impl Drop for ProtonDrive {
    fn drop(&mut self) {
        tracing::info!("ProtonDrive dropping, requesting shutdown...");
        self.shutdown.cancel();
        // Give the mount task a moment to unmount cleanly
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}