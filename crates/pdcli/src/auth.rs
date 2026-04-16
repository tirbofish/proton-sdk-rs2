use std::sync::Arc;

use proton_sdk_rs2::{
    cache::CacheRepository,
    client::ProtonClientOptions,
    session::ProtonAPISession,
    AppVersionConfiguration,
};

use crate::{credentials, task::AsyncTask};

enum LoginResult {
    Ready(ProtonAPISession),
    Needs2FA(ProtonAPISession, String),
}

pub struct AuthScreen {
    username: String,
    password: String,
    error: Option<String>,
    login_task: Option<AsyncTask<anyhow::Result<LoginResult>>>,
    phase: AuthPhase,
}

enum AuthPhase {
    /// Collecting username + password.
    Credentials,
    /// Waiting for a TOTP code. `session` is `None` while the verification task owns it.
    TwoFactor {
        session: Option<ProtonAPISession>,
        password: String,
        totp_code: String,
        totp_task: Option<AsyncTask<anyhow::Result<ProtonAPISession>>>,
        error: Option<String>,
    },
}

impl AuthScreen {
    pub fn new() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            error: None,
            login_task: None,
            phase: AuthPhase::Credentials,
        }
    }

    fn begin_login(&mut self, rt: &tokio::runtime::Handle, entity_repo: Arc<dyn CacheRepository>) {
        self.error = None;
        let username = self.username.clone();
        let password = self.password.clone();

        tracing::info!(username = %username, "starting authentication");

        self.login_task = Some(AsyncTask::spawn(rt, async move {
            let options = ProtonClientOptions {
                entity_cache_repository: Some(entity_repo.clone()),
                secret_cache_repository: Some(entity_repo),
                ..ProtonClientOptions::default()
            };
            let session = ProtonAPISession::begin(
                username,
                &password,
                AppVersionConfiguration::new("pdcli", 0, 1, 0),
                options,
            )
            .await?;

            if session.is_waiting_for_second_factor_code {
                tracing::info!("2FA required");
                Ok(LoginResult::Needs2FA(session, password))
            } else {
                tracing::info!(user = %session.username, "authenticated");
                Ok(LoginResult::Ready(session))
            }
        }));
    }

    fn is_busy(&self) -> bool {
        self.login_task.is_some()
    }

    /// Renders the auth UI. Returns `Some(session)` once fully authenticated.
    pub fn ui(&mut self, ui: &mut egui::Ui, rt: &tokio::runtime::Handle, entity_repo: Arc<dyn CacheRepository>) -> Option<ProtonAPISession> {
        match &mut self.phase {
            AuthPhase::Credentials => self.credentials_ui(ui, rt, entity_repo),
            AuthPhase::TwoFactor { .. } => self.two_factor_ui(ui, rt),
        }
    }

    fn credentials_ui(&mut self, ui: &mut egui::Ui, rt: &tokio::runtime::Handle, entity_repo: Arc<dyn CacheRepository>) -> Option<ProtonAPISession> {
        // Poll the login task
        if let Some(task) = &mut self.login_task {
            if let Some(result) = task.poll() {
                self.login_task = None;
                match result {
                    Ok(LoginResult::Ready(session)) => {
                        persist(&session);
                        return Some(session);
                    }
                    Ok(LoginResult::Needs2FA(session, password)) => {
                        self.phase = AuthPhase::TwoFactor {
                            session: Some(session),
                            password,
                            totp_code: String::new(),
                            totp_task: None,
                            error: None,
                        };
                        return None;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "login failed");
                        self.error = Some(e.to_string());
                    }
                }
            }
        }

        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.heading("Sign in to Proton Drive");
            ui.add_space(20.0);

            ui.add_sized([300.0, 28.0], egui::TextEdit::singleline(&mut self.username).hint_text("Username"));
            ui.add_space(8.0);
            ui.add_sized([300.0, 28.0], egui::TextEdit::singleline(&mut self.password).hint_text("Password").password(true));
            ui.add_space(12.0);

            if self.is_busy() {
                ui.spinner();
                ui.label("Authenticating…");
                ui.ctx().request_repaint();
            } else {
                let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let sign_in = ui.add_sized([300.0, 32.0], egui::Button::new("Sign in")).clicked();
                if (sign_in || enter_pressed) && !self.username.is_empty() && !self.password.is_empty() {
                    self.begin_login(rt, entity_repo);
                }
            }

            if let Some(err) = &self.error {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::RED, err);
            }
        });

        None
    }

    fn two_factor_ui(&mut self, ui: &mut egui::Ui, rt: &tokio::runtime::Handle) -> Option<ProtonAPISession> {
        let AuthPhase::TwoFactor {
            session: _,
            password: _,
            ref mut totp_code,
            ref mut totp_task,
            ref mut error,
        } = self.phase
        else {
            return None;
        };

        // Poll the 2FA verification task
        if let Some(task) = totp_task {
            if let Some(result) = task.poll() {
                *totp_task = None;
                match result {
                    Ok(completed_session) => {
                        tracing::info!(user = %completed_session.username, "2FA verified");
                        persist(&completed_session);
                        return Some(completed_session);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "2FA verification failed");
                        *error = Some(e.to_string());
                    }
                }
            }
        }

        let mut should_submit = false;

        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.heading("Two-factor authentication");
            ui.add_space(8.0);
            ui.label("Enter the 6-digit code from your authenticator app.");
            ui.add_space(20.0);

            ui.add_sized([200.0, 28.0], egui::TextEdit::singleline(totp_code).hint_text("TOTP code"));
            ui.add_space(12.0);

            if totp_task.is_some() {
                ui.spinner();
                ui.label("Verifying…");
                ui.ctx().request_repaint();
            } else {
                let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let verify_clicked = ui.add_sized([200.0, 32.0], egui::Button::new("Verify")).clicked();
                if (verify_clicked || enter_pressed) && !totp_code.is_empty() {
                    should_submit = true;
                }
            }

            if let Some(err) = error {
                ui.add_space(8.0);
                ui.colored_label(egui::Color32::RED, err.as_str());
            }
        });

        if should_submit {
            let AuthPhase::TwoFactor {
                ref mut session,
                ref password,
                ref totp_code,
                ref mut totp_task,
                ref mut error,
            } = self.phase
            else {
                return None;
            };

            // Take the session out — the async task will own it while running.
            if let Some(mut sess) = session.take() {
                *error = None;
                let code = totp_code.clone();
                let pw = password.clone();

                *totp_task = Some(AsyncTask::spawn(rt, async move {
                    sess.apply_second_factor_code(code).await?;
                    sess.apply_data_password(&pw).await?;
                    Ok(sess)
                }));
            }
        }

        None
    }
}

fn persist(session: &ProtonAPISession) {
    if let Err(e) = credentials::save(&session.to_stored_credentials()) {
        tracing::warn!(error = %e, "failed to persist credentials");
    }
}