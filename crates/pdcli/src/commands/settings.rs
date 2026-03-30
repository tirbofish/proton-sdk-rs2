use console::style;

use crate::app::AppState;
use crate::settings::Settings;

pub async fn settings_cmd(args: &[String], state: &AppState) -> anyhow::Result<()> {
    match args.first().map(|s| s.as_str()) {
        None | Some("show") => show(state),
        Some("set") => set_value(&args[1..]),
        Some(other) => {
            eprintln!("Unknown settings sub-command: '{other}'. Use: settings [show] | settings set <key> <value>");
            Ok(())
        }
    }
}

fn show(state: &AppState) -> anyhow::Result<()> {
    let s = &state.settings;
    eprintln!("{}", style("Settings").bold().underlined());
    eprintln!(
        "  {} = {}   {}",
        style("entity_cache_max_size").cyan(),
        style(s.entity_cache_max_size.map_or("unlimited".to_string(), |n| n.to_string())).green(),
        style("(positive integer, or unset for unlimited)").dim()
    );
    eprintln!(
        "  {} = {}   {}",
        style("secret_cache_max_size").cyan(),
        style(s.secret_cache_max_size.map_or("unlimited".to_string(), |n| n.to_string())).green(),
        style("(positive integer, or unset for unlimited)").dim()
    );
    Ok(())
}

fn set_value(args: &[String]) -> anyhow::Result<()> {
    let key = args.first().map(|s| s.as_str()).unwrap_or("");
    let val = args.get(1).map(|s| s.as_str()).unwrap_or("");

    match key {
        "entity_cache_max_size" | "secret_cache_max_size" => {
            let parsed: Option<usize> = if val.is_empty() || val == "unlimited" || val == "none" {
                None
            } else {
                let n: usize = val.parse().map_err(|_| anyhow::anyhow!("{key} must be a positive integer or 'unlimited'"))?;
                anyhow::ensure!(n > 0, "{key} must be greater than 0");
                Some(n)
            };
            let mut s = Settings::load().unwrap_or_default();
            if key == "entity_cache_max_size" {
                s.entity_cache_max_size = parsed;
            } else {
                s.secret_cache_max_size = parsed;
            }
            s.save()?;
            let display = parsed.map_or("unlimited".to_string(), |n| n.to_string());
            eprintln!(
                "{}  {} = {}",
                style("✓").green().bold(),
                style(key).cyan(),
                style(&display).green(),
            );
            eprintln!("{}", style("Restart pdcli for the change to take effect.").dim());
        }
        "" => {
            eprintln!("Usage: settings set <key> <value>");
        }
        _ => {
            eprintln!("Unknown setting: '{key}'");
        }
    }
    Ok(())
}
