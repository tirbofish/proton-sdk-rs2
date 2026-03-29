use console::style;

use crate::app::AppState;
use crate::settings::{IndexingMethod, Settings};

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
    let method = match s.indexing_method {
        IndexingMethod::IndexOnInit => "on_init",
        IndexingMethod::IndexOnDemand => "on_demand",
    };
    eprintln!("{}", style("Settings").bold().underlined());
    eprintln!(
        "  {} = {}   {}",
        style("indexing_method").cyan(),
        style(method).green(),
        style("(on_init | on_demand)").dim()
    );
    Ok(())
}

fn set_value(args: &[String]) -> anyhow::Result<()> {
    let key = args.first().map(|s| s.as_str()).unwrap_or("");
    let val = args.get(1).map(|s| s.as_str()).unwrap_or("");

    match key {
        "indexing_method" => {
            let method = match val {
                "on_init" => IndexingMethod::IndexOnInit,
                "on_demand" => IndexingMethod::IndexOnDemand,
                _ => anyhow::bail!("indexing_method must be 'on_init' or 'on_demand'"),
            };
            let mut s = Settings::load().unwrap_or_default();
            s.indexing_method = method;
            s.save()?;
            eprintln!(
                "{}  indexing_method = {}",
                style("✓").green().bold(),
                style(val).green()
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
