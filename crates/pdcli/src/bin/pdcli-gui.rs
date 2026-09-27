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
    use serde::Deserialize;
    use std::{
        cell::{Cell, RefCell},
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
        #[arg(long, value_parser = ["status", "files", "computers", "mount", "about", "account", "settings"])]
        page: Option<String>,
        #[arg(long)]
        force_offline: bool,
        #[arg(long)]
        no_tray: bool,
    }

    static CLI_FLAGS: OnceLock<Vec<&'static str>> = OnceLock::new();

    #[derive(Clone, Deserialize)]
    struct Folder {
        uid: String,
        name: String,
    }

    #[derive(Clone, Deserialize)]
    struct FileItem {
        uid: String,
        name: String,
        kind: String,
        size: Option<i64>,
        degraded: bool,
        error: Option<String>,
    }

    #[derive(Deserialize)]
    struct FileListing {
        folder: Folder,
        items: Vec<FileItem>,
    }

    #[derive(Deserialize)]
    struct Status {
        daemon: String,
        mountpoint: String,
        mounted: bool,
        journal: Option<Journal>,
    }

    #[derive(Deserialize)]
    struct Journal {
        pending: usize,
        failed: usize,
    }

    #[derive(Default)]
    struct FilesState {
        request: u64,
        trail: Vec<Folder>,
        items: Vec<FileItem>,
        selected: Option<FileItem>,
    }

    enum Event {
        Account(Option<String>),
        LoginLine(String),
        Done(String, Result<String, String>),
        IgnoreLoaded(String),
        Files(u64, Vec<Folder>, Result<FileListing, String>),
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

    fn refresh_status(tx: &Sender<Event>) {
        command(tx, "Status", vec!["status".into(), "--json".into()]);
    }

    fn load_files(tx: &Sender<Event>, state: &Rc<RefCell<FilesState>>, trail: Vec<Folder>) {
        let mut state = state.borrow_mut();
        state.request += 1;
        let request = state.request;
        drop(state);
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut args = vec!["browse".into(), "list".into()];
            if let Some(folder) = trail.last() {
                args.push(folder.uid.clone());
            }
            let result = run_command(&args).and_then(|text| {
                serde_json::from_str(&text).map_err(|e| format!("Invalid browse response: {e}"))
            });
            let _ = tx.send(Event::Files(request, trail, result));
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

    fn section(title: &str, description: &str) -> gtk4::Box {
        let group = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        group.set_margin_top(12);
        let heading = gtk4::Label::new(Some(title));
        heading.add_css_class("title-3");
        heading.set_halign(gtk4::Align::Start);
        group.append(&heading);
        let subtitle = gtk4::Label::new(Some(description));
        subtitle.add_css_class("dim-label");
        subtitle.set_halign(gtk4::Align::Start);
        subtitle.set_wrap(true);
        group.append(&subtitle);
        group
    }

    fn nav_button(
        box_: &gtk4::Box,
        pages: &gtk4::Stack,
        group: Option<&gtk4::ToggleButton>,
        title: &str,
        page: &str,
    ) -> gtk4::ToggleButton {
        let button = gtk4::ToggleButton::with_label(title);
        button.set_halign(gtk4::Align::Fill);
        if let Some(group) = group {
            button.set_group(Some(group));
        }
        let pages = pages.clone();
        let page = page.to_owned();
        button.connect_toggled(move |button| {
            if button.is_active() {
                pages.set_visible_child_name(&page);
            }
        });
        box_.append(&button);
        button
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

    fn name_dialog(
        window: &adw::ApplicationWindow,
        title: &str,
        initial: &str,
        apply: impl Fn(String) + 'static,
    ) {
        let dialog = gtk4::Dialog::builder()
            .title(title)
            .transient_for(window)
            .modal(true)
            .build();
        dialog.add_button("Cancel", gtk4::ResponseType::Cancel);
        dialog.add_button(title, gtk4::ResponseType::Accept);
        let entry = gtk4::Entry::new();
        entry.set_text(initial);
        entry.set_margin_start(12);
        entry.set_margin_end(12);
        entry.set_margin_top(12);
        entry.set_margin_bottom(12);
        dialog.content_area().append(&entry);
        dialog.connect_response(move |dialog, response| {
            if response == gtk4::ResponseType::Accept {
                let name = entry.text().trim().to_string();
                if !name.is_empty()
                    && name != "."
                    && name != ".."
                    && !name.contains(['/', '\\', '\0'])
                {
                    apply(name);
                }
            }
            dialog.close();
        });
        dialog.present();
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
        let sidebar = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        sidebar.set_margin_start(12);
        sidebar.set_margin_end(12);
        sidebar.set_margin_top(18);
        sidebar.set_size_request(150, -1);
        let primary = section("Browse", "Your Drive");
        sidebar.append(&primary);
        let first = nav_button(&sidebar, &pages, None, "Files", "files");
        let mut navigation = vec![("files", first.clone())];
        navigation.push((
            "computers",
            nav_button(&sidebar, &pages, Some(&first), "Computers", "computers"),
        ));
        navigation.push((
            "status",
            nav_button(&sidebar, &pages, Some(&first), "Status", "status"),
        ));
        sidebar.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
        let secondary = section("More", "Device and account");
        sidebar.append(&secondary);
        for (title, page) in [
            ("Mount", "mount"),
            ("Account", "account"),
            ("Settings", "settings"),
            ("About", "about"),
        ] {
            navigation.push((
                page,
                nav_button(&sidebar, &pages, Some(&first), title, page),
            ));
        }
        let main = row();
        main.append(&scroll(&sidebar));
        main.append(&pages);
        root.add_named(&main, Some("main"));
        root.set_visible_child_name("login");

        let status = page("Status");
        let sync_group = section(
            "Background sync",
            "Changes are synced while Drive is mounted.",
        );
        let status_text = gtk4::Label::new(Some("Checking sync status…"));
        status_text.set_halign(gtk4::Align::Start);
        status_text.set_selectable(true);
        status_text.set_wrap(true);
        sync_group.append(&status_text);
        let status_actions = row();
        for (label, action, args) in [
            ("Pause sync", "Pause", vec!["pause"]),
            ("Resume sync", "Resume", vec!["resume"]),
            ("Retry now", "Retry", vec!["sync"]),
        ] {
            let tx = tx.clone();
            button(label, &status_actions, move || {
                command(&tx, action, args.iter().map(|s| s.to_string()).collect());
            });
        }
        sync_group.append(&status_actions);
        status.append(&sync_group);
        let activity = gtk4::Label::new(Some(""));
        activity.set_wrap(true);
        activity.set_selectable(true);
        activity.set_halign(gtk4::Align::Start);
        pages.add_titled(&scroll(&status), Some("status"), "Status");

        let files_state = Rc::new(RefCell::new(FilesState::default()));
        let files = page("Files");
        let files_status = gtk4::Label::new(Some("Sign in to browse files."));
        files_status.set_halign(gtk4::Align::Start);
        files_status.set_wrap(true);
        files_status.set_selectable(true);
        let files_controls = row();
        let back = button("Back", &files_controls, {
            let state = files_state.clone();
            let tx = tx.clone();
            let status = files_status.clone();
            move || {
                let trail = state.borrow().trail.clone();
                if trail.len() > 1 {
                    status.set_text("Loading files…");
                    load_files(&tx, &state, trail[..trail.len() - 1].to_vec());
                }
            }
        });
        back.set_sensitive(false);
        let files_refresh = button("Refresh", &files_controls, {
            let state = files_state.clone();
            let tx = tx.clone();
            let status = files_status.clone();
            move || {
                let trail = state.borrow().trail.clone();
                status.set_text("Loading files…");
                load_files(&tx, &state, trail);
            }
        });
        let create = button("New folder", &files_controls, {
            let state = files_state.clone();
            let tx = tx.clone();
            let window = window.clone();
            move || {
                if let Some(folder) = state.borrow().trail.last() {
                    let uid = folder.uid.clone();
                    let tx = tx.clone();
                    name_dialog(&window, "Create folder", "", move |name| {
                        command(
                            &tx,
                            "Files mkdir",
                            vec!["browse".into(), "mkdir".into(), uid.clone(), name],
                        );
                    });
                }
            }
        });
        create.set_sensitive(false);
        create.add_css_class("suggested-action");
        files.append(&files_controls);
        let breadcrumb = row();
        let breadcrumb_scroll = gtk4::ScrolledWindow::new();
        breadcrumb_scroll.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Never);
        breadcrumb_scroll.set_child(Some(&breadcrumb));
        files.append(&breadcrumb_scroll);
        files.append(&files_status);
        let files_list = gtk4::ListBox::new();
        files_list.set_selection_mode(gtk4::SelectionMode::Single);
        files_list.add_css_class("boxed-list");
        files.append(&scroll(&files_list));
        let selection_group = section("Selected item", "Actions affect only the selected item.");
        let selected_label =
            gtk4::Label::new(Some("Select an item to rename, trash, or open on the web."));
        selected_label.set_halign(gtk4::Align::Start);
        selected_label.set_wrap(true);
        selection_group.append(&selected_label);
        let selection_actions = row();
        let rename = button("Rename", &selection_actions, {
            let state = files_state.clone();
            let tx = tx.clone();
            let window = window.clone();
            move || {
                if let Some(item) = state
                    .borrow()
                    .selected
                    .clone()
                    .filter(|item| !item.degraded)
                {
                    let tx = tx.clone();
                    name_dialog(&window, "Rename", &item.name, move |name| {
                        command(
                            &tx,
                            "Files rename",
                            vec!["browse".into(), "rename".into(), item.uid.clone(), name],
                        );
                    });
                }
            }
        });
        rename.set_sensitive(false);
        let trash = button("Move to trash", &selection_actions, {
            let state = files_state.clone();
            let tx = tx.clone();
            let window = window.clone();
            move || {
                if let Some(item) = state
                    .borrow()
                    .selected
                    .clone()
                    .filter(|item| !item.degraded)
                {
                    let dialog = gtk4::Dialog::builder()
                        .title("Move to trash?")
                        .transient_for(&window)
                        .modal(true)
                        .build();
                    dialog.add_button("Cancel", gtk4::ResponseType::Cancel);
                    dialog.add_button("Move to trash", gtk4::ResponseType::Accept);
                    dialog
                        .content_area()
                        .append(&gtk4::Label::new(Some(&format!(
                            "Move “{}” to trash?",
                            item.name
                        ))));
                    let tx = tx.clone();
                    dialog.connect_response(move |dialog, response| {
                        if response == gtk4::ResponseType::Accept {
                            command(
                                &tx,
                                "Files trash",
                                vec!["browse".into(), "trash".into(), item.uid.clone()],
                            );
                        }
                        dialog.close();
                    });
                    dialog.present();
                }
            }
        });
        trash.set_sensitive(false);
        let web = button("Get web link", &selection_actions, {
            let state = files_state.clone();
            let tx = tx.clone();
            move || {
                if let Some(item) = state.borrow().selected.as_ref() {
                    command(
                        &tx,
                        &format!("Files url {}", item.uid),
                        vec!["browse".into(), "url".into(), item.uid.clone()],
                    );
                }
            }
        });
        web.set_sensitive(false);
        selection_group.append(&selection_actions);
        let web_link = gtk4::LinkButton::new("https://drive.proton.me");
        web_link.set_label("Open on the web");
        web_link.set_visible(false);
        selection_group.append(&web_link);
        selection_group.set_visible(false);
        files.append(&selection_group);
        files_list.connect_row_selected({
            let state = files_state.clone();
            let label = selected_label.clone();
            let selection_group = selection_group.clone();
            let rename = rename.clone();
            let trash = trash.clone();
            let web = web.clone();
            let web_link = web_link.clone();
            move |_, row| {
                let selected =
                    row.and_then(|row| state.borrow().items.get(row.index() as usize).cloned());
                label.set_text(&selected.as_ref().map_or_else(
                    || "Select an item to rename, trash, or open on the web.".to_string(),
                    |item| {
                        format!(
                            "{} ({}){}",
                            item.name,
                            item.kind,
                            item.error
                                .as_ref()
                                .map_or(String::new(), |e| format!(" — {e}"))
                        )
                    },
                ));
                let enabled = selected.is_some();
                selection_group.set_visible(enabled);
                let mutable = selected.as_ref().is_some_and(|item| !item.degraded);
                rename.set_sensitive(mutable);
                trash.set_sensitive(mutable);
                web.set_sensitive(enabled);
                web_link.set_visible(false);
                state.borrow_mut().selected = selected;
            }
        });
        files_list.connect_row_activated({
            let state = files_state.clone();
            let tx = tx.clone();
            let files_status = files_status.clone();
            move |_, row| {
                let current = state.borrow();
                if let Some(item) = current.items.get(row.index() as usize) {
                    if (item.kind == "folder" || item.kind == "album") && !item.degraded {
                        let mut trail = current.trail.clone();
                        trail.push(Folder {
                            uid: item.uid.clone(),
                            name: item.name.clone(),
                        });
                        drop(current);
                        files_status.set_text("Loading files…");
                        load_files(&tx, &state, trail);
                    }
                }
            }
        });
        pages.add_titled(&files, Some("files"), "Files");

        let mount = page("Mount");
        let mount_group = section(
            "Local Drive folder",
            "Mount Proton Drive in your file manager, or stop the local mount.",
        );
        let mount_text = gtk4::Label::new(Some("Checking mount status…"));
        mount_text.set_halign(gtk4::Align::Start);
        mount_text.set_selectable(true);
        mount_text.set_wrap(true);
        mount_group.append(&mount_text);
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
        mount_group.append(&mount_actions);
        mount.append(&mount_group);
        pages.add_titled(&mount, Some("mount"), "Mount");

        let computers = page("Computers");
        let registered = section(
            "This computer",
            "Register this device, or enter an existing device ID to bind it.",
        );
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
        registered.append(&register_row);
        let backup = section(
            "Back up a folder",
            "Copy a local folder to this computer in Drive and keep it in sync.",
        );
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
        backup.append(&backup_row);
        let remove = section(
            "Stop syncing a folder",
            "Unsync by job name or ID. Local and cloud files are kept.",
        );
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
        remove.append(&remove_row);
        let restore = section(
            "Restore a backup",
            "Download a computer folder to a local path and continue syncing.",
        );
        let restore_row = row();
        restore_row.set_orientation(gtk4::Orientation::Vertical);
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
        restore.append(&restore_row);
        let computers_list = section("Registered computers", "Devices and local sync jobs.");
        let listing = gtk4::Label::new(Some("Loading computers…"));
        listing.set_halign(gtk4::Align::Start);
        listing.set_selectable(true);
        listing.add_css_class("monospace");
        listing.set_wrap(true);
        computers_list.append(&listing);
        button("Refresh list", &computers_list, {
            let tx = tx.clone();
            move || command(&tx, "Computers", vec!["computers".into()])
        });
        for group in [&computers_list, &backup] {
            computers.append(group);
            computers.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
        }
        let advanced = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        for group in [&registered, &restore, &remove] {
            advanced.append(group);
        }
        let advanced_toggle = gtk4::Expander::new(Some("Manage computers and backups"));
        advanced_toggle.set_child(Some(&advanced));
        computers.append(&advanced_toggle);
        pages.add_titled(&scroll(&computers), Some("computers"), "Computers");

        let account = page("Account");
        let account_group = section("Signed-in account", "Credentials are stored by pdcli.");
        let username = gtk4::Label::new(Some(""));
        account_group.append(&username);
        button("Sign out", &account_group, {
            let tx = tx.clone();
            move || command(&tx, "Sign out", vec!["logout".into()])
        });
        account.append(&account_group);
        pages.add_titled(&account, Some("account"), "Account");

        let settings = page("Settings");
        settings.append(&section(
            "Global .pdignore",
            "Patterns in this file are not uploaded from the mount.",
        ));
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
        let selected_page = initial_page.unwrap_or("files");
        navigation
            .iter()
            .find(|(page, _)| *page == selected_page)
            .map(|(_, button)| button)
            .unwrap_or(&first)
            .set_active(true);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        activity.set_margin_start(12);
        activity.set_margin_end(12);
        activity.set_margin_bottom(8);
        toolbar.add_bottom_bar(&activity);
        toolbar.set_content(Some(&root));
        window.set_content(Some(&toolbar));
        let compact = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 760sp").expect("valid breakpoint"),
        );
        for row in [
            &files_controls,
            &selection_actions,
            &status_actions,
            &mount_actions,
            &register_row,
            &backup_row,
            &remove_row,
        ] {
            compact.add_setter(
                row,
                "orientation",
                Some(&gtk4::Orientation::Vertical.to_value()),
            );
        }
        window.add_breakpoint(compact);
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
                refresh_status(&refresh);
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
                        files_status.set_text("Loading files…");
                        load_files(&tx, &files_state, Vec::new());
                        command(&tx, "Mount", vec!["mount".into()]);
                        refresh_status(&tx);
                        command(&tx, "Computers", vec!["computers".into()]);
                    }
                    Event::Account(None) => {
                        authenticated.set(false);
                        root.set_visible_child_name("login");
                        files_state.borrow_mut().request += 1;
                        files_state.borrow_mut().selected = None;
                        files_status.set_text("Sign in to browse files.");
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
                    Event::Files(request, mut trail, result) => {
                        if request != files_state.borrow().request {
                            continue;
                        }
                        match result {
                            Ok(listing) => {
                                if trail.is_empty() {
                                    trail.push(listing.folder.clone());
                                } else {
                                    *trail.last_mut().unwrap() = listing.folder.clone();
                                }
                                while let Some(child) = breadcrumb.first_child() {
                                    breadcrumb.remove(&child);
                                }
                                for (index, folder) in trail.iter().enumerate() {
                                    let tx = tx.clone();
                                    let state = files_state.clone();
                                    let destination = trail[..=index].to_vec();
                                    let status = files_status.clone();
                                    button(&folder.name, &breadcrumb, move || {
                                        status.set_text("Loading files…");
                                        load_files(&tx, &state, destination.clone());
                                    });
                                }
                                while let Some(child) = files_list.first_child() {
                                    files_list.remove(&child);
                                }
                                let count = listing.items.len();
                                files_state.borrow_mut().trail = trail;
                                files_state.borrow_mut().items = listing.items.clone();
                                files_state.borrow_mut().selected = None;
                                for item in listing.items {
                                    let line = row();
                                    let icon = if item.kind == "folder" || item.kind == "album" {
                                        "📁"
                                    } else {
                                        "📄"
                                    };
                                    let label =
                                        gtk4::Label::new(Some(&format!("{icon}  {}", item.name)));
                                    label.set_halign(gtk4::Align::Start);
                                    label.set_hexpand(true);
                                    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                                    line.append(&label);
                                    if let Some(size) = item.size.filter(|size| *size >= 0) {
                                        line.append(&gtk4::Label::new(Some(&format!(
                                            "{size} bytes"
                                        ))));
                                    }
                                    if item.degraded {
                                        line.append(&gtk4::Label::new(Some("Unavailable")));
                                    }
                                    let list_row = gtk4::ListBoxRow::new();
                                    list_row.set_child(Some(&line));
                                    files_list.append(&list_row);
                                }
                                files_status.set_text(if count == 0 {
                                    "This folder is empty."
                                } else {
                                    "Double-click a folder to open it."
                                });
                                back.set_sensitive(files_state.borrow().trail.len() > 1);
                                create.set_sensitive(true);
                                files_refresh.set_sensitive(true);
                            }
                            Err(error) => {
                                files_status.set_text(&format!("Could not load files: {error}"));
                            }
                        }
                    }
                    Event::Done(action, result) => {
                        let routine =
                            result.is_ok() && (action == "Status" || action == "Computers");
                        let message = match result {
                            Ok(text) => {
                                if action == "Status" {
                                    match serde_json::from_str::<Status>(&text) {
                                        Ok(status) => {
                                            let journal = status.journal.map_or_else(
                                                || "Queue unavailable".to_string(),
                                                |j| {
                                                    format!(
                                                        "{} pending · {} failed",
                                                        j.pending, j.failed
                                                    )
                                                },
                                            );
                                            status_text.set_text(&format!(
                                                "Daemon: {}\nSync queue: {journal}",
                                                status.daemon
                                            ));
                                            mount_text.set_text(&format!(
                                                "{}\n{}",
                                                status.mountpoint,
                                                if status.mounted {
                                                    "Mounted"
                                                } else {
                                                    "Not mounted"
                                                }
                                            ));
                                        }
                                        Err(error) => {
                                            let message =
                                                format!("Invalid status response: {error}");
                                            status_text.set_text(&message);
                                            mount_text.set_text(&message);
                                            activity.set_text(&message);
                                        }
                                    }
                                } else if action == "Computers" {
                                    listing.set_text(&text);
                                } else if action == "Mount" || action == "Stop" {
                                    refresh_status(&tx);
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
                                    files_state.borrow_mut().request += 1;
                                    files_state.borrow_mut().trail.clear();
                                    files_state.borrow_mut().items.clear();
                                    files_state.borrow_mut().selected = None;
                                    while let Some(child) = files_list.first_child() {
                                        files_list.remove(&child);
                                    }
                                    while let Some(child) = breadcrumb.first_child() {
                                        breadcrumb.remove(&child);
                                    }
                                    back.set_sensitive(false);
                                    create.set_sensitive(false);
                                    rename.set_sensitive(false);
                                    trash.set_sensitive(false);
                                    web.set_sensitive(false);
                                    web_link.set_visible(false);
                                    files_status.set_text("Sign in to browse files.");
                                    login_link.set_visible(false);
                                    login_details.set_text("Signed out.");
                                } else if let Some(uid) = action.strip_prefix("Files url ") {
                                    if text.starts_with("https://")
                                        && files_state
                                            .borrow()
                                            .selected
                                            .as_ref()
                                            .is_some_and(|item| item.uid == uid)
                                    {
                                        web_link.set_uri(&text);
                                        web_link.set_visible(true);
                                    } else if !text.starts_with("https://") {
                                        files_status.set_text("Invalid web URL returned by pdcli.");
                                    }
                                } else if authenticated.get()
                                    && matches!(
                                        action.as_str(),
                                        "Files mkdir" | "Files rename" | "Files trash"
                                    )
                                {
                                    files_status.set_text("Loading files…");
                                    let trail = files_state.borrow().trail.clone();
                                    load_files(&tx, &files_state, trail);
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
                                if action == "Status" {
                                    status_text
                                        .set_text(&format!("Could not check status: {error}"));
                                    mount_text.set_text(&format!("Could not check mount: {error}"));
                                }
                                if action.starts_with("Files ") {
                                    files_status.set_text(&format!("{action} failed: {error}"));
                                }
                                if action == "Sign in" {
                                    login_details.set_text(&format!("Sign-in failed: {error}"));
                                    login_button.set_sensitive(true);
                                }
                                format!("{action} failed: {error}")
                            }
                        };
                        if !routine {
                            activity.set_text(&message);
                        }
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
            assert_eq!(
                Options::try_parse_from(["pdcli-gui", "--page", "files"])
                    .unwrap()
                    .page
                    .as_deref(),
                Some("files")
            );
        }

        #[test]
        fn browse_listing_parses_nullable_and_degraded_items() {
            let listing: FileListing = serde_json::from_str(
                r#"{"folder":{"uid":"root","name":"My files","parent_uid":null},"items":[{"uid":"one","name":"Photo","kind":"photo","size":null,"degraded":true,"error":"No preview"}]}"#,
            )
            .unwrap();
            assert_eq!(listing.folder.uid, "root");
            assert_eq!(listing.items[0].kind, "photo");
            assert!(listing.items[0].degraded);
            assert!(serde_json::from_str::<FileListing>(r#"{"items":[]}"#).is_err());
            let signed_size: FileListing = serde_json::from_str(
                r#"{"folder":{"uid":"root","name":"My files","parent_uid":null},"items":[{"uid":"two","name":"File","kind":"file","size":-1,"degraded":false,"error":null}]}"#,
            )
            .unwrap();
            assert_eq!(signed_size.items[0].size, Some(-1));
        }

        #[test]
        fn status_response_parses_mount_and_queue() {
            let status: Status = serde_json::from_str(
                r#"{"signed_in":true,"username":"user","daemon":"paused","mountpoint":"/tmp/drive","mounted":true,"journal":{"pending":2,"failed":1,"entries":[]}}"#,
            )
            .unwrap();
            assert_eq!(status.daemon, "paused");
            assert!(status.mounted);
            assert_eq!(status.mountpoint, "/tmp/drive");
            assert_eq!(status.journal.unwrap().failed, 1);
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
