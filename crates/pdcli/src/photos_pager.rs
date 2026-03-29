use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Local, NaiveDate, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use parking_lot::RwLock;
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::node::photo::PhotosTimelineItem;
use proton_drive_sdk::utils::PotentialObject;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, Paragraph},
    Terminal,
};

use crate::app::AppState;

/// Per-photo data row. `name` / `size` start as None until decrypted.
#[derive(Clone)]
struct PhotoInfo {
    capture_time: DateTime<Utc>,
    name: Option<String>,
    size: Option<i64>,
}

/// A pre-built display row — stable once the timeline is fetched.
enum DisplayRow {
    /// A date header separating groups.
    DateHeader(NaiveDate),
    /// Reference into the `infos` array for a single photo.
    Photo { idx: usize, capture_time: DateTime<Utc> },
}

/// Show the scrollable pager for the full photo timeline (`/Photos/All`).
pub async fn show_photos_pager(state: &AppState) -> anyhow::Result<()> {
    let pb = crate::ui::spinner("Loading photo timeline…");
    let timeline_items = tokio::select! {
        result = state.photos.iterate_timeline() => {
            pb.finish_and_clear();
            result?
        }
        _ = tokio::signal::ctrl_c() => {
            pb.finish_and_clear();
            return Ok(());
        }
    };
    if timeline_items.is_empty() {
        eprintln!("{}", console::style("(no photos)").dim());
        return Ok(());
    }
    run_pager_with_items(state, timeline_items, None).await
}

/// Show the scrollable pager for a single album (`/Photos/<Album Name>`).
pub async fn show_album_pager(state: &AppState, album_uid: NodeUid) -> anyhow::Result<()> {
    let album_name = state.index.get(&album_uid)
        .map(|e| e.name)
        .unwrap_or_else(|| "Album".to_string());
    let forced = album_uid.clone();
    let pb = crate::ui::spinner(format!("Loading \"{}\"…", album_name));
    let raw_items = tokio::select! {
        result = state.photos.iterate_album(album_uid) => {
            pb.finish_and_clear();
            result?
        }
        _ = tokio::signal::ctrl_c() => {
            pb.finish_and_clear();
            return Ok(());
        }
    };
    if raw_items.is_empty() {
        eprintln!("{}", console::style("(empty album)").dim());
        return Ok(());
    }
    let items: Vec<PhotosTimelineItem> = raw_items
        .into_iter()
        .map(|i| PhotosTimelineItem { uid: i.uid, capture_time: i.capture_time })
        .collect();
    run_pager_with_items(state, items, Some(forced)).await
}

/// Shared pager core — takes pre-fetched items, shows TUI.
async fn run_pager_with_items(
    state: &AppState,
    timeline_items: Vec<PhotosTimelineItem>,
    forced_parent: Option<NodeUid>,
) -> anyhow::Result<()> {
    let total = timeline_items.len();

    // Pre-populate from index — items already in SQLite cache show names immediately.
    let infos: Arc<RwLock<Vec<PhotoInfo>>> = Arc::new(RwLock::new(
        timeline_items
            .iter()
            .map(|i| {
                if let Some(entry) = state.index.get(&i.uid) {
                    PhotoInfo {
                        capture_time: i.capture_time,
                        name: Some(entry.name),
                        size: entry.size,
                    }
                } else {
                    PhotoInfo { capture_time: i.capture_time, name: None, size: None }
                }
            })
            .collect(),
    ));

    // Collect UIDs that still need decryption.
    let uncached_uids: Vec<NodeUid> = {
        let locked = infos.read();
        timeline_items
            .iter()
            .enumerate()
            .filter(|(i, _)| locked[*i].name.is_none())
            .map(|(_, item)| item.uid.clone())
            .collect()
    };

    let uid_to_idx: Arc<HashMap<NodeUid, usize>> = Arc::new(
        timeline_items.iter().enumerate().map(|(i, item)| (item.uid.clone(), i)).collect(),
    );

    let display_rows: Arc<Vec<DisplayRow>> = Arc::new({
        let locked = infos.read();
        build_display_rows(&locked)
    });

    // Start the decrypted counter at however many we already have cached.
    let already_done = total - uncached_uids.len();
    let (notify_tx, notify_rx) = tokio::sync::watch::channel(already_done);
    let warnings: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(Vec::new()));

    {
        let photos_client = state.photos.clone();
        let index = state.index.clone();
        let infos_clone = infos.clone();
        let map = uid_to_idx;
        let notify = notify_tx;
        let warnings_clone = warnings.clone();

        tokio::spawn(async move {
            const BATCH: usize = 50;
            let mut done = already_done;

            'outer: for chunk in uncached_uids.chunks(BATCH) {
                let chunk_vec = chunk.to_vec();
                loop {
                    match photos_client.enumerate_nodes(chunk_vec.clone()).await {
                        Ok(nodes) => {
                            let mut locked = infos_clone.write();
                            for node in &nodes {
                                let uid = match node {
                                    PotentialObject::Node(n) => n.uid().clone(),
                                    PotentialObject::Degraded(d) => d.uid().clone(),
                                };
                                if let Some(&idx) = map.get(&uid) {
                                    if let Some(row) = locked.get_mut(idx) {
                                        match node {
                                            PotentialObject::Node(
                                                Node::File(f) | Node::Photo(f),
                                            ) => {
                                                row.name = Some(f.base.base.name.clone());
                                                row.size = f.active_revision.claimed_size;
                                            }
                                            _ => {
                                                row.name = Some("<unknown>".to_string());
                                            }
                                        }
                                    }
                                }
                                // Persist to SQLite so next launch is instant.
                                if let Some(fp) = &forced_parent {
                                    index.insert_node_force_parent(node, fp.clone());
                                } else {
                                    index.insert_node(node, None);
                                }
                            }
                            done += chunk.len();
                            let _ = notify.send(done);
                            break;
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            if msg.contains("429")
                                || msg.to_lowercase().contains("rate limit")
                                || msg.contains("Too Many")
                            {
                                {
                                    let mut w = warnings_clone.write();
                                    w.retain(|x| !x.starts_with("⚠ Rate limit"));
                                    w.push(format!(
                                        "⚠ Rate limit hit — backing off 10 s  ({done}/{total} decrypted so far)"
                                    ));
                                }
                                let _ = notify.send(done);
                                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                                warnings_clone
                                    .write()
                                    .retain(|x| !x.starts_with("⚠ Rate limit"));
                            } else {
                                warnings_clone
                                    .write()
                                    .push(format!("⚠ Decryption error: {e}"));
                                let _ = notify.send(done);
                                break 'outer;
                            }
                        }
                    }
                }
            }
        });
    }

    tokio::task::block_in_place(|| {
        run_pager_tui(infos, display_rows, warnings, total, notify_rx)
    })
}

// ── Display row builder ──────────────────────────────────────────────────────

fn build_display_rows(infos: &[PhotoInfo]) -> Vec<DisplayRow> {
    let mut rows = Vec::with_capacity(infos.len() + 64);
    let mut last_date: Option<NaiveDate> = None;
    for (idx, info) in infos.iter().enumerate() {
        let local_date = info.capture_time.with_timezone(&Local).date_naive();
        if last_date != Some(local_date) {
            rows.push(DisplayRow::DateHeader(local_date));
            last_date = Some(local_date);
        }
        rows.push(DisplayRow::Photo { idx, capture_time: info.capture_time });
    }
    rows
}

// ── TUI setup / teardown ────────────────────────────────────────────────────

fn run_pager_tui(
    infos: Arc<RwLock<Vec<PhotoInfo>>>,
    display_rows: Arc<Vec<DisplayRow>>,
    warnings: Arc<RwLock<Vec<String>>>,
    total: usize,
    notify_rx: tokio::sync::watch::Receiver<usize>,
) -> anyhow::Result<()> {
    enable_raw_mode()?;
    execute!(std::io::stderr(), EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(std::io::stderr());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let row_count = display_rows.len();
    let mut scroll: usize = 0;
    let mut cursor: usize = first_photo_row(&display_rows);
    // Cell so the draw closure can write back the rendered visible height.
    let visible_height = Cell::new(10usize);

    let result = (|| -> anyhow::Result<()> {
        loop {
            let decrypted = *notify_rx.borrow();
            let warn_text: Option<String> = warnings.read().last().cloned();
            let has_warning = warn_text.is_some();
            let cur_snap = cursor;

            terminal.draw(|frame| {
                let area = frame.area();

                let constraints: Vec<Constraint> = if has_warning {
                    vec![Constraint::Length(1), Constraint::Length(1), Constraint::Min(0)]
                } else {
                    vec![Constraint::Length(1), Constraint::Min(0)]
                };
                let chunks = Layout::new(Direction::Vertical, constraints).split(area);

                let (warn_chunk, info_chunk, list_chunk) = if has_warning {
                    (Some(chunks[0]), chunks[1], chunks[2])
                } else {
                    (None, chunks[0], chunks[1])
                };

                if let (Some(wc), Some(w)) = (warn_chunk, &warn_text) {
                    frame.render_widget(
                        Paragraph::new(w.as_str()).style(
                            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        ),
                        wc,
                    );
                }

                let status = format!(
                    " {} total  {} decrypted  ↑↓ j k  PgUp PgDn  Home End  q/Ctrl+C quit",
                    total, decrypted
                );
                frame.render_widget(
                    Paragraph::new(status.as_str()).style(Style::default().fg(Color::DarkGray)),
                    info_chunk,
                );

                let vh = list_chunk.height as usize;
                visible_height.set(vh);

                let clamped = scroll.min(row_count.saturating_sub(1));
                let end = (clamped + vh).min(row_count);
                let visible = &display_rows[clamped..end];

                let terminal_width = area.width as usize;
                let infos_locked = infos.read();

                let items: Vec<ListItem> = visible
                    .iter()
                    .enumerate()
                    .map(|(vis_i, row)| {
                        let abs_row = clamped + vis_i;
                        let is_cursor = abs_row == cur_snap;
                        match row {
                            DisplayRow::DateHeader(date) => {
                                let label = date.format("%A, %d %B %Y").to_string();
                                let dashes = "─".repeat(
                                    terminal_width.saturating_sub(label.len() + 5),
                                );
                                ListItem::new(Line::from(Span::styled(
                                    format!("── {} {}", label, dashes),
                                    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
                                )))
                            }
                            DisplayRow::Photo { idx, capture_time } => {
                                let time_str =
                                    capture_time.with_timezone(&Local).format("%H:%M").to_string();
                                if let Some(info) = infos_locked.get(*idx) {
                                    let pending = info.name.is_none();
                                    let name = info.name.as_deref().unwrap_or("…");
                                    let size = info.size.map(format_size).unwrap_or_default();
                                    let style = if is_cursor {
                                        Style::default()
                                            .bg(Color::Blue)
                                            .fg(Color::White)
                                            .add_modifier(Modifier::BOLD)
                                    } else if pending {
                                        Style::default().fg(Color::DarkGray)
                                    } else {
                                        Style::default()
                                    };
                                    let name_col = terminal_width.saturating_sub(5 + 4 + 12);
                                    ListItem::new(Line::from(Span::styled(
                                        format!(
                                            "  {}  {:<nw$}  {}",
                                            time_str,
                                            name,
                                            size,
                                            nw = name_col
                                        ),
                                        style,
                                    )))
                                } else {
                                    ListItem::new("")
                                }
                            }
                        }
                    })
                    .collect();

                frame.render_widget(List::new(items), list_chunk);
            })?;

            let vh = visible_height.get().max(1);

            if event::poll(std::time::Duration::from_millis(80))? {
                if let Event::Key(key) = event::read()? {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c')
                            if key.modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            return Ok(());
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            cursor = prev_photo_row(&display_rows, cursor);
                            ensure_cursor_visible(cursor, &mut scroll, vh);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            cursor = next_photo_row(&display_rows, cursor);
                            ensure_cursor_visible(cursor, &mut scroll, vh);
                        }
                        KeyCode::PageUp => {
                            for _ in 0..vh.saturating_sub(1) {
                                let p = prev_photo_row(&display_rows, cursor);
                                if p == cursor { break; }
                                cursor = p;
                            }
                            ensure_cursor_visible(cursor, &mut scroll, vh);
                        }
                        KeyCode::PageDown => {
                            for _ in 0..vh.saturating_sub(1) {
                                let n = next_photo_row(&display_rows, cursor);
                                if n == cursor { break; }
                                cursor = n;
                            }
                            ensure_cursor_visible(cursor, &mut scroll, vh);
                        }
                        KeyCode::Home => {
                            cursor = first_photo_row(&display_rows);
                            scroll = 0;
                        }
                        KeyCode::End => {
                            cursor = last_photo_row(&display_rows);
                            ensure_cursor_visible(cursor, &mut scroll, vh);
                        }
                        _ => {}
                    }
                }
            }
        }
    })();

    // Always restore terminal, even on error.
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stderr(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    result
}

// ── Cursor navigation helpers ────────────────────────────────────────────────

fn first_photo_row(rows: &[DisplayRow]) -> usize {
    rows.iter().position(|r| matches!(r, DisplayRow::Photo { .. })).unwrap_or(0)
}

fn last_photo_row(rows: &[DisplayRow]) -> usize {
    rows.iter().rposition(|r| matches!(r, DisplayRow::Photo { .. })).unwrap_or(0)
}

fn prev_photo_row(rows: &[DisplayRow], from: usize) -> usize {
    let mut i = from;
    while i > 0 {
        i -= 1;
        if matches!(rows[i], DisplayRow::Photo { .. }) {
            return i;
        }
    }
    from
}

fn next_photo_row(rows: &[DisplayRow], from: usize) -> usize {
    let mut i = from + 1;
    while i < rows.len() {
        if matches!(rows[i], DisplayRow::Photo { .. }) {
            return i;
        }
        i += 1;
    }
    from
}

fn ensure_cursor_visible(cursor: usize, scroll: &mut usize, visible_height: usize) {
    if cursor < *scroll {
        *scroll = cursor;
    } else if cursor >= *scroll + visible_height {
        *scroll = cursor.saturating_sub(visible_height - 1);
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn format_size(bytes: i64) -> String {
    let b = bytes as f64;
    if b < 1024.0 {
        format!("{b:.0} B")
    } else if b < 1_048_576.0 {
        format!("{:.1} KB", b / 1024.0)
    } else if b < 1_073_741_824.0 {
        format!("{:.1} MB", b / 1_048_576.0)
    } else {
        format!("{:.2} GB", b / 1_073_741_824.0)
    }
}
