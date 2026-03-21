use anyhow::{anyhow, Result};
use std::sync::Arc;
use parking_lot::Mutex;
use crate::state::ReplState;
use crate::settings::{Settings, IndexMode};
use dialoguer::{Select, Input, theme::ColorfulTheme};

pub async fn settings_command(args: &[&str], state: &Arc<Mutex<ReplState>>) -> Result<()> {
    if args.is_empty() {
        tokio::task::spawn_blocking({
            let state = state.clone();
            move || show_settings_menu(&state)
        }).await??;
        Ok(())
    } else {
        match args[0] {
            "display" | "show" => {
                let s = state.lock();
                s.get_settings().display();
                Ok(())
            }
            "reset" => {
                let mut s = state.lock();
                s.set_settings(Settings::default());
                println!("Settings reset to defaults.");
                s.get_settings().display();
                Ok(())
            }
            _ => Err(anyhow!("Usage: settings [display|reset] or just 'settings' for interactive menu")),
        }
    }
}

fn show_settings_menu(state: &Arc<Mutex<ReplState>>) -> Result<()> {
    let theme = ColorfulTheme::default();
    loop {
        let index_mode = state.lock().get_settings().indexing.mode;
        let cache_gb = state.lock().get_settings().mounting.cache_size_gb;

        let items = vec![
            format!("Indexing  [{}]", index_mode),
            format!("Mounting  [cache: {:.1} GB]", cache_gb),
            "Exit settings".to_string(),
        ];

        let choice = Select::with_theme(&theme)
            .with_prompt("Settings")
            .items(&items)
            .default(0)
            .interact_opt()?;

        match choice {
            Some(0) => indexing_submenu(state, &theme)?,
            Some(1) => mounting_submenu(state, &theme)?,
            Some(2) | None => break,
            _ => {}
        }
    }

    if let Err(e) = save_settings(state) {
        eprintln!("Warning: Failed to save settings: {}", e);
    }

    Ok(())
}

fn indexing_submenu(state: &Arc<Mutex<ReplState>>, theme: &ColorfulTheme) -> Result<()> {
    let items = vec![
        "IndexOnInit     — index all folders at startup (slower start, instant navigation)",
        "IndexOnDemand   — skip startup indexing, index folders when first visited",
    ];

    let current = state.lock().get_settings().indexing.mode;
    let default = match current {
        IndexMode::IndexOnInit => 0,
        IndexMode::IndexOnDemand => 1,
    };

    let choice = Select::with_theme(theme)
        .with_prompt("Indexing mode")
        .items(&items)
        .default(default)
        .interact_opt()?;

    match choice {
        Some(0) => state.lock().get_settings_mut().indexing.mode = IndexMode::IndexOnInit,
        Some(1) => state.lock().get_settings_mut().indexing.mode = IndexMode::IndexOnDemand,
        _ => {}
    }

    Ok(())
}

fn mounting_submenu(state: &Arc<Mutex<ReplState>>, theme: &ColorfulTheme) -> Result<()> {
    let current = state.lock().get_settings().mounting.cache_size_gb;

    let new_size: String = Input::with_theme(theme)
        .with_prompt("Mount cache size (GB)")
        .default(format!("{:.1}", current))
        .validate_with(|s: &String| {
            s.trim().parse::<f32>()
                .map_err(|_| "Enter a positive number".to_string())
                .and_then(|v| if v > 0.0 { Ok(()) } else { Err("Must be > 0".to_string()) })
        })
        .interact_text()?;

    if let Ok(size) = new_size.trim().parse::<f32>() {
        state.lock().get_settings_mut().mounting.cache_size_gb = size;
    }

    Ok(())
}

fn save_settings(state: &Arc<Mutex<ReplState>>) -> Result<()> {
    let paths = crate::app_paths::resolve_paths()?;
    let s = state.lock();
    s.get_settings().save(&paths.settings_path)?;
    Ok(())
}

pub fn load_settings(paths: &crate::app_paths::AppDataPaths) -> Result<Settings> {
    Settings::load_or_default(&paths.settings_path)
}

