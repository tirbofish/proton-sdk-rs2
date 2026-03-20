use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use proton_sdk_rs2::{
    PasswordMode, SessionId, UserId,
    client::ProtonClientOptions,
    session::{ProtonAPISession, ProtonSessionOptions},
};
use ron::ser::{PrettyConfig, to_string_pretty};

use crate::{DriveClient, file::FileCacheRepository};

impl DriveClient {
    pub async fn auth() -> anyhow::Result<Self> {
        let (session, cache) = authenticate().await?;
        Ok(Self { session, cache })
    }
}

pub(crate) async fn authenticate() -> anyhow::Result<(ProtonAPISession, Arc<FileCacheRepository>)> {
    let app_version = semver::Version::new(0, 1, 0);
    let cred_path = PathBuf::from("cred.ron"); // todo: make this appdirs
    let stored = load_credentials(&cred_path)?;
    let secret_cache_repository = Arc::new(FileCacheRepository::load(PathBuf::from("cache.json"))?);

    let session = if let Some(stored) = stored {
        println!("Found cred.ron, resuming stored session");

        let resumed = ProtonAPISession::resume(
            SessionId::new(stored.session_id),
            stored.username,
            UserId::new(stored.user_id),
            stored.access_token,
            stored.refresh_token,
            stored.scopes,
            stored.is_waiting_for_second_factor_code,
            stored.password_mode,
            app_version.clone(),
            secret_cache_repository.clone(),
        );

        println!("Session resumed from cred.ron");
        let mut resumed = resumed;
        match resumed.ensure_authenticated().await {
            Ok(()) => resumed,
            Err(e) => {
                println!("Session expired or invalid ({}), starting fresh login...", e);
                std::fs::remove_file(&cred_path).ok();
                begin_new_session(app_version.clone(), secret_cache_repository.clone()).await?
            }
        }
    } else {
        println!("No cred.ron found, creating a new session");
        begin_new_session(app_version.clone(), secret_cache_repository.clone()).await?
    };

    persist_credentials(&cred_path, &session).await?;
    println!("Saved credentials to {}", cred_path.display());

    Ok((session, secret_cache_repository))
}

async fn begin_new_session(
    app_version: semver::Version,
    cache: Arc<FileCacheRepository>,
) -> anyhow::Result<ProtonAPISession> {
    let username = prompt_input("Username")?;
    let password = rpassword::prompt_password("Enter password: ")?;

    let mut session = ProtonAPISession::begin(
        &username,
        &password,
        app_version,
        ProtonSessionOptions::new(ProtonClientOptions {
            secret_cache_repository: Some(cache.clone()),
            ..Default::default()
        }),
    )
    .await?;

    if session.is_waiting_for_second_factor_code {
        let mut line = String::new();
        println!("Enter 2FA code:");
        std::io::stdin().read_line(&mut line)?;
        let two_factor_code = line.trim().to_string();

        session.apply_second_factor_code(two_factor_code).await?;
    }

    if let Err(error) = session.apply_data_password(&password).await {
        log::warn!("Failed to apply data password: {error}");
    }

    Ok(session)
}

fn prompt_input(label: &str) -> anyhow::Result<String> {
    println!("Enter {label}:");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let value = line.trim().to_string();

    if value.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{label} cannot be empty"),
        )
        .into());
    }

    Ok(value)
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredCredentials {
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
    pub fn from_ron(content: &str) -> anyhow::Result<Self> {
        let result = ron::from_str::<StoredCredentials>(content)?;
        Ok(result)
    }
}

fn load_credentials(path: &Path) -> anyhow::Result<Option<StoredCredentials>> {
    match fs::read_to_string(path) {
        Ok(content) => match StoredCredentials::from_ron(content.as_str()) {
            Ok(stored) => Ok(Some(stored)),
            Err(error) => {
                println!("Ignoring invalid cred.ron ({error}), creating a new session");
                Ok(None)
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn persist_credentials(path: &Path, session: &ProtonAPISession) -> anyhow::Result<()> {
    let (access_token, refresh_token) = session.token_credential.get_tokens().await?;

    let session = StoredCredentials {
        session_id: session.session_id.to_string(),
        username: session.username.clone(),
        user_id: session.user_id.to_string(),
        access_token,
        refresh_token,
        scopes: session.scopes.clone(),
        is_waiting_for_second_factor_code: session.is_waiting_for_second_factor_code,
        password_mode: session.password_mode,
    };

    let writer = to_string_pretty(&session, PrettyConfig::default())?;

    fs::write(path, writer)?;
    Ok(())
}
