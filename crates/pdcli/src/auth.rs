//! Authentication module for pdcli.
//!
//! Handles login, logout, and session persistence using:
//! - `.ron` file for session tokens (access/refresh tokens, session ID)
//! - SQLite cache for entity data
//! - Never stores passwords

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use console::style;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password};
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use proton_drive_sdk::cache::sqlite::SqliteCacheRepository;
use proton_sdk_rs2::{
    cache::CacheRepository,
    session::ProtonAPISession,
    ser::StoredCredentials,
    AppVersionConfiguration,
    PasswordMode,
};

/// Session tokens stored in a .ron file for resumption.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTokens {
    pub session_id: String,
    pub username: String,
    pub user_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub scopes: Vec<String>,
    pub is_waiting_for_second_factor_code: bool,
    pub password_mode: PasswordMode,
}

impl From<StoredCredentials> for SessionTokens {
    fn from(cred: StoredCredentials) -> Self {
        Self {
            session_id: cred.session_id().to_string(),
            username: cred.username().to_string(),
            user_id: cred.user_id().to_string(),
            access_token: cred.access_token().to_string(),
            refresh_token: cred.refresh_token().to_string(),
            scopes: cred.scopes().to_vec(),
            is_waiting_for_second_factor_code: cred.is_waiting_for_second_factor_code(),
            password_mode: cred.password_mode(),
        }
    }
}

impl SessionTokens {
    pub fn to_stored_credentials(&self) -> StoredCredentials {
        StoredCredentials::new(
            self.session_id.clone(),
            self.username.clone(),
            self.user_id.clone(),
            self.access_token.clone(),
            self.refresh_token.clone(),
            self.scopes.clone(),
            self.is_waiting_for_second_factor_code,
            self.password_mode,
        )
    }
}

/// Authentication state for the CLI.
pub struct ProtonAuth {
    /// Current session, if authenticated
    session: RwLock<Option<ProtonAPISession>>,
    /// Path to config directory
    config_dir: PathBuf,
    /// Path to session tokens file
    tokens_path: PathBuf,
    /// Path to cache database
    cache_path: PathBuf,
    /// App version for API requests
    app_version: AppVersionConfiguration,
}

impl ProtonAuth {
    /// Create a new ProtonAuth instance.
    pub fn new() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .context("Could not determine config directory")?
            .join("pdcli");
        
        std::fs::create_dir_all(&config_dir)
            .context("Failed to create config directory")?;
        
        let tokens_path = config_dir.join("session.ron");
        let cache_path = config_dir.join("cache.db");
        
        let app_version = AppVersionConfiguration::new("pdcli", 0, 1, 0);
        
        Ok(Self {
            session: RwLock::new(None),
            config_dir,
            tokens_path,
            cache_path,
            app_version,
        })
    }

    /// Get the config directory path.
    pub fn config_dir(&self) -> &PathBuf {
        &self.config_dir
    }

    /// Check if session tokens exist on disk.
    pub fn has_stored_session(&self) -> bool {
        self.tokens_path.exists()
    }

    /// Load session tokens from disk.
    fn load_tokens(&self) -> Result<Option<SessionTokens>> {
        if !self.tokens_path.exists() {
            return Ok(None);
        }
        
        let content = std::fs::read_to_string(&self.tokens_path)
            .context("Failed to read session tokens")?;
        
        let tokens: SessionTokens = ron::from_str(&content)
            .context("Failed to parse session tokens")?;
        
        Ok(Some(tokens))
    }

    /// Save session tokens to disk.
    fn save_tokens(&self, tokens: &SessionTokens) -> Result<()> {
        let content = ron::ser::to_string_pretty(tokens, ron::ser::PrettyConfig::default())
            .context("Failed to serialize session tokens")?;
        
        std::fs::write(&self.tokens_path, content)
            .context("Failed to write session tokens")?;
        
        Ok(())
    }

    /// Delete stored session tokens.
    fn delete_tokens(&self) -> Result<()> {
        if self.tokens_path.exists() {
            std::fs::remove_file(&self.tokens_path)
                .context("Failed to delete session tokens")?;
        }
        Ok(())
    }

    /// Create a cache repository.
    fn create_cache_repository(&self) -> Result<Arc<dyn CacheRepository>> {
        let repo = SqliteCacheRepository::open_file(&self.cache_path, Some(10000))
            .context("Failed to open cache database")?;
        Ok(Arc::new(repo))
    }

    /// Try to resume an existing session.
    pub async fn try_resume(&self) -> Result<bool> {
        let tokens = match self.load_tokens()? {
            Some(t) => t,
            None => return Ok(false),
        };

        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap()
        );
        spinner.set_message("Resuming session...");
        spinner.enable_steady_tick(std::time::Duration::from_millis(80));

        let cache_repo = self.create_cache_repository()?;
        
        let session = ProtonAPISession::from_stored_credentials(
            tokens.to_stored_credentials(),
            self.app_version.clone(),
            cache_repo,
        );

        // Try to ensure the session is still valid
        let mut session = session;
        match session.ensure_authenticated().await {
            Ok(()) => {
                spinner.finish_with_message(format!(
                    "{} Resumed session for {}",
                    style("✓").green().bold(),
                    style(&tokens.username).cyan()
                ));
                
                // Save updated tokens (they may have been refreshed)
                let new_tokens: SessionTokens = session.to_stored_credentials().into();
                self.save_tokens(&new_tokens)?;
                
                *self.session.write().await = Some(session);
                Ok(true)
            }
            Err(e) => {
                spinner.finish_with_message(format!(
                    "{} Session expired or invalid",
                    style("✗").red().bold()
                ));
                tracing::debug!("Session resume failed: {}", e);
                Ok(false)
            }
        }
    }

    /// Prompt user for login credentials and authenticate.
    pub async fn login_interactive(&self) -> Result<()> {
        let theme = ColorfulTheme::default();

        println!();
        println!("{}", style("Proton Drive Login").bold().cyan());
        println!();

        // Get username
        let username: String = Input::with_theme(&theme)
            .with_prompt("Email or username")
            .interact_text()
            .context("Failed to read username")?;

        // Get password (not stored anywhere)
        let password: String = Password::with_theme(&theme)
            .with_prompt("Password")
            .interact()
            .context("Failed to read password")?;

        // Show spinner while authenticating
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap()
        );
        spinner.set_message("Authenticating...");
        spinner.enable_steady_tick(std::time::Duration::from_millis(80));

        // Attempt authentication
        let session = ProtonAPISession::begin(
            &username,
            &password,
            self.app_version.clone(),
            Default::default(),
        ).await;

        // Password is now out of scope and will be dropped
        drop(password);

        let mut session = match session {
            Ok(s) => s,
            Err(e) => {
                spinner.finish_with_message(format!(
                    "{} Authentication failed",
                    style("✗").red().bold()
                ));
                return Err(e).context("Authentication failed");
            }
        };

        // Handle 2FA if required
        if session.is_waiting_for_second_factor_code {
            spinner.finish_with_message("Two-factor authentication required");
            
            let code: String = Input::with_theme(&theme)
                .with_prompt("Enter 2FA code")
                .interact_text()
                .context("Failed to read 2FA code")?;

            spinner.set_message("Verifying 2FA code...");
            spinner.enable_steady_tick(std::time::Duration::from_millis(80));

            session.apply_second_factor_code(code).await
                .context("2FA verification failed")?;
        }

        spinner.finish_with_message(format!(
            "{} Logged in as {}",
            style("✓").green().bold(),
            style(&username).cyan()
        ));

        // Save session tokens
        let tokens: SessionTokens = session.to_stored_credentials().into();
        self.save_tokens(&tokens)?;

        *self.session.write().await = Some(session);

        println!();
        println!("{}", style("Session saved. You can now use pdcli commands.").dim());

        Ok(())
    }

    /// Logout and clear stored session.
    pub async fn logout(&self) -> Result<()> {
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap()
        );
        spinner.set_message("Logging out...");
        spinner.enable_steady_tick(std::time::Duration::from_millis(80));

        // End session on server if we have one
        let mut session_guard = self.session.write().await;
        if let Some(mut session) = session_guard.take() {
            if let Err(e) = session.end_from_session().await {
                tracing::warn!("Failed to end session on server: {}", e);
            }
        }

        // Delete local tokens
        self.delete_tokens()?;

        spinner.finish_with_message(format!(
            "{} Logged out successfully",
            style("✓").green().bold()
        ));

        Ok(())
    }

    /// Ensure we have an authenticated session.
    /// If not authenticated, prompts for login or exit.
    pub async fn ensure_authenticated(&self) -> Result<bool> {
        // Check if already authenticated in memory
        {
            let session = self.session.read().await;
            if session.is_some() {
                return Ok(true);
            }
        }

        // Try to resume from stored tokens
        if self.try_resume().await? {
            return Ok(true);
        }

        // No valid session - prompt user
        let theme = ColorfulTheme::default();
        
        if self.has_stored_session() {
            // Had a session but it's invalid
            println!();
            println!("{}", style("Your session has expired or is invalid.").yellow());
            
            let choice = Confirm::with_theme(&theme)
                .with_prompt("Would you like to login again?")
                .default(true)
                .interact()
                .context("Failed to read user choice")?;

            if choice {
                self.login_interactive().await?;
                Ok(true)
            } else {
                Ok(false)
            }
        } else {
            // First time user
            println!();
            println!("{}", style("Welcome to Proton Drive CLI!").bold().cyan());
            println!("You need to login to continue.");
            println!();
            
            let choice = Confirm::with_theme(&theme)
                .with_prompt("Would you like to login now?")
                .default(true)
                .interact()
                .context("Failed to read user choice")?;

            if choice {
                self.login_interactive().await?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }

    /// Get a reference to the current session if authenticated.
    pub async fn session(&self) -> Option<tokio::sync::RwLockReadGuard<'_, Option<ProtonAPISession>>> {
        let guard = self.session.read().await;
        if guard.is_some() {
            Some(guard)
        } else {
            None
        }
    }

    /// Check if currently authenticated.
    pub async fn is_authenticated(&self) -> bool {
        self.session.read().await.is_some()
    }

    /// Get current account information from stored tokens.
    pub fn get_account_info(&self) -> Option<AccountInfo> {
        self.load_tokens().ok().flatten().map(|tokens| AccountInfo {
            username: tokens.username,
            user_id: tokens.user_id,
            session_id: tokens.session_id,
        })
    }

    /// Logout and clear ALL stored data (session, cache, etc.).
    pub async fn logout_clear(&self) -> Result<()> {
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .unwrap()
        );
        spinner.set_message("Clearing all data...");
        spinner.enable_steady_tick(std::time::Duration::from_millis(80));

        // End session on server if we have one
        let mut session_guard = self.session.write().await;
        if let Some(mut session) = session_guard.take() {
            if let Err(e) = session.end_from_session().await {
                tracing::warn!("Failed to end session on server: {}", e);
            }
        }

        // Delete all stored data
        self.delete_all_data()?;

        spinner.finish_with_message(format!(
            "{} All data cleared",
            style("✓").green().bold()
        ));

        Ok(())
    }

    /// Delete all stored data (tokens, cache, etc.).
    fn delete_all_data(&self) -> Result<()> {
        // Delete session tokens
        if self.tokens_path.exists() {
            std::fs::remove_file(&self.tokens_path)
                .context("Failed to delete session tokens")?;
        }

        // Delete cache database
        if self.cache_path.exists() {
            std::fs::remove_file(&self.cache_path)
                .context("Failed to delete cache database")?;
        }

        // Delete any other files in config directory
        // Keep the directory itself
        if self.config_dir.exists() {
            for entry in std::fs::read_dir(&self.config_dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("Failed to delete {}", path.display()))?;
                }
            }
        }

        Ok(())
    }
}

/// Information about the current account.
#[derive(Debug, Clone)]
pub struct AccountInfo {
    pub username: String,
    pub user_id: String,
    pub session_id: String,
}