use std::process::{Command, Stdio};

use poll_promise::Promise;
use proton_sdk_rs2::{
    AppVersionConfiguration, client::ProtonClientOptions, session::ProtonAPISession,
};

use crate::credentials;

enum LoginResult {
    Ready(ProtonAPISession),
    Needs2FA(ProtonAPISession, String),
}

enum LoginAction {
    Ready(ProtonAPISession),
    Needs2FA(ProtonAPISession, String),
    Error(String),
}

pub struct AuthScreen {
    username: String,
    password: String,
    error: Option<String>,
    login_task: Option<Promise<anyhow::Result<LoginResult>>>,
    phase: AuthPhase,
}

enum AuthPhase {
    Credentials,
    TwoFactor {
        session: Option<ProtonAPISession>,
        password: String,
        totp_code: String,
        totp_task: Option<Promise<anyhow::Result<ProtonAPISession>>>,
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

    fn begin_login(&mut self) {
        self.error = None;

        let (entity_cache, secret_cache) =
            credentials::open_session_caches().expect("failed to open session caches");

        let username = self.username.clone();
        let password = self.password.clone();

        tracing::info!(username = %username, "starting authentication");

        self.login_task = Some(Promise::spawn_async(async move {
            let session = ProtonAPISession::begin(
                username,
                &password,
                AppVersionConfiguration::new("pdcli", 0, 1, 0),
                ProtonClientOptions {
                    entity_cache_repository: Some(entity_cache),
                    secret_cache_repository: Some(secret_cache),
                    ..Default::default()
                },
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

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<ProtonAPISession> {
        match &mut self.phase {
            AuthPhase::Credentials => self.credentials_ui(ui),
            AuthPhase::TwoFactor { .. } => self.two_factor_ui(ui),
        }
    }

    fn credentials_ui(&mut self, ui: &mut egui::Ui) -> Option<ProtonAPISession> {
        let login_action = self
            .login_task
            .as_ref()
            .and_then(|task| task.ready())
            .map(|result| match result {
                Ok(LoginResult::Ready(session)) => LoginAction::Ready(session.clone()),
                Ok(LoginResult::Needs2FA(session, password)) => {
                    LoginAction::Needs2FA(session.clone(), password.clone())
                }
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
                LoginAction::Needs2FA(session, password) => {
                    self.phase = AuthPhase::TwoFactor {
                        session: Some(session),
                        password,
                        totp_code: String::new(),
                        totp_task: None,
                        error: None,
                    };
                    return None;
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
            ui.add_space(20.0);

            ui.add_sized(
                [300.0, 28.0],
                egui::TextEdit::singleline(&mut self.username).hint_text("Username"),
            );
            ui.add_space(8.0);
            ui.add_sized(
                [300.0, 28.0],
                egui::TextEdit::singleline(&mut self.password)
                    .hint_text("Password")
                    .password(true),
            );
            ui.add_space(12.0);

            let is_loading = self
                .login_task
                .as_ref()
                .is_some_and(|t| t.ready().is_none());

            if is_loading {
                ui.spinner();
                ui.label("Authenticating…");
                ui.ctx().request_repaint();
            } else {
                let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let sign_in = ui
                    .add_sized([300.0, 32.0], egui::Button::new("Sign in"))
                    .clicked();
                if (sign_in || enter_pressed)
                    && !self.username.is_empty()
                    && !self.password.is_empty()
                {
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

    fn two_factor_ui(&mut self, ui: &mut egui::Ui) -> Option<ProtonAPISession> {
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

        let totp_action =
            totp_task
                .as_ref()
                .and_then(|task| task.ready())
                .map(|result| match result {
                    Ok(completed_session) => LoginAction::Ready(completed_session.clone()),
                    Err(error) => LoginAction::Error(error.to_string()),
                });

        if let Some(action) = totp_action {
            *totp_task = None;

            match action {
                LoginAction::Ready(completed_session) => {
                    tracing::info!(user = %completed_session.username, "2FA verified");
                    persist(&completed_session);
                    credentials::save_session_tokens_on_refresh(&completed_session);
                    return Some(completed_session);
                }
                LoginAction::Error(message) => {
                    tracing::error!(error = %message, "2FA verification failed");
                    *error = Some(message);
                }
                LoginAction::Needs2FA(_, _) => {
                    unreachable!("2FA task cannot request another 2FA transition");
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

            ui.add_sized(
                [200.0, 28.0],
                egui::TextEdit::singleline(totp_code).hint_text("TOTP code"),
            );
            ui.add_space(12.0);

            if totp_task.is_some() {
                ui.spinner();
                ui.label("Verifying…");
                ui.ctx().request_repaint();
            } else {
                let enter_pressed = ui.input(|i| i.key_pressed(egui::Key::Enter));
                let verify_clicked = ui
                    .add_sized([200.0, 32.0], egui::Button::new("Verify"))
                    .clicked();
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

            if let Some(mut sess) = session.clone() {
                *error = None;
                let code = totp_code.clone();
                let pw = password.clone();

                *totp_task = Some(Promise::spawn_async(async move {
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
