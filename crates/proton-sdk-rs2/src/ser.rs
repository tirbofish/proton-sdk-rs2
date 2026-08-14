use crate::PasswordMode;

/// A serializable snapshot of the credentials needed to resume a [`crate::session::ProtonAPISession`]
/// without re-authenticating.
///
/// Persist this value (e.g. to disk or a secrets store) after a successful login and pass it to
/// [`crate::session::ProtonAPISession::from_stored_credentials`] on the next startup.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct StoredCredentials {
    session_id: String,
    username: String,
    user_id: String,
    access_token: String,
    refresh_token: String,
    scopes: Vec<String>,
    is_waiting_for_second_factor_code: bool,
    password_mode: PasswordMode,
}

impl StoredCredentials {
    /// Creates a new [`StoredCredentials`] with the given values.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_id: String,
        username: String,
        user_id: String,
        access_token: String,
        refresh_token: String,
        scopes: Vec<String>,
        is_waiting_for_second_factor_code: bool,
        password_mode: PasswordMode,
    ) -> Self {
        Self {
            session_id,
            username,
            user_id,
            access_token,
            refresh_token,
            scopes,
            is_waiting_for_second_factor_code,
            password_mode,
        }
    }

    /// Returns the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the username.
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Returns the user ID.
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// Returns the access token.
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    /// Returns the refresh token.
    pub fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    /// Returns the list of granted scopes.
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    /// Returns whether the session was suspended pending a second-factor code.
    pub fn is_waiting_for_second_factor_code(&self) -> bool {
        self.is_waiting_for_second_factor_code
    }

    /// Returns the password mode in use for this account.
    pub fn password_mode(&self) -> PasswordMode {
        self.password_mode
    }
}
