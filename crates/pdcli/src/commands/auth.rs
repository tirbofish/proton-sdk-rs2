use crate::auth;
use crate::state::ReplState;
use anyhow::Result;
use proton_drive_sdk::client::ProtonDriveClient;
use std::sync::Arc;
use parking_lot::Mutex;

use super::helpers::new_spinner;

pub async fn auth_command_with_options(
    state: &Arc<Mutex<ReplState>>,
    announce: bool,
) -> Result<()> {
    let (session, username) = auth::authenticate().await?;
    apply_authenticated_session_with_options(state, session, username, announce).await
}

pub async fn apply_authenticated_session_with_options(
    state: &Arc<Mutex<ReplState>>,
    session: proton_sdk_rs2::session::ProtonAPISession,
    username: String,
    announce: bool,
) -> Result<()> {
    let drive_client = ProtonDriveClient::new(&session, None)?;

    let sp = new_spinner("Connecting to Proton Drive...");
    let my_files = drive_client.get_my_files_folder().await?;
    sp.finish_and_clear();

    let root_uid = my_files.base.uid.clone();

    let mut s = state.lock();
    s.set_session(session);
    s.set_client(drive_client);
    s.set_username(username.clone());
    s.set_root_node_uid(root_uid.clone());
    s.set_current_node_uid(root_uid);
    s.set_current_path(vec!["MyFiles".to_string()]);

    if announce {
        println!("Welcome, {}! You are now in My Files.", username);
    }
    Ok(())
}

/// whoami
pub async fn whoami_command(state: &Arc<Mutex<ReplState>>) -> Result<()> {
    let s = state.lock();
    match s.get_username() {
        Some(u) => {
            println!("Logged in as: {}", u);
            println!("Current path: {}", s.current_path_display());
        }
        None => println!("Not authenticated. Use 'login' to authenticate."),
    }
    Ok(())
}

/// logout
pub async fn logout_command(state: &Arc<Mutex<ReplState>>) -> Result<()> {
    let mut s = state.lock();
    s.clear_session();
    auth::clear_persisted_session();

    // Clear command history
    if let Ok(paths) = crate::app_paths::resolve_paths() {
        let _ = std::fs::remove_file(paths.history_path);
    }

    println!("Logged out.");
    Ok(())
}
