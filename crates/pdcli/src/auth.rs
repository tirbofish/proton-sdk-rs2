use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use poll_promise::Promise;
use proton_sdk_rs2::{
    AppVersionConfiguration, client::ProtonClientOptions, session::ProtonAPISession,
};

use crate::credentials;

enum LoginAction {
    Ready(ProtonAPISession),
    Error(String),
}

#[derive(Clone)]
struct BrowserSignIn {
    url: String,
    user_code: String,
}

pub struct AuthScreen {
    error: Option<String>,
    login_task: Option<Promise<anyhow::Result<ProtonAPISession>>>,
    browser_sign_in: Arc<Mutex<Option<BrowserSignIn>>>,
}

impl AuthScreen {
    pub fn new() -> Self {
        Self {
            error: None,
            login_task: None,
            browser_sign_in: Arc::new(Mutex::new(None)),
        }
    }

    fn begin_login(&mut self) {
        self.error = None;
        *self.browser_sign_in.lock().unwrap() = None;
        let browser_sign_in = self.browser_sign_in.clone();

        self.login_task = Some(Promise::spawn_async(async move {
            let (entity_cache, secret_cache) = credentials::open_session_caches()?;

            tracing::info!("starting browser authentication");
            ProtonAPISession::begin_via_web(
                AppVersionConfiguration::new("pdcli", 0, 1, 0),
                ProtonClientOptions {
                    entity_cache_repository: Some(entity_cache),
                    secret_cache_repository: Some(secret_cache),
                    ..Default::default()
                },
                move |url, user_code| {
                    *browser_sign_in.lock().unwrap() = Some(BrowserSignIn {
                        url: url.to_owned(),
                        user_code: user_code.to_owned(),
                    });
                    open_browser(url);
                },
            )
            .await
        }));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<ProtonAPISession> {
        let login_action = self
            .login_task
            .as_ref()
            .and_then(|task| task.ready())
            .map(|result| match result {
                Ok(session) => LoginAction::Ready(session.clone()),
                Err(error) => LoginAction::Error(error.to_string()),
            });

        if let Some(action) = login_action {
            self.login_task = None;

            match action {
                LoginAction::Ready(session) => {
                    persist(&session);
                    credentials::save_session_tokens_on_refresh(&session);
                    return Some(session);
                }
                LoginAction::Error(error) => {
                    tracing::error!(error = %error, "login failed");
                    self.error = Some(error);
                }
            }
        }

        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.heading("Sign in to Proton Drive");
            ui.add_space(8.0);
            ui.label("Sign in securely in your browser to continue.");
            ui.add_space(20.0);

            let is_loading = self
                .login_task
                .as_ref()
                .is_some_and(|t| t.ready().is_none());

            if is_loading {
                ui.spinner();
                ui.label("Waiting for browser sign-in…");

                let details = self.browser_sign_in.lock().unwrap().clone();
                if let Some(details) = details {
                    ui.add_space(12.0);
                    ui.label("Confirm that this code appears in your browser:");
                    ui.monospace(&details.user_code);
                    ui.add_space(8.0);
                    ui.hyperlink_to("Open sign-in page", details.url);
                }
                ui.ctx().request_repaint();
            } else {
                let sign_in = ui
                    .add_sized([300.0, 32.0], egui::Button::new("Sign in with browser"))
                    .clicked();
                if sign_in {
                    self.begin_login();
                }
            }

            if let Some(err) = &self.error {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::RED, err);
            }
        });

        None
    }
}

fn persist(session: &ProtonAPISession) {
    if let Err(e) = credentials::save(&session.to_stored_credentials()) {
        tracing::warn!(error = %e, "failed to persist credentials");
    }
}

pub async fn login_cli() -> anyhow::Result<ProtonAPISession> {
    let (entity_cache, secret_cache) = credentials::open_session_caches()?;

    println!("This is a third-party application not officially supported by Proton.");
    tracing::info!("starting browser authentication");
    let session = ProtonAPISession::begin_via_web(
        AppVersionConfiguration::new("pdcli", 0, 1, 0),
        ProtonClientOptions {
            entity_cache_repository: Some(entity_cache),
            secret_cache_repository: Some(secret_cache),
            ..Default::default()
        },
        |url, user_code| {
            println!("Complete sign-in in the browser. Keep this terminal open.");
            println!("Sign-in code: {user_code}");
            println!("{url}");
            open_browser(url);
        },
    )
    .await?;

    tracing::info!(user = %session.username, "authenticated");
    persist(&session);
    credentials::save_session_tokens_on_refresh(&session);
    Ok(session)
}

fn open_browser(url: &str) {
    for cmd in ["xdg-open", "wslview"] {
        let _ = Command::new(cmd)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}
