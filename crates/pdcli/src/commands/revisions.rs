use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use proton_drive_sdk::node::revision::{RevisionInfo, RevisionState, RevisionUid};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
    Terminal,
};
use std::io;

use crate::app::AppState;
use crate::vfs::VirtualPath;

pub async fn rev(args: &[String], state: &mut AppState) -> anyhow::Result<()> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "ls" => rev_ls(&args[1..], state).await,
        "restore" => {
            let uid_str = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("rev restore: missing revision UID"))?;
            let uid = RevisionUid::parse(uid_str)
                .map_err(|e| anyhow::anyhow!("Invalid revision UID: {e}"))?;
            let pb = crate::ui::spinner("Restoring revision…");
            let r = state.drive.restore_revision(uid).await;
            pb.finish_and_clear();
            r?;
            eprintln!("Revision restored");
            Ok(())
        }
        "delete" => {
            let uid_str = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("rev delete: missing revision UID"))?;
            let uid = RevisionUid::parse(uid_str)
                .map_err(|e| anyhow::anyhow!("Invalid revision UID: {e}"))?;
            let pb = crate::ui::spinner("Deleting revision…");
            let r = state.drive.delete_revision(uid).await;
            pb.finish_and_clear();
            r?;
            eprintln!("Revision deleted");
            Ok(())
        }
        other => {
            eprintln!("Unknown rev sub-command: '{other}'. Use: rev ls | rev restore <uid> | rev delete <uid>");
            Ok(())
        }
    }
}

async fn rev_ls(args: &[String], state: &AppState) -> anyhow::Result<()> {
    let path_arg = args.first().map(|s| s.as_str());
    let target = match path_arg {
        Some(p) => VirtualPath::resolve(&state.cwd, p),
        None => state.cwd.clone(),
    };

    let uid = state
        .resolve_uid(&target)
        .await?
        .ok_or_else(|| anyhow::anyhow!("rev ls: path not found: {}", target))?;

    let entry = state.index.get(&uid);
    if entry.map(|e| e.is_folder).unwrap_or(false) {
        anyhow::bail!("rev ls: '{}' is a directory — specify a file path", target);
    }

    let pb = crate::ui::spinner(format!("Loading revisions for '{}'…", target));
    let revisions = match state.drive.iterate_revisions(uid).await {
        Ok(v) => { pb.finish_and_clear(); v }
        Err(e) => { pb.finish_and_clear(); return Err(e); }
    };
    if revisions.is_empty() {
        eprintln!("No revisions found for '{}'", target);
        return Ok(());
    }

    run_revision_pager(state, &revisions).await
}

/// Restores terminal state even when the pager future is cancelled (Ctrl+C).
struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
    }
}

async fn run_revision_pager(state: &AppState, revisions: &[RevisionInfo]) -> anyhow::Result<()> {
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    pager_loop(state, revisions, &mut terminal).await
    // _guard drops here, restoring terminal
}

async fn pager_loop(
    state: &AppState,
    revisions: &[RevisionInfo],
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> anyhow::Result<()> {
    let mut list_state = ListState::default();
    list_state.select(Some(0));
    let mut status_msg: Option<String> = None;

    loop {
        let sel = list_state.selected().unwrap_or(0);
        let status = status_msg.clone().unwrap_or_else(|| {
            format!(
                "{}/{}  •  Enter=restore superseded  D=delete superseded",
                sel + 1,
                revisions.len()
            )
        });

        terminal.draw(|f| {
            let area = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(area);

            let header = Paragraph::new(
                "Revisions   ↑/↓ navigate   Enter=restore   D=delete   Esc/q=exit",
            )
            .style(Style::default().add_modifier(Modifier::BOLD));
            f.render_widget(header, chunks[0]);

            let items: Vec<ListItem> = revisions
                .iter()
                .enumerate()
                .map(|(i, rev)| {
                    let (state_str, state_style) = match rev.state {
                        RevisionState::Active => (
                            "  active  ",
                            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                        ),
                        RevisionState::Superseded => (
                            "superseded",
                            Style::default().fg(Color::DarkGray),
                        ),
                        RevisionState::Draft => (
                            "  draft   ",
                            Style::default().fg(Color::Yellow),
                        ),
                    };
                    let size = rev
                        .claimed_size
                        .map(format_size)
                        .unwrap_or_else(|| "?".to_string());
                    let date = rev.creation_time.format("%Y-%m-%d %H:%M:%S");
                    let uid_s = rev.uid.to_string();
                    let uid_short = format!("…{}", &uid_s[uid_s.len().saturating_sub(16)..]);
                    let hint = if rev.state == RevisionState::Active && i == sel {
                        " (cannot restore — active)"
                    } else {
                        ""
                    };

                    ListItem::new(Line::from(vec![
                        Span::raw(format!("{:>2}  ", i + 1)),
                        Span::styled(state_str, state_style),
                        Span::raw(format!("  {}  {:>10}  ", date, size)),
                        Span::styled(uid_short, Style::default().fg(Color::DarkGray)),
                        Span::styled(hint, Style::default().fg(Color::DarkGray)),
                    ]))
                })
                .collect();

            let list = List::new(items)
                .highlight_symbol("▶ ")
                .highlight_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );

            let mut render_state = ListState::default();
            render_state.select(Some(sel));
            f.render_stateful_widget(list, chunks[1], &mut render_state);

            let status_para = Paragraph::new(status.as_str())
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(status_para, chunks[2]);
        })?;

        status_msg = None;

        let ev = event::read()?;
        match ev {
            Event::Key(KeyEvent { code: KeyCode::Esc, .. })
            | Event::Key(KeyEvent { code: KeyCode::Char('q'), .. }) => break,

            Event::Key(KeyEvent { code: KeyCode::Up, .. }) => {
                if sel > 0 {
                    list_state.select(Some(sel - 1));
                }
            }
            Event::Key(KeyEvent { code: KeyCode::Down, .. }) => {
                if sel + 1 < revisions.len() {
                    list_state.select(Some(sel + 1));
                }
            }

            Event::Key(KeyEvent { code: KeyCode::Enter, .. }) => {
                if revisions[sel].state == RevisionState::Active {
                    status_msg = Some(
                        "Cannot restore the active revision — select a superseded one".to_string(),
                    );
                    continue;
                }
                let uid = revisions[sel].uid.clone();
                terminal::disable_raw_mode()?;
                execute!(io::stdout(), LeaveAlternateScreen, cursor::Show)?;
                let pb = crate::ui::spinner("Restoring revision…");
                let r = state.drive.restore_revision(uid).await;
                pb.finish_and_clear();
                match r {
                    Ok(_) => {
                        eprintln!("Revision restored");
                        return Ok(());
                    }
                    Err(e) => eprintln!("Failed to restore: {e}"),
                }
                terminal::enable_raw_mode()?;
                execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
                terminal.clear()?;
            }

            Event::Key(KeyEvent {
                code: KeyCode::Char('d') | KeyCode::Delete,
                ..
            }) => {
                if revisions[sel].state == RevisionState::Active {
                    status_msg = Some("Cannot delete the active revision".to_string());
                    continue;
                }
                let uid = revisions[sel].uid.clone();
                terminal::disable_raw_mode()?;
                execute!(io::stdout(), LeaveAlternateScreen, cursor::Show)?;
                let pb = crate::ui::spinner("Deleting revision…");
                let r = state.drive.delete_revision(uid).await;
                pb.finish_and_clear();
                match r {
                    Ok(_) => {
                        eprintln!("Revision deleted");
                        return Ok(());
                    }
                    Err(e) => eprintln!("Failed to delete: {e}"),
                }
                terminal::enable_raw_mode()?;
                execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
                terminal.clear()?;
            }

            _ => {}
        }
    }
    Ok(())
}

fn format_size(bytes: i64) -> String {
    let b = bytes as f64;
    if b < 1024.0 { format!("{b:.0} B") }
    else if b < 1024.0 * 1024.0 { format!("{:.1} KB", b / 1024.0) }
    else if b < 1024.0 * 1024.0 * 1024.0 { format!("{:.1} MB", b / (1024.0 * 1024.0)) }
    else { format!("{:.2} GB", b / (1024.0 * 1024.0 * 1024.0)) }
}
