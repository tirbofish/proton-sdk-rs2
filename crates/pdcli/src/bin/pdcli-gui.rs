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
    use serde::{Deserialize, Serialize};
    use std::{
        cell::{Cell, RefCell},
        io::{BufRead, BufReader, Write},
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
        #[arg(long, value_parser = ["status", "files", "photos", "computers", "mount", "about", "account", "settings"])]
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
        transfers: Vec<Transfer>,
    }

    #[derive(Deserialize)]
    struct Transfer {
        id: usize,
        filename: String,
        direction: String,
        bytes_transferred: i64,
        total_bytes: i64,
        cancellable: bool,
    }

    #[derive(Deserialize)]
    struct Journal {
        pending: usize,
        failed: usize,
    }

    #[derive(Deserialize)]
    struct SharingInfo {
        members: Vec<ShareMember>,
        pending_invitations: usize,
        public_link: Option<PublicLink>,
    }

    #[derive(Deserialize)]
    struct ShareMember {
        email: String,
        role: String,
    }

    #[derive(Deserialize)]
    struct PublicLink {
        url: String,
        role: String,
        expires: Option<String>,
    }

    #[derive(Deserialize)]
    struct ComputerList {
        this_device_id: Option<String>,
        computers: Vec<Computer>,
        jobs: Vec<ComputerJob>,
    }

    #[derive(Deserialize)]
    struct Computer {
        id: String,
        name: String,
        last_sync_time: Option<String>,
    }

    #[derive(Deserialize)]
    struct ComputerJob {
        name: String,
        local_path: String,
        device_id: String,
    }

    #[derive(Clone, Deserialize)]
    struct PhotoItem {
        uid: String,
        name: Option<String>,
        capture_time: String,
    }

    #[derive(Deserialize)]
    struct PhotoPage {
        items: Vec<PhotoItem>,
        next_cursor: Option<String>,
    }

    #[derive(Deserialize)]
    struct AlbumItem {
        uid: String,
        name: Option<String>,
        photo_count: u64,
    }

    #[derive(Deserialize)]
    struct AlbumList {
        albums: Vec<AlbumItem>,
    }

    #[derive(Deserialize)]
    struct AlbumDetails {
        album: AlbumItem,
        items: Vec<PhotoItem>,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(default)]
    struct Preferences {
        auto_mount: bool,
        start_page: String,
    }

    impl Default for Preferences {
        fn default() -> Self {
            Self {
                auto_mount: true,
                start_page: "files".into(),
            }
        }
    }

    fn preferences_path() -> std::path::PathBuf {
        pdignore::global_path().with_file_name("desktop.json")
    }

    fn load_preferences() -> Result<Preferences, String> {
        match std::fs::read(preferences_path()) {
            Ok(bytes) => {
                let preferences: Preferences =
                    serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
                if !["files", "photos", "computers", "status"]
                    .contains(&preferences.start_page.as_str())
                {
                    return Err("Invalid default page in desktop settings".into());
                }
                Ok(preferences)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Preferences::default())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    fn save_preferences(preferences: &Preferences) -> Result<(), String> {
        let path = preferences_path();
        std::fs::create_dir_all(path.parent().expect("preferences have a config directory"))
            .map_err(|error| error.to_string())?;
        let content = serde_json::to_vec(preferences).map_err(|error| error.to_string())?;
        std::fs::write(path, content).map_err(|error| error.to_string())
    }

    #[derive(Default)]
    struct FilesState {
        request: u64,
        trail: Vec<Folder>,
        items: Vec<FileItem>,
        selected: Option<FileItem>,
    }

    #[derive(Default)]
    struct PhotosState {
        request: u64,
        next_cursor: Option<String>,
        selected: Option<String>,
        items: Vec<PhotoItem>,
    }

    enum Event {
        Account(Option<String>),
        LoginLine(String),
        Done(String, Result<String, String>),
        IgnoreLoaded(String),
        Files(u64, Vec<Folder>, Result<FileListing, String>),
        Photos(u64, Option<String>, Result<PhotoPage, String>),
        Albums(u64, Result<AlbumList, String>),
        Album(u64, Result<AlbumDetails, String>),
        Preview(String, Result<Vec<u8>, String>),
    }

    fn run_command_bytes(args: &[String]) -> Result<Vec<u8>, String> {
        run_command_bytes_with_input(args, None)
    }

    fn run_command_bytes_with_input(
        args: &[String],
        input: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        let binary = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .with_file_name("pdcli");
        let mut command = Command::new(binary);
        command
            .args(CLI_FLAGS.get().expect("CLI flags initialized"))
            .args(args);
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Could not start pdcli: {e}"))?;
        if let Some(input) = input {
            let write_result = child
                .stdin
                .take()
                .expect("requested piped stdin")
                .write_all(input);
            if let Err(error) = write_result {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Could not send link password: {error}"));
            }
        }
        let output = child
            .wait_with_output()
            .map_err(|error| error.to_string())?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "pdcli exited with {}: {}",
                output.status,
                stderr.trim()
            ))
        }
    }

    fn run_command(args: &[String]) -> Result<String, String> {
        String::from_utf8(run_command_bytes(args)?)
            .map(|text| text.trim().to_owned())
            .map_err(|error| format!("Invalid CLI text: {error}"))
    }

    fn command_with_input(tx: &Sender<Event>, action: String, args: Vec<String>, input: Vec<u8>) {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let result = run_command_bytes_with_input(&args, Some(&input))
                .and_then(|bytes| String::from_utf8(bytes).map_err(|error| error.to_string()))
                .map(|text| text.trim().to_owned());
            let _ = tx.send(Event::Done(action, result));
        });
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

    fn load_photos(tx: &Sender<Event>, state: &Rc<RefCell<PhotosState>>, cursor: Option<String>) {
        let mut state = state.borrow_mut();
        state.request += 1;
        let request = state.request;
        drop(state);
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut args = vec!["photos".into(), "timeline".into()];
            if let Some(cursor) = &cursor {
                args.extend(["--cursor".into(), cursor.clone()]);
            }
            let result = run_command(&args).and_then(|text| {
                serde_json::from_str(&text)
                    .map_err(|error| format!("Invalid Photos response: {error}"))
            });
            let _ = tx.send(Event::Photos(request, cursor, result));
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

    fn show_photos(list: &gtk4::ListBox, items: &[PhotoItem], append: bool) {
        if !append {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
        }
        for item in items {
            let line = row();
            let name = gtk4::Label::new(Some(item.name.as_deref().unwrap_or("Untitled photo")));
            name.set_hexpand(true);
            name.set_halign(gtk4::Align::Start);
            name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            line.append(&name);
            line.append(&gtk4::Label::new(Some(&item.capture_time)));
            let row = gtk4::ListBoxRow::new();
            row.set_child(Some(&line));
            list.append(&row);
        }
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
            "photos",
            nav_button(&sidebar, &pages, Some(&first), "Photos", "photos"),
        ));
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
            ("Account", "account"),
            ("Settings", "settings"),
            ("About", "about"),
        ] {
            navigation.push((
                page,
                nav_button(&sidebar, &pages, Some(&first), title, page),
            ));
        }
        let sidebar_column = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        let navigation_scroll = scroll(&sidebar);
        navigation_scroll.set_min_content_width(150);
        sidebar_column.append(&navigation_scroll);
        let profile = gtk4::MenuButton::new();
        profile.set_icon_name("avatar-default-symbolic");
        profile.set_tooltip_text(Some("Account menu"));
        profile.set_margin_start(12);
        profile.set_margin_end(12);
        profile.set_margin_bottom(12);
        let profile_menu = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        profile_menu.set_margin_start(12);
        profile_menu.set_margin_end(12);
        profile_menu.set_margin_top(12);
        profile_menu.set_margin_bottom(12);
        let profile_name = gtk4::Label::new(Some("Account"));
        profile_name.set_selectable(true);
        profile_menu.append(&profile_name);
        button("Settings", &profile_menu, {
            let settings = navigation
                .iter()
                .find(|(page, _)| *page == "settings")
                .unwrap()
                .1
                .clone();
            move || settings.set_active(true)
        });
        button("Log out", &profile_menu, {
            let tx = tx.clone();
            move || command(&tx, "Sign out", vec!["logout".into()])
        });
        let popover = gtk4::Popover::new();
        popover.set_child(Some(&profile_menu));
        profile.set_popover(Some(&popover));
        sidebar_column.append(&profile);
        let main = gtk4::Paned::new(gtk4::Orientation::Horizontal);
        main.set_start_child(Some(&sidebar_column));
        main.set_end_child(Some(&pages));
        main.set_position(200);
        main.set_shrink_start_child(false);
        main.set_wide_handle(true);
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
            ("Pause background sync", "Pause", vec!["pause"]),
            ("Resume sync", "Resume", vec!["resume"]),
            ("Sync now", "Retry", vec!["sync"]),
            ("Retry failed", "Retry failed", vec!["retry", "--all"]),
        ] {
            let tx = tx.clone();
            button(label, &status_actions, move || {
                command(&tx, action, args.iter().map(|s| s.to_string()).collect());
            });
        }
        sync_group.append(&status_actions);
        status.append(&sync_group);
        let transfer_group = section("Transfers", "Active downloads and uploads.");
        let transfer_list = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        transfer_list.append(&gtk4::Label::new(Some("No active transfers.")));
        transfer_group.append(&transfer_list);
        status.append(&transfer_group);
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
        let file_rows = scroll(&files_list);
        file_rows.set_min_content_height(220);
        files.append(&file_rows);
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
        let sharing = section(
            "Sharing",
            "Invite someone or create a public link for this item.",
        );
        let sharing_status = gtk4::Label::new(Some("Check sharing to see current access."));
        sharing_status.set_halign(gtk4::Align::Start);
        sharing_status.set_wrap(true);
        sharing_status.set_selectable(true);
        sharing.append(&sharing_status);
        let share_link = gtk4::LinkButton::new("https://drive.proton.me");
        share_link.set_label("Open public link");
        share_link.set_visible(false);
        sharing.append(&share_link);
        let share_actions = row();
        button("Check sharing", &share_actions, {
            let state = files_state.clone();
            let tx = tx.clone();
            move || {
                if let Some(item) = state.borrow().selected.as_ref() {
                    command(
                        &tx,
                        &format!("Share status {}", item.uid),
                        vec![
                            "share".into(),
                            "status".into(),
                            item.uid.clone(),
                            "--json".into(),
                        ],
                    );
                }
            }
        });
        sharing.append(&share_actions);
        let link_fields = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        let link_warning = gtk4::Label::new(Some(
            "Updating a link replaces its role, password, and expiry. Blank fields remove existing limits.",
        ));
        link_warning.set_wrap(true);
        link_warning.set_halign(gtk4::Align::Start);
        link_fields.append(&link_warning);
        let link_options = row();
        let share_role = gtk4::DropDown::from_strings(&["Viewer", "Editor"]);
        link_options.append(&share_role);
        let share_password = field("Link password (optional)", &link_options);
        share_password.set_visibility(false);
        let share_expires = field("Expiry (RFC 3339, optional)", &link_options);
        link_fields.append(&link_options);
        button("Create / update link", &link_fields, {
            let state = files_state.clone();
            let tx = tx.clone();
            let role = share_role.clone();
            move || {
                if let Some(item) = state
                    .borrow()
                    .selected
                    .as_ref()
                    .filter(|item| !item.degraded)
                {
                    let mut args = vec![
                        "share".into(),
                        "link".into(),
                        item.uid.clone(),
                        "--role".into(),
                        if role.selected() == 0 {
                            "viewer"
                        } else {
                            "editor"
                        }
                        .into(),
                    ];
                    if !share_expires.text().is_empty() {
                        args.extend(["--expires".into(), share_expires.text().to_string()]);
                    }
                    let password = share_password.text().to_string();
                    if password.is_empty() {
                        command(&tx, &format!("Share link {}", item.uid), args);
                    } else {
                        args.push("--password-stdin".into());
                        share_password.set_text("");
                        command_with_input(
                            &tx,
                            format!("Share link {}", item.uid),
                            args,
                            format!("{password}\n").into_bytes(),
                        );
                    }
                }
            }
        });
        let invite_actions = row();
        let invite_email = field("Email address", &invite_actions);
        button("Invite", &invite_actions, {
            let state = files_state.clone();
            let tx = tx.clone();
            let role = share_role.clone();
            move || {
                if let Some(item) = state
                    .borrow()
                    .selected
                    .as_ref()
                    .filter(|item| !item.degraded)
                {
                    let email = invite_email.text().trim().to_string();
                    if !email.is_empty() {
                        command(
                            &tx,
                            &format!("Share invite {}", item.uid),
                            vec![
                                "share".into(),
                                "invite".into(),
                                item.uid.clone(),
                                email,
                                "--role".into(),
                                if role.selected() == 0 {
                                    "viewer"
                                } else {
                                    "editor"
                                }
                                .into(),
                            ],
                        );
                    }
                }
            }
        });
        button("Remove public link", &link_fields, {
            let state = files_state.clone();
            let tx = tx.clone();
            let window = window.clone();
            move || {
                if let Some(item) = state
                    .borrow()
                    .selected
                    .as_ref()
                    .filter(|item| !item.degraded)
                {
                    let uid = item.uid.clone();
                    let dialog = gtk4::Dialog::builder()
                        .title("Remove public link?")
                        .transient_for(&window)
                        .modal(true)
                        .build();
                    dialog.add_button("Cancel", gtk4::ResponseType::Cancel);
                    dialog.add_button("Remove link", gtk4::ResponseType::Accept);
                    dialog.content_area().append(&gtk4::Label::new(Some(
                        "Anyone using this link will lose access.",
                    )));
                    let tx = tx.clone();
                    dialog.connect_response(move |dialog, response| {
                        if response == gtk4::ResponseType::Accept {
                            command(
                                &tx,
                                &format!("Share remove {uid}"),
                                vec!["share".into(), "remove".into(), uid.clone()],
                            );
                        }
                        dialog.close();
                    });
                    dialog.present();
                }
            }
        });
        let link_expander = gtk4::Expander::new(Some("Public link"));
        link_expander.set_child(Some(&link_fields));
        sharing.append(&link_expander);
        let people_fields = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        people_fields.append(&invite_actions);
        let revoke_actions = row();
        let revoke_email = field("Member email to remove", &revoke_actions);
        button("Remove member", &revoke_actions, {
            let state = files_state.clone();
            let tx = tx.clone();
            let window = window.clone();
            move || {
                if let Some(item) = state
                    .borrow()
                    .selected
                    .as_ref()
                    .filter(|item| !item.degraded)
                {
                    let email = revoke_email.text().trim().to_string();
                    if email.is_empty() {
                        return;
                    }
                    let uid = item.uid.clone();
                    let dialog = gtk4::Dialog::builder()
                        .title("Remove member?")
                        .transient_for(&window)
                        .modal(true)
                        .build();
                    dialog.add_button("Cancel", gtk4::ResponseType::Cancel);
                    dialog.add_button("Remove access", gtk4::ResponseType::Accept);
                    dialog
                        .content_area()
                        .append(&gtk4::Label::new(Some(&format!(
                            "Remove access for {email}?"
                        ))));
                    let tx = tx.clone();
                    dialog.connect_response(move |dialog, response| {
                        if response == gtk4::ResponseType::Accept {
                            command(
                                &tx,
                                &format!("Share revoke {uid}"),
                                vec!["share".into(), "revoke".into(), uid.clone(), email.clone()],
                            );
                        }
                        dialog.close();
                    });
                    dialog.present();
                }
            }
        });
        people_fields.append(&revoke_actions);
        let people_expander = gtk4::Expander::new(Some("People with access"));
        people_expander.set_child(Some(&people_fields));
        sharing.append(&people_expander);
        sharing.set_visible(false);
        files.append(&sharing);
        files_list.connect_row_selected({
            let state = files_state.clone();
            let label = selected_label.clone();
            let selection_group = selection_group.clone();
            let rename = rename.clone();
            let trash = trash.clone();
            let web = web.clone();
            let web_link = web_link.clone();
            let sharing = sharing.clone();
            let sharing_status = sharing_status.clone();
            let share_link = share_link.clone();
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
                sharing.set_visible(enabled);
                sharing_status.set_text("Check sharing to see current access.");
                share_link.set_visible(false);
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
        pages.add_titled(&scroll(&files), Some("files"), "Files");

        let photos_state = Rc::new(RefCell::new(PhotosState::default()));
        let photos = page("Photos");
        let photos_controls = row();
        let photos_status = gtk4::Label::new(Some("Loading photos…"));
        photos_status.set_halign(gtk4::Align::Start);
        photos_status.set_wrap(true);
        let photos_views = gtk4::Stack::new();
        let timeline = gtk4::ListBox::new();
        timeline.add_css_class("boxed-list");
        let albums = gtk4::ListBox::new();
        albums.add_css_class("boxed-list");
        let album_photos = gtk4::ListBox::new();
        album_photos.add_css_class("boxed-list");
        photos_views.add_named(&scroll(&timeline), Some("timeline"));
        photos_views.add_named(&scroll(&albums), Some("albums"));
        photos_views.add_named(&scroll(&album_photos), Some("album"));
        let album_items = Rc::new(RefCell::new(Vec::<AlbumItem>::new()));
        button("Timeline", &photos_controls, {
            let views = photos_views.clone();
            let timeline = timeline.clone();
            let state = photos_state.clone();
            let tx = tx.clone();
            let status = photos_status.clone();
            move || {
                views.set_visible_child_name("timeline");
                status.set_text("Loading photos…");
                state.borrow_mut().items.clear();
                state.borrow_mut().selected = None;
                while let Some(child) = timeline.first_child() {
                    timeline.remove(&child);
                }
                load_photos(&tx, &state, None);
            }
        });
        button("Albums", &photos_controls, {
            let views = photos_views.clone();
            let state = photos_state.clone();
            let tx = tx.clone();
            let status = photos_status.clone();
            move || {
                views.set_visible_child_name("albums");
                status.set_text("Loading albums…");
                state.borrow_mut().request += 1;
                let request = state.borrow().request;
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let result =
                        run_command(&["photos".into(), "albums".into()]).and_then(|text| {
                            serde_json::from_str(&text)
                                .map_err(|error| format!("Invalid albums response: {error}"))
                        });
                    let _ = tx.send(Event::Albums(request, result));
                });
            }
        });
        let back_to_albums = button("Back to albums", &photos_controls, {
            let views = photos_views.clone();
            move || views.set_visible_child_name("albums")
        });
        back_to_albums.set_visible(false);
        back_to_albums.connect_clicked(|button| button.set_visible(false));
        photos.append(&photos_controls);
        photos.append(&photos_status);
        photos_views.set_vexpand(true);
        photos.append(&photos_views);
        let load_more = button("Load more", &photos, {
            let state = photos_state.clone();
            let tx = tx.clone();
            let status = photos_status.clone();
            move || {
                if let Some(cursor) = state.borrow().next_cursor.clone() {
                    status.set_text("Loading more photos…");
                    load_photos(&tx, &state, Some(cursor));
                }
            }
        });
        load_more.set_visible(false);
        let preview_group = section("Preview", "Select a photo to view its encrypted thumbnail.");
        let preview = gtk4::Picture::new();
        preview.set_size_request(-1, 260);
        preview.set_can_shrink(true);
        preview_group.append(&preview);
        let photo_web = gtk4::LinkButton::new("https://drive.proton.me");
        photo_web.set_label("Open in Proton Drive");
        photo_web.set_visible(false);
        preview_group.append(&photo_web);
        photos.append(&preview_group);
        for list in [&timeline, &album_photos] {
            list.connect_row_selected({
                let state = photos_state.clone();
                let tx = tx.clone();
                let preview = preview.clone();
                let photo_web = photo_web.clone();
                let status = photos_status.clone();
                move |_, row| {
                    let selected =
                        row.and_then(|row| state.borrow().items.get(row.index() as usize).cloned());
                    preview.set_paintable(None::<&gtk4::gdk::Paintable>);
                    photo_web.set_visible(false);
                    state.borrow_mut().selected = selected.as_ref().map(|item| item.uid.clone());
                    if let Some(item) = selected {
                        status.set_text(&format!(
                            "Loading preview: {}",
                            item.name.as_deref().unwrap_or("Photo")
                        ));
                        command(
                            &tx,
                            &format!("Photo web {}", item.uid),
                            vec!["browse".into(), "url".into(), item.uid.clone()],
                        );
                        let tx = tx.clone();
                        std::thread::spawn(move || {
                            let result = run_command_bytes(&[
                                "photos".into(),
                                "thumbnail".into(),
                                item.uid.clone(),
                                "--preview".into(),
                            ]);
                            let _ = tx.send(Event::Preview(item.uid, result));
                        });
                    }
                }
            });
        }
        albums.connect_row_activated({
            let items = album_items.clone();
            let state = photos_state.clone();
            let tx = tx.clone();
            let status = photos_status.clone();
            move |_, row| {
                if let Some(album) = items.borrow().get(row.index() as usize) {
                    status.set_text("Loading album…");
                    state.borrow_mut().request += 1;
                    let request = state.borrow().request;
                    let uid = album.uid.clone();
                    let tx = tx.clone();
                    std::thread::spawn(move || {
                        let result =
                            run_command(&["photos".into(), "album".into(), uid]).and_then(|text| {
                                serde_json::from_str(&text)
                                    .map_err(|error| format!("Invalid album response: {error}"))
                            });
                        let _ = tx.send(Event::Album(request, result));
                    });
                }
            }
        });
        pages.add_titled(&scroll(&photos), Some("photos"), "Photos");

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
        status.append(&mount_group);

        let computers = page("Computers");
        let backup = section(
            "Back up this computer",
            "Choose a folder to keep a copy in Drive. Deleting local files does not delete the cloud copy.",
        );
        let backup_row = row();
        let backup_path = field("Local folder path", &backup_row);
        button("Choose folder…", &backup_row, {
            let window = window.clone();
            let backup_path = backup_path.clone();
            move || {
                let dialog = gtk4::FileChooserNative::builder()
                    .title("Choose a folder to back up")
                    .action(gtk4::FileChooserAction::SelectFolder)
                    .transient_for(&window)
                    .build();
                let path = backup_path.clone();
                dialog.connect_response(move |dialog, response| {
                    if response == gtk4::ResponseType::Accept {
                        if let Some(folder) = dialog.file() {
                            if let Some(value) = folder.path() {
                                path.set_text(&value.to_string_lossy());
                            }
                        }
                    }
                    dialog.destroy();
                });
                dialog.show();
            }
        });
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
        let computers_list = section(
            "Your backups",
            "The asterisk marks this computer; indented folders are its backups.",
        );
        let computers_status = gtk4::Label::new(Some("Loading computers…"));
        computers_status.set_halign(gtk4::Align::Start);
        computers_status.set_wrap(true);
        computers_list.append(&computers_status);
        let computer_rows = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        computers_list.append(&computer_rows);
        button("Refresh list", &computers_list, {
            let tx = tx.clone();
            move || command(&tx, "Computers", vec!["computers".into(), "--json".into()])
        });
        for group in [&computers_list, &backup] {
            computers.append(group);
            computers.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
        }
        let advanced = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        for group in [&restore, &remove] {
            advanced.append(group);
        }
        let advanced_toggle = gtk4::Expander::new(Some("Restore or stop a backup"));
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
        let preferences = load_preferences();
        let startup = section(
            "Startup",
            "Choose whether opening the window also starts the mount and tray.",
        );
        let auto_mount = gtk4::CheckButton::with_label("Mount Drive automatically after sign-in");
        auto_mount.set_active(preferences.as_ref().map_or(true, |value| value.auto_mount));
        startup.append(&auto_mount);
        let default_page =
            gtk4::DropDown::from_strings(&["Files", "Photos", "Computers", "Status"]);
        let selected = ["files", "photos", "computers", "status"]
            .iter()
            .position(|page| {
                preferences
                    .as_ref()
                    .is_ok_and(|value| value.start_page == *page)
            })
            .unwrap_or(0);
        default_page.set_selected(selected as u32);
        startup.append(&gtk4::Label::new(Some("Open this page by default:")));
        startup.append(&default_page);
        button("Save startup setting", &startup, {
            let tx = tx.clone();
            let auto_mount = auto_mount.clone();
            let default_page = default_page.clone();
            move || {
                let tx = tx.clone();
                let preferences = Preferences {
                    auto_mount: auto_mount.is_active(),
                    start_page: ["files", "photos", "computers", "status"]
                        .get(default_page.selected() as usize)
                        .unwrap_or(&"files")
                        .to_string(),
                };
                std::thread::spawn(move || {
                    let result =
                        save_preferences(&preferences).map(|_| "Startup setting saved".into());
                    let _ = tx.send(Event::Done("Preferences".into(), result));
                });
            }
        });
        settings.append(&startup);
        settings.append(&section(
            "Offline storage",
            "Downloaded FUSE files are not yet encrypted at rest. Use an encrypted filesystem for the configuration directory; safe cache removal is not available in this version.",
        ));
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
        let selected_page = match initial_page {
            Some("mount") => "status",
            Some(page) => page,
            None => preferences
                .as_ref()
                .map_or("files", |value| value.start_page.as_str()),
        };
        navigation
            .iter()
            .find(|(page, _)| *page == selected_page)
            .map(|(_, button)| button)
            .unwrap_or(&first)
            .set_active(true);

        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        let sidebar_toggle = gtk4::Button::from_icon_name("sidebar-show-symbolic");
        sidebar_toggle.set_tooltip_text(Some("Show or hide navigation"));
        sidebar_toggle.connect_clicked(move |_| {
            sidebar_column.set_visible(!sidebar_column.is_visible());
        });
        header.pack_start(&sidebar_toggle);
        toolbar.add_top_bar(&header);
        let notifications = adw::ToastOverlay::new();
        notifications.set_child(Some(&root));
        toolbar.set_content(Some(&notifications));
        window.set_content(Some(&toolbar));
        let compact = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 760sp").expect("valid breakpoint"),
        );
        for row in [
            &files_controls,
            &selection_actions,
            &share_actions,
            &link_options,
            &invite_actions,
            &revoke_actions,
            &status_actions,
            &mount_actions,
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
        if let Err(error) = preferences {
            notifications.add_toast(adw::Toast::new(&format!(
                "Could not load desktop settings: {error}"
            )));
        }

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
        gtk4::glib::timeout_add_local(Duration::from_secs(2), move || {
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
                        profile_name.set_text(&name);
                        root.set_visible_child_name("main");
                        files_status.set_text("Loading files…");
                        load_files(&tx, &files_state, Vec::new());
                        photos_status.set_text("Loading photos…");
                        load_photos(&tx, &photos_state, None);
                        if auto_mount.is_active() {
                            command(&tx, "Mount", vec!["mount".into()]);
                        }
                        refresh_status(&tx);
                        command(&tx, "Computers", vec!["computers".into(), "--json".into()]);
                    }
                    Event::Account(None) => {
                        authenticated.set(false);
                        profile_name.set_text("Account");
                        root.set_visible_child_name("login");
                        files_state.borrow_mut().request += 1;
                        files_state.borrow_mut().selected = None;
                        photos_state.borrow_mut().request += 1;
                        photos_state.borrow_mut().selected = None;
                        preview.set_paintable(None::<&gtk4::gdk::Paintable>);
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
                                    let folder = item.kind == "folder" || item.kind == "album";
                                    let image = gtk4::Image::from_icon_name(if folder {
                                        "folder-symbolic"
                                    } else {
                                        "text-x-generic-symbolic"
                                    });
                                    image.set_pixel_size(24);
                                    line.append(&image);
                                    let label = gtk4::Label::new(Some(&item.name));
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
                    Event::Photos(request, cursor, result) => {
                        if request != photos_state.borrow().request {
                            continue;
                        }
                        match result {
                            Ok(page) => {
                                back_to_albums.set_visible(false);
                                let append = cursor.is_some();
                                show_photos(&timeline, &page.items, append);
                                let mut state = photos_state.borrow_mut();
                                if !append {
                                    state.items.clear();
                                }
                                state.items.extend(page.items);
                                state.next_cursor = page.next_cursor;
                                load_more.set_visible(state.next_cursor.is_some());
                                photos_status.set_text(if state.items.is_empty() {
                                    "No photos in the timeline."
                                } else {
                                    "Select a photo to preview it."
                                });
                                photos_views.set_visible_child_name("timeline");
                            }
                            Err(error) => {
                                photos_status.set_text(&format!("Could not load photos: {error}"))
                            }
                        }
                    }
                    Event::Albums(request, result) => {
                        if request != photos_state.borrow().request {
                            continue;
                        }
                        match result {
                            Ok(result) => {
                                back_to_albums.set_visible(false);
                                load_more.set_visible(false);
                                while let Some(child) = albums.first_child() {
                                    albums.remove(&child);
                                }
                                album_items.replace(result.albums);
                                for album in album_items.borrow().iter() {
                                    let row = gtk4::ListBoxRow::new();
                                    row.set_child(Some(&gtk4::Label::new(Some(&format!(
                                        "{} — {} photos",
                                        album.name.as_deref().unwrap_or("Untitled album"),
                                        album.photo_count
                                    )))));
                                    albums.append(&row);
                                }
                                photos_status.set_text(if album_items.borrow().is_empty() {
                                    "No albums yet."
                                } else {
                                    "Double-click an album to open it."
                                });
                                photos_views.set_visible_child_name("albums");
                            }
                            Err(error) => {
                                photos_status.set_text(&format!("Could not load albums: {error}"))
                            }
                        }
                    }
                    Event::Album(request, result) => {
                        if request != photos_state.borrow().request {
                            continue;
                        }
                        match result {
                            Ok(result) => {
                                back_to_albums.set_visible(true);
                                load_more.set_visible(false);
                                photos_status.set_text(&format!(
                                    "{} — {} photos",
                                    result.album.name.as_deref().unwrap_or("Album"),
                                    result.items.len()
                                ));
                                photos_state.borrow_mut().items = result.items.clone();
                                show_photos(&album_photos, &result.items, false);
                                photos_views.set_visible_child_name("album");
                            }
                            Err(error) => {
                                photos_status.set_text(&format!("Could not open album: {error}"))
                            }
                        }
                    }
                    Event::Preview(uid, result) => {
                        if photos_state.borrow().selected.as_deref() != Some(&uid) {
                            continue;
                        }
                        match result {
                            Ok(bytes) => {
                                let loader = gtk4::gdk_pixbuf::PixbufLoader::new();
                                match loader.write(&bytes).and_then(|_| loader.close()) {
                                    Ok(()) => {
                                        if let Some(pixbuf) = loader.pixbuf() {
                                            preview.set_paintable(Some(
                                                &gtk4::gdk::Texture::for_pixbuf(&pixbuf),
                                            ));
                                            photos_status.set_text("Preview ready.");
                                        } else {
                                            photos_status.set_text("Preview image is empty.");
                                        }
                                    }
                                    Err(error) => photos_status
                                        .set_text(&format!("Could not decode preview: {error}")),
                                }
                            }
                            Err(error) => {
                                photos_status.set_text(&format!("Could not load preview: {error}"))
                            }
                        }
                    }
                    Event::Done(action, result) => {
                        let routine = result.is_ok()
                            && (action == "Status"
                                || action == "Computers"
                                || action.starts_with("Share status ")
                                || action.starts_with("Share link ")
                                || action.starts_with("Photo web ")
                                || action.starts_with("Files url "));
                        let message = match result {
                            Ok(text) => {
                                if action == "Status" {
                                    match serde_json::from_str::<Status>(&text) {
                                        Ok(status) => {
                                            while let Some(child) = transfer_list.first_child() {
                                                transfer_list.remove(&child);
                                            }
                                            if status.transfers.is_empty() {
                                                transfer_list.append(&gtk4::Label::new(Some(
                                                    "No active transfers.",
                                                )));
                                            }
                                            for transfer in status.transfers {
                                                let label = gtk4::Label::new(Some(&format!(
                                                    "{}: {} — {} / {} bytes",
                                                    transfer.direction,
                                                    transfer.filename,
                                                    transfer.bytes_transferred,
                                                    transfer.total_bytes
                                                )));
                                                label.set_halign(gtk4::Align::Start);
                                                label
                                                    .set_ellipsize(gtk4::pango::EllipsizeMode::End);
                                                transfer_list.append(&label);
                                                let progress = gtk4::ProgressBar::new();
                                                if transfer.total_bytes > 0 {
                                                    progress.set_fraction(
                                                        (transfer.bytes_transferred as f64
                                                            / transfer.total_bytes as f64)
                                                            .clamp(0.0, 1.0),
                                                    );
                                                }
                                                transfer_list.append(&progress);
                                                if transfer.cancellable {
                                                    let tx = tx.clone();
                                                    let id = transfer.id;
                                                    button(
                                                        "Cancel download",
                                                        &transfer_list,
                                                        move || {
                                                            command(
                                                                &tx,
                                                                "Cancel download",
                                                                vec![
                                                                    "cancel-transfer".into(),
                                                                    id.to_string(),
                                                                ],
                                                            );
                                                        },
                                                    );
                                                }
                                            }
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
                                            notifications.add_toast(adw::Toast::new(&message));
                                        }
                                    }
                                } else if action == "Computers" {
                                    match serde_json::from_str::<ComputerList>(&text) {
                                        Ok(snapshot) => {
                                            while let Some(child) = computer_rows.first_child() {
                                                computer_rows.remove(&child);
                                            }
                                            computers_status.set_text(if snapshot.computers.is_empty() {
                                                "No computers registered. Add a backup to register this device."
                                            } else {
                                                "Registered computers and local folders:"
                                            });
                                            for computer in snapshot.computers {
                                                let group = section(
                                                    &format!(
                                                        "{}{}",
                                                        computer.name,
                                                        if snapshot.this_device_id.as_deref()
                                                            == Some(&computer.id)
                                                        {
                                                            " (this computer)"
                                                        } else {
                                                            ""
                                                        }
                                                    ),
                                                    &computer
                                                        .last_sync_time
                                                        .map_or("Not synced yet".into(), |time| {
                                                            format!("Last sync: {time}")
                                                        }),
                                                );
                                                for job in snapshot
                                                    .jobs
                                                    .iter()
                                                    .filter(|job| job.device_id == computer.id)
                                                {
                                                    let label = gtk4::Label::new(Some(&format!(
                                                        "{}  ·  {}",
                                                        job.name, job.local_path
                                                    )));
                                                    label.set_halign(gtk4::Align::Start);
                                                    label.set_selectable(true);
                                                    label.set_wrap(true);
                                                    group.append(&label);
                                                }
                                                computer_rows.append(&group);
                                            }
                                        }
                                        Err(error) => computers_status
                                            .set_text(&format!("Invalid computer list: {error}")),
                                    }
                                } else if action == "Mount" || action == "Stop" {
                                    refresh_status(&tx);
                                } else if action == "Cancel download" {
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
                                    profile_name.set_text("Account");
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
                                } else if let Some(uid) = action.strip_prefix("Photo web ") {
                                    if text.starts_with("https://")
                                        && photos_state.borrow().selected.as_deref() == Some(uid)
                                    {
                                        photo_web.set_uri(&text);
                                        photo_web.set_visible(true);
                                    }
                                } else if let Some(uid) = action.strip_prefix("Share status ") {
                                    if files_state
                                        .borrow()
                                        .selected
                                        .as_ref()
                                        .is_some_and(|item| item.uid == uid)
                                    {
                                        match serde_json::from_str::<Option<SharingInfo>>(&text) {
                                            Ok(Some(info)) => {
                                                let members = info
                                                    .members
                                                    .iter()
                                                    .map(|member| {
                                                        format!(
                                                            "{} ({})",
                                                            member.email, member.role
                                                        )
                                                    })
                                                    .collect::<Vec<_>>();
                                                sharing_status.set_text(&format!(
                                                    "{} member(s), {} pending invitation(s){}{}",
                                                    members.len(),
                                                    info.pending_invitations,
                                                    if members.is_empty() {
                                                        String::new()
                                                    } else {
                                                        format!("\n{}", members.join("\n"))
                                                    },
                                                    info.public_link.as_ref().map_or_else(
                                                        || "\nNo public link".to_string(),
                                                        |link| format!(
                                                            "\nPublic link: {}{}",
                                                            link.role,
                                                            link.expires.as_ref().map_or_else(
                                                                String::new,
                                                                |date| format!(" · expires {date}")
                                                            )
                                                        )
                                                    )
                                                ));
                                                if let Some(link) = info
                                                    .public_link
                                                    .filter(|link| link.url.starts_with("https://"))
                                                {
                                                    share_link.set_uri(&link.url);
                                                    share_link.set_visible(true);
                                                } else {
                                                    share_link.set_visible(false);
                                                }
                                            }
                                            Ok(None) => {
                                                sharing_status.set_text("Not shared.");
                                                share_link.set_visible(false);
                                            }
                                            Err(error) => sharing_status.set_text(&format!(
                                                "Invalid sharing response: {error}"
                                            )),
                                        }
                                    }
                                } else if let Some(uid) = action.strip_prefix("Share link ") {
                                    if files_state
                                        .borrow()
                                        .selected
                                        .as_ref()
                                        .is_some_and(|item| item.uid == uid)
                                    {
                                        if text.starts_with("https://") {
                                            share_link.set_uri(&text);
                                            share_link.set_visible(true);
                                            sharing_status.set_text("Public link ready.");
                                        } else {
                                            sharing_status
                                                .set_text("Invalid public link returned by pdcli.");
                                        }
                                    }
                                } else if let Some(uid) = action.strip_prefix("Share remove ") {
                                    if files_state
                                        .borrow()
                                        .selected
                                        .as_ref()
                                        .is_some_and(|item| item.uid == uid)
                                    {
                                        share_link.set_visible(false);
                                        command(
                                            &tx,
                                            &format!("Share status {uid}"),
                                            vec![
                                                "share".into(),
                                                "status".into(),
                                                uid.into(),
                                                "--json".into(),
                                            ],
                                        );
                                    }
                                } else if let Some(uid) = action
                                    .strip_prefix("Share invite ")
                                    .or_else(|| action.strip_prefix("Share revoke "))
                                {
                                    if files_state
                                        .borrow()
                                        .selected
                                        .as_ref()
                                        .is_some_and(|item| item.uid == uid)
                                    {
                                        command(
                                            &tx,
                                            &format!("Share status {uid}"),
                                            vec![
                                                "share".into(),
                                                "status".into(),
                                                uid.into(),
                                                "--json".into(),
                                            ],
                                        );
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
                                    command(
                                        &tx,
                                        "Computers",
                                        vec!["computers".into(), "--json".into()],
                                    );
                                }
                                format!("{action}: {text}")
                            }
                            Err(error) => {
                                if action == "Computers" {
                                    computers_status
                                        .set_text(&format!("Could not load computers: {error}"));
                                }
                                if action == "Status" {
                                    status_text
                                        .set_text(&format!("Could not check status: {error}"));
                                    mount_text.set_text(&format!("Could not check mount: {error}"));
                                }
                                if action.starts_with("Files ") {
                                    files_status.set_text(&format!("{action} failed: {error}"));
                                }
                                if action.starts_with("Share ") {
                                    sharing_status.set_text(&format!("{action} failed: {error}"));
                                }
                                if action.starts_with("Photo ") {
                                    photos_status.set_text(&format!("{action} failed: {error}"));
                                }
                                if action == "Sign in" {
                                    login_details.set_text(&format!("Sign-in failed: {error}"));
                                    login_button.set_sensitive(true);
                                }
                                format!("{action} failed: {error}")
                            }
                        };
                        if !routine {
                            notifications.add_toast(adw::Toast::new(&message));
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
                r#"{"signed_in":true,"username":"user","daemon":"paused","mountpoint":"/tmp/drive","mounted":true,"journal":{"pending":2,"failed":1,"entries":[]},"transfers":[]}"#,
            )
            .unwrap();
            assert_eq!(status.daemon, "paused");
            assert!(status.mounted);
            assert_eq!(status.mountpoint, "/tmp/drive");
            assert_eq!(status.journal.unwrap().failed, 1);
        }

        #[test]
        fn sharing_response_parses() {
            let info: Option<SharingInfo> = serde_json::from_str(
                r#"{"members":[{"email":"a@example.com","role":"viewer"}],"pending_invitations":1,"public_link":{"url":"https://example.com/link","role":"viewer","expires":null}}"#,
            )
            .unwrap();
            assert_eq!(info.unwrap().members.len(), 1);
        }

        #[test]
        fn computer_listing_parses_jobs() {
            let snapshot: ComputerList = serde_json::from_str(
                r#"{"this_device_id":"device","computers":[{"id":"device","name":"Laptop","last_sync_time":null}],"jobs":[{"id":"job","name":"Documents","local_path":"/home/user/Documents","device_id":"device"}]}"#,
            )
            .unwrap();
            assert_eq!(snapshot.computers[0].name, "Laptop");
            assert_eq!(snapshot.jobs[0].name, "Documents");
        }

        #[test]
        fn older_desktop_settings_keep_files_as_default_page() {
            let settings: Preferences = serde_json::from_str(r#"{"auto_mount":false}"#).unwrap();
            assert!(!settings.auto_mount);
            assert_eq!(settings.start_page, "files");
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
