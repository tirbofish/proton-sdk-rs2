#[cfg(target_os = "linux")]
#[path = "../credentials.rs"]
mod credentials;
#[cfg(target_os = "linux")]
#[path = "../pdignore.rs"]
mod pdignore;

#[cfg(target_os = "linux")]
mod desktop {
    use super::{credentials, pdignore};
    use clap::Parser;
    use gtk4::prelude::*;
    use libadwaita as adw;
    use libadwaita::prelude::*;
    use std::{
        cell::Cell,
        io::{BufRead, BufReader},
        process::{Command, Stdio},
        rc::Rc,
        sync::{
            OnceLock,
            mpsc::{self, Sender},
        },
        time::Duration,
    };

    #[derive(Parser)]
    struct Options {
        #[arg(long, value_parser = ["status", "computers", "mount", "about", "account", "settings"])]
        page: Option<String>,
        #[arg(long)]
        force_offline: bool,
        #[arg(long)]
        no_tray: bool,
    }

    static CLI_FLAGS: OnceLock<Vec<&'static str>> = OnceLock::new();

    enum Event {
        Account(Option<String>),
        LoginLine(String),
        Done(String, Result<String, String>),
        IgnoreLoaded(String),
    }

    fn run_command(args: &[String]) -> Result<String, String> {
        let binary = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .with_file_name("pdcli");
        let output = Command::new(binary)
            .args(CLI_FLAGS.get().expect("CLI flags initialized"))
            .args(args)
            .output()
            .map_err(|e| format!("Could not start pdcli: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if output.status.success() {
            Ok(stdout)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("{} {}", stdout, stderr.trim()).trim().to_owned())
        }
    }

    fn command(tx: &Sender<Event>, action: &str, args: Vec<String>) {
        let tx = tx.clone();
        let action = action.to_owned();
        std::thread::spawn(move || {
            let result = run_command(&args);
            let _ = tx.send(Event::Done(action, result));
        });
    }

    fn button(label: &str, box_: &gtk4::Box, callback: impl Fn() + 'static) -> gtk4::Button {
        let button = gtk4::Button::with_label(label);
        button.connect_clicked(move |_| callback());
        box_.append(&button);
        button
    }

    fn row() -> gtk4::Box {
        gtk4::Box::new(gtk4::Orientation::Horizontal, 8)
    }

    fn page(title: &str) -> gtk4::Box {
        let box_ = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        box_.set_margin_start(24);
        box_.set_margin_end(24);
        box_.set_margin_top(24);
        box_.set_margin_bottom(24);
        let heading = gtk4::Label::new(Some(title));
        heading.add_css_class("title-1");
        heading.set_halign(gtk4::Align::Start);
        box_.append(&heading);
        box_
    }

    fn field(placeholder: &str, box_: &gtk4::Box) -> gtk4::Entry {
        let entry = gtk4::Entry::new();
        entry.set_placeholder_text(Some(placeholder));
        entry.set_hexpand(true);
        box_.append(&entry);
        entry
    }

    fn scroll(widget: &impl IsA<gtk4::Widget>) -> gtk4::ScrolledWindow {
        let scrolled = gtk4::ScrolledWindow::new();
        scrolled.set_child(Some(widget));
        scrolled.set_vexpand(true);
        scrolled
    }

    fn login(tx: &Sender<Event>) {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let binary = match std::env::current_exe() {
                Ok(path) => path.with_file_name("pdcli"),
                Err(e) => {
                    let _ = tx.send(Event::Done("Sign in".into(), Err(e.to_string())));
                    return;
                }
            };
            let mut child = match Command::new(binary)
                .args(CLI_FLAGS.get().expect("CLI flags initialized"))
                .arg("login")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    let _ = tx.send(Event::Done("Sign in".into(), Err(e.to_string())));
                    return;
                }
            };
            if let Some(stdout) = child.stdout.take() {
                for line in BufReader::new(stdout).lines() {
                    if let Ok(line) = line {
                        let _ = tx.send(Event::LoginLine(line));
                    }
                }
            }
            let result = child
                .wait_with_output()
                .map_err(|e| e.to_string())
                .and_then(|out| {
                    if out.status.success() {
                        Ok("Signed in".into())
                    } else {
                        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
                    }
                });
            let _ = tx.send(Event::Done("Sign in".into(), result));
        });
    }

    pub fn main() {
        let options = Options::parse();
        CLI_FLAGS
            .set(
                [
                    options.force_offline.then_some("--force-offline"),
                    options.no_tray.then_some("--no-tray"),
                ]
                .into_iter()
                .flatten()
                .collect(),
            )
            .expect("CLI flags initialized once");
        let app = adw::Application::builder()
            .application_id("io.github.tirbofish.pdcli")
            .build();
        app.connect_activate(move |app| build(app, options.page.as_deref()));
        app.run_with_args(&[] as &[&str]);
    }

    fn build(app: &adw::Application, initial_page: Option<&str>) {
        let (tx, rx) = mpsc::channel::<Event>();
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("pdcli")
            .default_width(920)
            .default_height(660)
            .build();
        let root = gtk4::Stack::new();
        let sign_in = page("Sign in to Proton Drive");
        sign_in.append(&gtk4::Label::new(Some(
            "pdcli is an unofficial third-party Proton Drive client.",
        )));
        sign_in.append(&gtk4::Label::new(Some(
            "Sign in securely in your browser to continue.",
        )));
        let login_details = gtk4::Label::new(Some("Checking for a saved account…"));
        login_details.set_selectable(true);
        login_details.set_wrap(true);
        sign_in.append(&login_details);
        let login_button = button("Sign in with browser", &sign_in, {
            let tx = tx.clone();
            let details = login_details.clone();
            move || {
                details.set_text("Waiting for browser sign-in…");
                login(&tx);
            }
        });
        let login_link = gtk4::LinkButton::new("https://account.proton.me");
        login_link.set_label("Open sign-in page");
        login_link.set_visible(false);
        sign_in.append(&login_link);
        login_button.add_css_class("suggested-action");
        login_button.connect_clicked(|button| button.set_sensitive(false));
        login_button.set_sensitive(false);
        root.add_named(&sign_in, Some("login"));

        let pages = gtk4::Stack::new();
        pages.set_hexpand(true);
        pages.set_vexpand(true);
        let sidebar = gtk4::StackSidebar::new();
        sidebar.set_stack(&pages);
        sidebar.set_size_request(180, -1);
        let main = row();
        main.append(&sidebar);
        main.append(&pages);
        root.add_named(&main, Some("main"));
        root.set_visible_child_name("login");

        let status = page("Status");
        let status_text = gtk4::Label::new(Some("Checking mount and sync status…"));
        status_text.set_halign(gtk4::Align::Start);
        status_text.set_selectable(true);
        status.append(&status_text);
        let status_actions = row();
        for (label, action, args) in [
            ("Mount", "Mount", vec!["mount"]),
            ("Open folder", "Open", vec!["open"]),
            ("Pause sync", "Pause", vec!["pause"]),
            ("Resume sync", "Resume", vec!["resume"]),
            ("Retry now", "Retry", vec!["sync"]),
        ] {
            let tx = tx.clone();
            button(label, &status_actions, move || {
                command(&tx, action, args.iter().map(|s| s.to_string()).collect());
            });
        }
        status.append(&status_actions);
        let activity = gtk4::Label::new(Some(""));
        activity.set_wrap(true);
        activity.set_selectable(true);
        activity.set_halign(gtk4::Align::Start);
        pages.add_titled(&scroll(&status), Some("status"), "Status");

        let mount = page("Mount");
        let mount_text = gtk4::Label::new(Some("Checking mount status…"));
        mount_text.set_halign(gtk4::Align::Start);
        mount_text.set_selectable(true);
        mount.append(&mount_text);
        let mount_actions = row();
        for (label, action, argument) in [
            ("Mount Proton Drive", "Mount", "mount"),
            ("Open folder", "Open", "open"),
            ("Stop mount", "Stop", "stop"),
        ] {
            let tx = tx.clone();
            button(label, &mount_actions, move || {
                command(&tx, action, vec![argument.into()]);
            });
        }
        mount.append(&mount_actions);
        pages.add_titled(&mount, Some("mount"), "Mount");

        let computers = page("Computers");
        computers.append(&gtk4::Label::new(Some(
            "Register this computer or bind an existing device ID.",
        )));
        let register_row = row();
        let bind = field("Existing device ID (optional)", &register_row);
        button("Register / bind", &register_row, {
            let tx = tx.clone();
            move || {
                let mut args = vec!["computers".into(), "register".into()];
                if !bind.text().is_empty() {
                    args.extend(["--bind".into(), bind.text().to_string()]);
                }
                command(&tx, "Register computer", args);
            }
        });
        computers.append(&register_row);
        computers.append(&gtk4::Label::new(Some(
            "Back up a local folder and keep it in sync.",
        )));
        let backup_row = row();
        let backup_path = field("Local folder path", &backup_row);
        button("Add backup", &backup_row, {
            let tx = tx.clone();
            move || {
                if !backup_path.text().is_empty() {
                    command(
                        &tx,
                        "Add backup",
                        vec![
                            "computers".into(),
                            "sync".into(),
                            backup_path.text().to_string(),
                        ],
                    );
                }
            }
        });
        computers.append(&backup_row);
        let remove_row = row();
        let job = field("Sync job name or ID", &remove_row);
        button("Unsync (keeps files)", &remove_row, {
            let tx = tx.clone();
            move || {
                if !job.text().is_empty() {
                    command(
                        &tx,
                        "Unsync",
                        vec!["computers".into(), "unsync".into(), job.text().to_string()],
                    );
                }
            }
        });
        computers.append(&remove_row);
        computers.append(&gtk4::Label::new(Some(
            "Restore a backed-up folder to a local path and continue syncing.",
        )));
        let restore_row = row();
        let computer = field("Computer name or ID", &restore_row);
        let folder = field("Folder name", &restore_row);
        let path = field("Local destination", &restore_row);
        button("Restore", &restore_row, {
            let tx = tx.clone();
            move || {
                if !computer.text().is_empty()
                    && !folder.text().is_empty()
                    && !path.text().is_empty()
                {
                    command(
                        &tx,
                        "Restore",
                        vec![
                            "computers".into(),
                            "restore".into(),
                            computer.text().to_string(),
                            folder.text().to_string(),
                            path.text().to_string(),
                        ],
                    );
                }
            }
        });
        computers.append(&restore_row);
        let listing = gtk4::Label::new(Some("Loading computers…"));
        listing.set_halign(gtk4::Align::Start);
        listing.set_selectable(true);
        computers.append(&listing);
        button("Refresh computers", &computers, {
            let tx = tx.clone();
            move || command(&tx, "Computers", vec!["computers".into()])
        });
        pages.add_titled(&scroll(&computers), Some("computers"), "Computers");

        let account = page("Account");
        let username = gtk4::Label::new(Some(""));
        account.append(&username);
        button("Sign out", &account, {
            let tx = tx.clone();
            move || command(&tx, "Sign out", vec!["logout".into()])
        });
        pages.add_titled(&account, Some("account"), "Account");

        let settings = page("Settings");
        settings.append(&gtk4::Label::new(Some("Global .pdignore")));
        let path_label = gtk4::Label::new(Some(&pdignore::global_path().display().to_string()));
        path_label.set_selectable(true);
        path_label.set_halign(gtk4::Align::Start);
        settings.append(&path_label);
        let editor = gtk4::TextView::new();
        editor.set_monospace(true);
        editor.set_vexpand(true);
        settings.append(&scroll(&editor));
        let settings_actions = row();
        button("Save", &settings_actions, {
            let tx = tx.clone();
            let buffer = editor.buffer();
            move || {
                let text = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let result = pdignore::save_global_text(&text)
                        .map(|_| "Saved global .pdignore".into())
                        .map_err(|e| e.to_string());
                    let _ = tx.send(Event::Done("Settings".into(), result));
                });
            }
        });
        button("Reload", &settings_actions, {
            let tx = tx.clone();
            move || {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(Event::IgnoreLoaded(pdignore::load_global_text()));
                });
            }
        });
        button("Reset defaults", &settings_actions, {
            let buffer = editor.buffer();
            move || buffer.set_text(pdignore::DEFAULT_GLOBAL_PDIGNORE)
        });
        settings.append(&settings_actions);
        pages.add_titled(&settings, Some("settings"), "Settings");

        let about = page("About");
        about.append(&gtk4::Label::new(Some(&format!(
            "pdcli {} — unofficial Proton Drive client for Linux",
            env!("CARGO_PKG_VERSION")
        ))));
        about.append(&gtk4::Label::new(Some(
            "Not affiliated with or supported by Proton.",
        )));
        pages.add_titled(&about, Some("about"), "About");
        pages.set_visible_child_name(initial_page.unwrap_or("status"));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        activity.set_margin_start(12);
        activity.set_margin_end(12);
        activity.set_margin_bottom(8);
        toolbar.add_bottom_bar(&activity);
        toolbar.set_content(Some(&root));
        window.set_content(Some(&toolbar));
        window.present();

        let tx_restore = tx.clone();
        std::thread::spawn(move || {
            let account = credentials::load().map(|cred| cred.username().to_string());
            let _ = tx_restore.send(Event::Account(account));
        });
        let tx_ignore = tx.clone();
        std::thread::spawn(move || {
            let _ = tx_ignore.send(Event::IgnoreLoaded(pdignore::load_global_text()));
        });

        let authenticated = Rc::new(Cell::new(false));
        let refresh = tx.clone();
        let refresh_authenticated = authenticated.clone();
        gtk4::glib::timeout_add_local(Duration::from_secs(8), move || {
            if refresh_authenticated.get() {
                command(&refresh, "Status", vec!["status".into()]);
            }
            gtk4::glib::ControlFlow::Continue
        });
        gtk4::glib::timeout_add_local(Duration::from_millis(100), move || {
            while let Ok(event) = rx.try_recv() {
                match event {
                    Event::Account(Some(name)) => {
                        authenticated.set(true);
                        username.set_text(&format!("Signed in as {name}"));
                        root.set_visible_child_name("main");
                        command(&tx, "Mount", vec!["mount".into()]);
                        command(&tx, "Status", vec!["status".into()]);
                        command(&tx, "Computers", vec!["computers".into()]);
                    }
                    Event::Account(None) => {
                        authenticated.set(false);
                        root.set_visible_child_name("login");
                        login_link.set_visible(false);
                        login_details.set_text("No saved account.");
                        login_button.set_sensitive(true);
                    }
                    Event::LoginLine(line) => {
                        if line.starts_with("https://") {
                            login_link.set_uri(&line);
                            login_link.set_visible(true);
                        } else {
                            login_details.set_text(&format!("{}\n{line}", login_details.text()));
                        }
                    }
                    Event::IgnoreLoaded(text) => editor.buffer().set_text(&text),
                    Event::Done(action, result) => {
                        let message = match result {
                            Ok(text) => {
                                if action == "Status" {
                                    status_text.set_text(&text);
                                    mount_text.set_text(&text);
                                } else if action == "Computers" {
                                    listing.set_text(&text);
                                } else if action == "Mount" || action == "Stop" {
                                    command(&tx, "Status", vec!["status".into()]);
                                } else if action == "Sign in" {
                                    login_button.set_sensitive(true);
                                    let tx = tx.clone();
                                    std::thread::spawn(move || {
                                        let account = credentials::load()
                                            .map(|cred| cred.username().to_string());
                                        let _ = tx.send(Event::Account(account));
                                    });
                                } else if action == "Sign out" {
                                    authenticated.set(false);
                                    username.set_text("");
                                    root.set_visible_child_name("login");
                                    login_link.set_visible(false);
                                    login_details.set_text("Signed out.");
                                } else if action == "Register computer"
                                    || action == "Add backup"
                                    || action == "Unsync"
                                    || action == "Restore"
                                {
                                    command(&tx, "Computers", vec!["computers".into()]);
                                }
                                format!("{action}: {text}")
                            }
                            Err(error) => {
                                if action == "Sign in" {
                                    login_details.set_text(&format!("Sign-in failed: {error}"));
                                    login_button.set_sensitive(true);
                                }
                                format!("{action} failed: {error}")
                            }
                        };
                        activity.set_text(&message);
                    }
                }
            }
            gtk4::glib::ControlFlow::Continue
        });
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn gui_accepts_known_page_and_global_flags() {
            let options = Options::try_parse_from([
                "pdcli-gui",
                "--page",
                "computers",
                "--force-offline",
                "--no-tray",
            ])
            .unwrap();
            assert_eq!(options.page.as_deref(), Some("computers"));
            assert!(options.force_offline && options.no_tray);
            assert!(Options::try_parse_from(["pdcli-gui", "--page", "unknown"]).is_err());
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    desktop::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("The pdcli GTK4 desktop app is available on Linux only.");
}
