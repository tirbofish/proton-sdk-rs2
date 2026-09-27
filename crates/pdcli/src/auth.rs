use std::io::Write;
use std::process::{Command, Stdio};

use proton_drive_sdk::proton_sdk_rs2::{client::ProtonClientOptions, session::ProtonAPISession};

use crate::{credentials, version::app_version_configuration};

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
        app_version_configuration(),
        ProtonClientOptions {
            entity_cache_repository: Some(entity_cache),
            secret_cache_repository: Some(secret_cache),
            ..Default::default()
        },
        |url, user_code| {
            println!("Complete sign-in in the browser. Keep this terminal open.");
            println!("Sign-in code: {user_code}");
            println!("{url}");
            let _ = std::io::stdout().flush();
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
