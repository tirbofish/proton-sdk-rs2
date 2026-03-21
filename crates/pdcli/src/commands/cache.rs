use anyhow::Result;
use std::sync::Arc;
use parking_lot::Mutex;
use crate::state::ReplState;
use crate::app_paths::resolve_paths;
use crate::auth::clear_persisted_session;

pub async fn cache_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        anyhow::bail!("Usage: cache <get|clear> [item]");
    }

    match args[0] {
        "get" => {
            if args.len() < 2 {
                anyhow::bail!("Usage: cache get <cred|secrets|history|cache>");
            }
            let paths = resolve_paths()?;
            match args[1] {
                "cred" => println!("{}", paths.credentials_path.display()),
                "secrets" => println!("{}", paths.secrets_cache_path.display()),
                "history" => println!("{}", paths.history_path.display()),
                "cache" => println!("{}", paths.cache_dir.display()),
                _ => anyhow::bail!("Unknown cache item: {}", args[1]),
            }
        }
        "clear" => {
            println!("Clearing all cache and logging out...");
            
            // 1. Logout and clear session
            {
                let mut s = state.lock();
                s.clear_session();
            }
            clear_persisted_session();

            // 2. Resolve paths
            let paths = resolve_paths()?;

            // 3. Remove files
            let _ = std::fs::remove_file(&paths.secrets_cache_path);
            let _ = std::fs::remove_file(&paths.history_path);
            let _ = std::fs::remove_dir_all(&paths.cache_dir); // This removes the SQLite DB too

            println!("Cache cleared successfully.");
        }
        _ => anyhow::bail!("Unknown cache subcommand: {}", args[0]),
    }

    Ok(())
}
