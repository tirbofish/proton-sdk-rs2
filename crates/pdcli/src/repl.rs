use reedline::{
    ColumnarMenu, Emacs, KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptEditMode,
    PromptHistorySearch, Reedline, ReedlineEvent, ReedlineMenu, Signal,
};
use std::borrow::Cow;
use std::sync::Arc;

use crate::app::AppState;
use crate::commands;
use crate::completer::DriveCompleter;

struct DrivePrompt {
    display: String,
}

impl Prompt for DrivePrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Owned(self.display.clone())
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed(" 〉")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("::: ")
    }

    fn render_prompt_history_search_indicator(&self, _: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Borrowed("? ")
    }
}

fn prompt_display(state: &AppState) -> String {
    format!("{} {}", state.session.username, state.cwd_display())
}

pub async fn repl_loop(state: &mut AppState) -> anyhow::Result<()> {
    use std::sync::mpsc as std_mpsc;
    use tokio::sync::mpsc;

    let (input_tx, mut input_rx) = mpsc::channel::<Signal>(8);
    let (prompt_tx, prompt_rx) = std_mpsc::sync_channel::<String>(1);

    let cwd_shared = Arc::new(parking_lot::RwLock::new(state.cwd.clone()));
    let completer = DriveCompleter::new(
        state.index.clone(),
        cwd_shared.clone(),
        state.devices.clone(),
        state.trash_items.clone(),
    );

    prompt_tx.send(prompt_display(state)).ok();

    std::thread::spawn(move || {
        let completion_menu = Box::new(
            ColumnarMenu::default()
                .with_name("completion_menu")
                .with_columns(1)
                .with_column_width(Some(60)),
        );

        let mut keybindings = reedline::default_emacs_keybindings();
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu("completion_menu".to_string()),
                ReedlineEvent::MenuNext,
            ]),
        );
        let edit_mode = Box::new(Emacs::new(keybindings));

        let mut editor = Reedline::create()
            .with_completer(Box::new(completer))
            .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
            .with_edit_mode(edit_mode);

        loop {
            let display = prompt_rx.recv().unwrap_or_default();
            let prompt = DrivePrompt { display };
            let signal = match editor.read_line(&prompt) {
                Ok(s) => s,
                Err(_) => break,
            };
            let done = matches!(signal, Signal::CtrlD | Signal::CtrlC);
            if input_tx.blocking_send(signal).is_err() || done {
                break;
            }
        }
    });

    while let Some(signal) = input_rx.recv().await {
        match signal {
            Signal::Success(buf) => {
                let line = buf.trim().to_string();
                if !line.is_empty() {
                    tokio::select! {
                        result = commands::dispatch(&line, state) => {
                            if let Err(e) = result {
                                eprintln!("Error: {e:#}");
                            }
                        }
                        _ = tokio::signal::ctrl_c() => {
                            eprintln!();
                        }
                    }
                }
                if state.should_quit {
                    break;
                }
            }
            Signal::CtrlD | Signal::CtrlC => break,
        }
        // Keep shared cwd in sync for the completer.
        {
            let mut w = cwd_shared.write();
            *w = state.cwd.clone();
        }
        prompt_tx.send(prompt_display(state)).ok();
    }

    Ok(())
}
