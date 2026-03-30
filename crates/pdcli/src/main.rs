mod app;
mod app_paths;
mod auth;
mod commands;
mod completer;
mod db;
mod events;
mod index;
mod photos_pager;
mod repl;
mod settings;
mod ui;
mod vfs;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                // pgp crate warns on packet header size mismatches during re-serialization;
                // these are cosmetic (the key material is intact).
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(
                    "warn,pgp::packet::packet_sum=off",
                )),
        )
        .init();

    let session = auth::authenticate().await?;
    let mut state = app::AppState::new(session).await?;
    state.spawn_background_tasks();

    // One-time CLI mode: `pdcli <section> <command> [args...]`
    // e.g. `pdcli trash drop * --force` or `pdcli myfiles ls`
    let cli_args: Vec<String> = std::env::args().skip(1).collect();
    if !cli_args.is_empty() {
        let section_arg = cli_args[0].to_lowercase();
        let target_path = match section_arg.as_str() {
            "trash"     => "/Trash",
            "computers" => "/Computers",
            "photos"    => "/Photos",
            "myfiles" | "my-files" | "drive" => "/MyFiles",
            other => {
                eprintln!("pdcli: unknown section '{}'. Use: trash | computers | photos | myfiles", other);
                std::process::exit(1);
            }
        };
        commands::dispatch(&format!("cd {}", target_path), &mut state).await?;
        let cmd_line = cli_args[1..].join(" ");
        if !cmd_line.is_empty() {
            commands::dispatch(&cmd_line, &mut state).await?;
        }
        return Ok(());
    }

    repl::repl_loop(&mut state).await
}
