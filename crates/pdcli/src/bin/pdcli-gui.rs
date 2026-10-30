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
        collections::HashMap,
        io::{BufRead, BufReader, Read, Write},
        path::PathBuf,
        process::{Command, Stdio},
        rc::Rc,
        sync::{
            OnceLock,
            mpsc::{self, Sender},
        },
        time::Duration,
    };

    #[derive(Parser)]
    #[command(
        name = "pdcli-gui",
        version,
        about = "pdcli Desktop — unofficial Proton Drive desktop app",
        after_help = "Launch with `pdcli gui`. Use `pdcli --help` for command-line operations. Both executables must be installed together."
    )]
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
        #[serde(default)]
        available_offline: Option<bool>,
        #[serde(default)]
        web_url: Option<String>,
    }

    #[derive(Deserialize)]
    struct FileListing {
        folder: Folder,
        items: Vec<FileItem>,
    }

    #[derive(Deserialize)]
    struct Status {
        signed_in: bool,
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
        #[serde(default)]
        tags: Option<Vec<u8>>,
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
        next_cursor: Option<String>,
    }

    #[derive(Deserialize)]
    struct AlbumSelection {
        uid: String,
        name: Option<String>,
    }

    #[derive(Deserialize)]
    struct AlbumDetails {
        album: AlbumSelection,
        items: Vec<PhotoItem>,
        next_cursor: Option<String>,
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
        albums_cursor: Option<String>,
        album_cursor: Option<String>,
        album_uid: Option<String>,
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
        Albums(u64, Option<String>, Result<AlbumList, String>),
        Album(u64, String, Option<String>, Result<AlbumDetails, String>),
        /// A photo UID's thumbnail image, or `None` when it could not be fetched.
        Thumbnail(String, Option<Vec<u8>>),
        PhotoDownloaded(Result<PathBuf, String>),
    }

    /// Thumbnails stay in memory only: they are decrypted photo content.
    #[derive(Default)]
    struct PhotoThumbnails {
        textures: HashMap<String, gtk4::gdk::Texture>,
        waiting: HashMap<String, Vec<gtk4::glib::WeakRef<gtk4::Image>>>,
    }

    impl PhotoThumbnails {
        fn received(&mut self, uid: String, bytes: Option<Vec<u8>>) {
            let images = self.waiting.remove(&uid).unwrap_or_default();
            let Some(texture) = bytes.and_then(|bytes| {
                gtk4::gdk::Texture::from_bytes(&gtk4::glib::Bytes::from_owned(bytes)).ok()
            }) else {
                return;
            };
            for image in images.iter().filter_map(|image| image.upgrade()) {
                image.set_paintable(Some(&texture));
            }
            self.textures.insert(uid, texture);
        }
    }

    /// Streams thumbnails from one `photos thumbnails` process, so rows fill in as
    /// each batch arrives instead of after the whole page.
    fn load_thumbnails(tx: &Sender<Event>, uids: Vec<String>) {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut answered = std::collections::HashSet::new();
            let result = (|| -> Result<(), String> {
                let binary = std::env::current_exe()
                    .map_err(|e| e.to_string())?
                    .with_file_name("pdcli");
                let mut child = Command::new(binary)
                    .args(CLI_FLAGS.get().expect("CLI flags initialized"))
                    .args(["photos", "thumbnails"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| format!("Could not start pdcli: {e}"))?;
                let mut stdin = child.stdin.take().expect("requested piped stdin");
                let input = uids.join("\n");
                std::thread::spawn(move || {
                    let _ = stdin.write_all(input.as_bytes());
                });
                let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));
                let mut header = String::new();
                while reader.read_line(&mut header).map_err(|e| e.to_string())? > 0 {
                    #[derive(Deserialize)]
                    struct Header {
                        uid: String,
                        size: Option<usize>,
                    }
                    let parsed: Header =
                        serde_json::from_str(header.trim()).map_err(|e| e.to_string())?;
                    header.clear();
                    let bytes = match parsed.size {
                        Some(size) => {
                            let mut bytes = vec![0; size];
                            reader.read_exact(&mut bytes).map_err(|e| e.to_string())?;
                            Some(bytes)
                        }
                        None => None,
                    };
                    answered.insert(parsed.uid.clone());
                    let _ = tx.send(Event::Thumbnail(parsed.uid, bytes));
                }
                let _ = child.wait();
                Ok(())
            })();
            if let Err(error) = result {
                eprintln!("Could not load photo thumbnails: {error}");
            }
            // Rows still waiting keep their placeholder icon.
            for uid in uids.into_iter().filter(|uid| !answered.contains(uid)) {
                let _ = tx.send(Event::Thumbnail(uid, None));
            }
        });
    }

    /// Downloads a photo's original only when the user opens it. The copy lives in
    /// the per-user runtime directory, which is private and cleared at logout.
    fn open_photo(tx: &Sender<Event>, item: &PhotoItem) {
        let tx = tx.clone();
        let uid = item.uid.clone();
        let name = item
            .name
            .clone()
            .filter(|name| valid_name(name))
            .unwrap_or_else(|| "photo".into());
        std::thread::spawn(move || {
            let folder: String = uid
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            let directory = gtk4::glib::user_runtime_dir()
                .join("pdcli-photos")
                .join(folder);
            let path = directory.join(name);
            let result = if path.exists() {
                Ok(path)
            } else {
                std::fs::create_dir_all(&directory)
                    .map_err(|error| error.to_string())
                    .and_then(|_| {
                        run_command(&[
                            "photos".into(),
                            "download".into(),
                            uid,
                            path.to_string_lossy().into_owned(),
                        ])
                    })
                    .map(|_| path)
            };
            let _ = tx.send(Event::PhotoDownloaded(result));
        });
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

    /// Pseudo-folder UID for the flat list of files kept on this device.
    const OFFLINE_VIEW: &str = "pdcli:available-offline";

    fn offline_view() -> Folder {
        Folder {
            uid: OFFLINE_VIEW.into(),
            name: "Available Offline".into(),
        }
    }

    fn load_offline_files() -> Result<FileListing, String> {
        #[derive(Deserialize)]
        struct OfflineFile {
            uid: Option<String>,
            path: String,
            size: i64,
        }
        let text = run_command(&["offline".into(), "list".into(), "--json".into()])?;
        let files: Vec<OfflineFile> = serde_json::from_str(&text)
            .map_err(|e| format!("Invalid offline files response: {e}"))?;
        Ok(FileListing {
            folder: offline_view(),
            items: files
                .into_iter()
                // Files never uploaded have no UID and nothing to free.
                .filter_map(|file| {
                    Some(FileItem {
                        uid: file.uid?,
                        name: file.path,
                        kind: "file".into(),
                        size: Some(file.size),
                        degraded: false,
                        error: None,
                        available_offline: Some(true),
                        web_url: None,
                    })
                })
                .collect(),
        })
    }

    fn load_files(tx: &Sender<Event>, state: &Rc<RefCell<FilesState>>, trail: Vec<Folder>) {
        let mut state = state.borrow_mut();
        state.request += 1;
        let request = state.request;
        drop(state);
        let tx = tx.clone();
        std::thread::spawn(move || {
            if trail
                .last()
                .is_some_and(|folder| folder.uid == OFFLINE_VIEW)
            {
                let _ = tx.send(Event::Files(request, trail, load_offline_files()));
                return;
            }
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

    fn load_albums(tx: &Sender<Event>, state: &Rc<RefCell<PhotosState>>, cursor: Option<String>) {
        let mut state = state.borrow_mut();
        state.request += 1;
        let request = state.request;
        drop(state);
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut args = vec!["photos".into(), "albums".into()];
            if let Some(cursor) = &cursor {
                args.extend(["--cursor".into(), cursor.clone()]);
            }
            let result = run_command(&args).and_then(|text| {
                serde_json::from_str(&text)
                    .map_err(|error| format!("Invalid albums response: {error}"))
            });
            let _ = tx.send(Event::Albums(request, cursor, result));
        });
    }

    fn load_album(
        tx: &Sender<Event>,
        state: &Rc<RefCell<PhotosState>>,
        uid: String,
        cursor: Option<String>,
    ) {
        let mut state = state.borrow_mut();
        state.request += 1;
        let request = state.request;
        drop(state);
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut args = vec!["photos".into(), "album".into(), uid.clone()];
            if let Some(cursor) = &cursor {
                args.extend(["--cursor".into(), cursor.clone()]);
            }
            let result = run_command(&args).and_then(|text| {
                serde_json::from_str(&text)
                    .map_err(|error| format!("Invalid album response: {error}"))
            });
            let _ = tx.send(Event::Album(request, uid, cursor, result));
        });
    }

    const SIDEBAR_PAGES: [(&str, &str, &str); 4] = [
        ("files", "Files", "folder-symbolic"),
        ("photos", "Photos", "image-x-generic-symbolic"),
        ("computers", "Computers", "computer-symbolic"),
        ("status", "Status", "emblem-synchronizing-symbolic"),
    ];

    fn page_title(page: &str) -> &'static str {
        match page {
            "files" => "Files",
            "photos" => "Photos",
            "computers" => "Computers",
            "status" => "Status",
            "account" => "Account",
            "settings" => "Preferences",
            _ => "pdcli Desktop",
        }
    }

    fn button(label: &str, callback: impl Fn() + 'static) -> gtk4::Button {
        let button = gtk4::Button::with_label(label);
        button.set_valign(gtk4::Align::Center);
        button.connect_clicked(move |_| callback());
        button
    }

    fn icon_button(icon: &str, tooltip: &str, callback: impl Fn() + 'static) -> gtk4::Button {
        let button = gtk4::Button::from_icon_name(icon);
        button.set_tooltip_text(Some(tooltip));
        button.add_css_class("flat");
        button.set_valign(gtk4::Align::Center);
        button.connect_clicked(move |_| callback());
        button
    }

    fn row() -> gtk4::Box {
        gtk4::Box::new(gtk4::Orientation::Horizontal, 6)
    }

    fn group(title: &str, description: &str) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::new();
        group.set_title(title);
        if !description.is_empty() {
            group.set_description(Some(description));
        }
        group
    }

    /// An action row for text that comes from Proton or the CLI, so it is never
    /// parsed as Pango markup.
    fn info_row(title: &str, subtitle: &str) -> adw::ActionRow {
        let row = adw::ActionRow::new();
        row.set_use_markup(false);
        row.set_title(title);
        row.set_subtitle(subtitle);
        row
    }

    fn entry_row(title: &str) -> adw::EntryRow {
        let entry = adw::EntryRow::new();
        entry.set_title(title);
        entry
    }

    fn toast(text: &str) -> adw::Toast {
        let toast = adw::Toast::new("");
        toast.set_use_markup(false);
        toast.set_title(text);
        toast
    }

    fn caption(text: &str) -> gtk4::Label {
        let label = gtk4::Label::new(Some(text));
        label.add_css_class("dim-label");
        label.set_halign(gtk4::Align::Start);
        label.set_xalign(0.0);
        label.set_wrap(true);
        label
    }

    fn boxed_list() -> gtk4::ListBox {
        let list = gtk4::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk4::SelectionMode::None);
        list.set_valign(gtk4::Align::Start);
        list
    }

    fn clear_list(list: &gtk4::ListBox) {
        while let Some(row) = list.row_at_index(0) {
            list.remove(&row);
        }
    }

    fn scroll(widget: &impl IsA<gtk4::Widget>) -> gtk4::ScrolledWindow {
        let scrolled = gtk4::ScrolledWindow::new();
        scrolled.set_child(Some(widget));
        scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scrolled.set_vexpand(true);
        scrolled
    }

    /// Scrollable page content held to a readable width, like `AdwPreferencesPage`.
    fn clamped(widget: &impl IsA<gtk4::Widget>) -> gtk4::ScrolledWindow {
        let clamp = adw::Clamp::new();
        clamp.set_maximum_size(860);
        clamp.set_child(Some(widget));
        clamp.set_margin_start(12);
        clamp.set_margin_end(12);
        clamp.set_margin_top(12);
        clamp.set_margin_bottom(24);
        scroll(&clamp)
    }

    fn capitalized(text: &str) -> String {
        let mut chars = text.chars();
        chars.next().map_or_else(String::new, |first| {
            first.to_uppercase().chain(chars).collect()
        })
    }

    fn plural(count: usize, one: &str, many: &str) -> String {
        format!("{count} {}", if count == 1 { one } else { many })
    }

    fn capture_label(time: &str) -> String {
        gtk4::glib::DateTime::from_iso8601(time, None)
            .and_then(|time| time.to_local())
            .and_then(|time| time.format("%e %B %Y, %H:%M"))
            .map(|text| text.trim().to_string())
            .unwrap_or_else(|_| time.to_string())
    }

    fn transfer_label(transfer: &Transfer) -> String {
        let verb = match transfer.direction.as_str() {
            "upload" => "Uploading",
            "download" => "Downloading",
            other => return capitalized(other),
        };
        let done = gtk4::glib::format_size(transfer.bytes_transferred.max(0) as u64);
        if transfer.total_bytes > 0 {
            let total = gtk4::glib::format_size(transfer.total_bytes as u64);
            format!("{verb}, {done} of {total}")
        } else {
            format!("{verb}, {done}")
        }
    }

    fn sharing_summary(info: &SharingInfo) -> String {
        let mut lines = vec![match info.members.len() {
            0 => "Not shared with anyone".to_string(),
            count => format!("Shared with {}", plural(count, "person", "people")),
        }];
        if info.pending_invitations > 0 {
            lines[0].push_str(&format!(
                ", {}",
                plural(
                    info.pending_invitations,
                    "pending invitation",
                    "pending invitations"
                )
            ));
        }
        lines.extend(
            info.members
                .iter()
                .map(|member| format!("{} ({})", member.email, member.role)),
        );
        lines.push(info.public_link.as_ref().map_or_else(
            || "No public link".to_string(),
            |link| {
                format!(
                    "Public link for {}s{}",
                    link.role,
                    link.expires
                        .as_ref()
                        .map_or_else(String::new, |date| format!(", expires {date}"))
                )
            },
        ));
        lines.join("\n")
    }

    fn file_icon(kind: &str) -> &'static str {
        match kind {
            "folder" => "folder-symbolic",
            "album" => "folder-pictures-symbolic",
            "photo" => "image-x-generic-symbolic",
            "document" => "x-office-document-symbolic",
            "spreadsheet" => "x-office-spreadsheet-symbolic",
            _ => "text-x-generic-symbolic",
        }
    }

    fn open_uri(uri: &str) {
        if let Err(error) =
            gtk4::gio::AppInfo::launch_default_for_uri(uri, None::<&gtk4::gio::AppLaunchContext>)
        {
            eprintln!("Could not open {uri}: {error}");
        }
    }

    fn show_photos(
        list: &gtk4::ListBox,
        items: &[PhotoItem],
        append: bool,
        thumbnails: &Rc<RefCell<PhotoThumbnails>>,
        tx: &Sender<Event>,
    ) {
        if !append {
            clear_list(list);
        }
        let mut requested = Vec::new();
        for item in items {
            let row = info_row(
                item.name.as_deref().unwrap_or("Untitled photo"),
                &capture_label(&item.capture_time),
            );
            row.set_activatable(true);
            row.set_tooltip_text(Some("Double-click to open the original"));
            let image = gtk4::Image::from_icon_name("image-x-generic-symbolic");
            image.set_pixel_size(56);
            image.set_margin_top(4);
            image.set_margin_bottom(4);
            let mut thumbnails = thumbnails.borrow_mut();
            if let Some(texture) = thumbnails.textures.get(&item.uid) {
                image.set_paintable(Some(texture));
            } else {
                let waiting = thumbnails.waiting.entry(item.uid.clone()).or_default();
                if waiting.is_empty() {
                    requested.push(item.uid.clone());
                }
                waiting.push(image.downgrade());
            }
            row.add_prefix(&image);
            if item.tags.as_ref().is_some_and(|tags| tags.contains(&0)) {
                let star = gtk4::Image::from_icon_name("starred-symbolic");
                star.set_tooltip_text(Some("Favorite"));
                row.add_suffix(&star);
            }
            list.append(&row);
        }
        if !requested.is_empty() {
            load_thumbnails(tx, requested);
        }
    }

    fn valid_name(name: &str) -> bool {
        !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
    }

    fn name_dialog(
        window: &impl IsA<gtk4::Window>,
        heading: &str,
        confirm: &str,
        initial: &str,
        apply: impl Fn(String) + 'static,
    ) {
        let dialog = adw::MessageDialog::new(Some(window), Some(heading), None);
        dialog.add_responses(&[("cancel", "Cancel"), ("apply", confirm)]);
        dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("apply"));
        dialog.set_close_response("cancel");
        let entry = entry_row("Name");
        entry.set_text(initial);
        entry.set_activates_default(true);
        let list = boxed_list();
        list.append(&entry);
        dialog.set_extra_child(Some(&list));
        dialog.set_response_enabled("apply", valid_name(initial.trim()));
        entry.connect_changed({
            let dialog = dialog.clone();
            move |entry| dialog.set_response_enabled("apply", valid_name(entry.text().trim()))
        });
        dialog.connect_response(Some("apply"), move |_, _| {
            let name = entry.text().trim().to_string();
            if valid_name(&name) {
                apply(name);
            }
        });
        dialog.present();
    }

    fn confirm_dialog(
        window: &impl IsA<gtk4::Window>,
        heading: &str,
        body: &str,
        confirm: &str,
        apply: impl Fn() + 'static,
    ) {
        let dialog = adw::MessageDialog::new(Some(window), Some(heading), Some(body));
        dialog.add_responses(&[("cancel", "Cancel"), ("confirm", confirm)]);
        dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        dialog.connect_response(Some("confirm"), move |_, _| apply());
        dialog.present();
    }

    fn present_about(window: &adw::ApplicationWindow) {
        adw::AboutWindow::builder()
            .transient_for(window)
            .modal(true)
            .application_name("pdcli Desktop")
            .application_icon("folder-remote")
            .version(env!("CARGO_PKG_VERSION"))
            .comments(
                "An unofficial Proton Drive desktop app for Linux.\n\nNot affiliated with or supported by Proton.",
            )
            .website(env!("CARGO_PKG_REPOSITORY"))
            .issue_url(concat!(env!("CARGO_PKG_REPOSITORY"), "/issues"))
            .license_type(gtk4::License::MitX11)
            .build()
            .present();
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
            .title("pdcli Desktop")
            .default_width(960)
            .default_height(680)
            .width_request(360)
            .height_request(294)
            .build();
        let notifications = adw::ToastOverlay::new();
        let root = gtk4::Stack::new();
        root.set_transition_type(gtk4::StackTransitionType::Crossfade);

        let login_details = gtk4::Label::new(Some("Checking for a saved account…"));
        login_details.add_css_class("dim-label");
        login_details.set_selectable(true);
        login_details.set_wrap(true);
        login_details.set_justify(gtk4::Justification::Center);
        let login_button = gtk4::Button::with_label("Sign In With Browser");
        login_button.add_css_class("pill");
        login_button.add_css_class("suggested-action");
        login_button.set_halign(gtk4::Align::Center);
        login_button.set_sensitive(false);
        login_button.connect_clicked({
            let tx = tx.clone();
            let details = login_details.clone();
            move |button| {
                button.set_sensitive(false);
                details.set_text("Waiting for browser sign-in…");
                login(&tx);
            }
        });
        let login_link =
            gtk4::LinkButton::with_label("https://account.proton.me", "Open Sign-In Page");
        login_link.set_halign(gtk4::Align::Center);
        login_link.set_visible(false);
        let sign_in_actions = gtk4::Box::new(gtk4::Orientation::Vertical, 18);
        sign_in_actions.append(&login_button);
        sign_in_actions.append(&login_link);
        sign_in_actions.append(&login_details);
        let sign_in_status = adw::StatusPage::builder()
            .icon_name("folder-remote-symbolic")
            .title("Sign In to Proton Drive")
            .description(
                "pdcli is an unofficial Proton Drive client. Sign in securely in your browser to continue.",
            )
            .child(&adw::Clamp::builder().maximum_size(420).child(&sign_in_actions).build())
            .vexpand(true)
            .build();
        let sign_in = adw::ToolbarView::new();
        let sign_in_header = adw::HeaderBar::new();
        sign_in_header.set_show_title(false);
        sign_in.add_top_bar(&sign_in_header);
        sign_in.set_content(Some(&sign_in_status));
        root.add_named(&sign_in, Some("login"));

        let pages = gtk4::Stack::new();
        pages.set_transition_type(gtk4::StackTransitionType::Crossfade);
        pages.set_hexpand(true);
        pages.set_vexpand(true);
        let sidebar_list = gtk4::ListBox::new();
        sidebar_list.add_css_class("navigation-sidebar");
        for (_, title, icon) in SIDEBAR_PAGES {
            let line = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
            line.append(&gtk4::Image::from_icon_name(icon));
            let label = gtk4::Label::new(Some(title));
            label.set_halign(gtk4::Align::Start);
            line.append(&label);
            sidebar_list.append(&line);
        }
        let primary_menu = gtk4::gio::Menu::new();
        let account_section = gtk4::gio::Menu::new();
        account_section.append(Some("Account"), Some("win.account"));
        account_section.append(Some("Preferences"), Some("win.preferences"));
        primary_menu.append_section(None, &account_section);
        let app_section = gtk4::gio::Menu::new();
        app_section.append(Some("About pdcli Desktop"), Some("win.about"));
        app_section.append(Some("Quit"), Some("app.quit"));
        primary_menu.append_section(None, &app_section);
        let menu_button = gtk4::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .tooltip_text("Main Menu")
            .menu_model(&primary_menu)
            .build();
        let sidebar_title = adw::WindowTitle::new("Proton Drive", "");
        let sidebar_header = adw::HeaderBar::new();
        sidebar_header.set_title_widget(Some(&sidebar_title));
        sidebar_header.pack_end(&menu_button);
        let sidebar_view = adw::ToolbarView::new();
        sidebar_view.add_top_bar(&sidebar_header);
        sidebar_view.set_content(Some(&scroll(&sidebar_list)));
        let sidebar_page = adw::NavigationPage::new(&sidebar_view, "pdcli Desktop");

        let content_title = adw::WindowTitle::new("Files", "");
        let content_header = adw::HeaderBar::new();
        content_header.set_title_widget(Some(&content_title));
        let files_start = row();
        let files_end = row();
        content_header.pack_start(&files_start);
        content_header.pack_end(&files_end);
        let content_view = adw::ToolbarView::new();
        content_view.add_top_bar(&content_header);
        content_view.set_content(Some(&pages));
        let content_page = adw::NavigationPage::new(&content_view, "Files");
        let split_view = adw::NavigationSplitView::new();
        split_view.set_sidebar(Some(&sidebar_page));
        split_view.set_content(Some(&content_page));
        root.add_named(&split_view, Some("main"));
        root.set_visible_child_name("login");

        let show_page: Rc<dyn Fn(&str)> = Rc::new({
            let pages = pages.clone();
            let content_title = content_title.clone();
            let content_page = content_page.clone();
            let sidebar_list = sidebar_list.clone();
            let split_view = split_view.clone();
            let files_start = files_start.clone();
            let files_end = files_end.clone();
            move |page: &str| {
                pages.set_visible_child_name(page);
                content_title.set_title(page_title(page));
                content_page.set_title(page_title(page));
                files_start.set_visible(page == "files");
                files_end.set_visible(page == "files");
                match SIDEBAR_PAGES.iter().position(|(name, _, _)| *name == page) {
                    Some(index) => {
                        let row = sidebar_list.row_at_index(index as i32);
                        if sidebar_list.selected_row() != row {
                            sidebar_list.select_row(row.as_ref());
                        }
                    }
                    None => sidebar_list.unselect_all(),
                }
                split_view.set_show_content(true);
            }
        });
        sidebar_list.connect_row_activated({
            let show_page = show_page.clone();
            move |_, row| show_page(SIDEBAR_PAGES[row.index() as usize].0)
        });
        for (name, page) in [("account", "account"), ("preferences", "settings")] {
            let action = gtk4::gio::SimpleAction::new(name, None);
            let show_page = show_page.clone();
            action.connect_activate(move |_, _| show_page(page));
            window.add_action(&action);
        }
        let about_action = gtk4::gio::SimpleAction::new("about", None);
        about_action.connect_activate({
            let window = window.clone();
            move |_, _| present_about(&window)
        });
        window.add_action(&about_action);
        let quit_action = gtk4::gio::SimpleAction::new("quit", None);
        quit_action.connect_activate({
            let app = app.clone();
            move |_, _| app.quit()
        });
        app.add_action(&quit_action);
        app.set_accels_for_action("app.quit", &["<Control>q"]);
        app.set_accels_for_action("win.preferences", &["<Control>comma"]);

        let status = adw::PreferencesPage::new();
        let mount_group = group(
            "Drive Folder",
            "Mount Proton Drive to use it from your file manager.",
        );
        let mount_row = info_row("Mount Point", "Checking mount status…");
        mount_row.set_subtitle_selectable(true);
        let mut mount_buttons = Vec::new();
        for (label, action, argument) in [
            ("Open", "Open", "open"),
            ("Mount", "Mount", "mount"),
            ("Stop", "Stop", "stop"),
        ] {
            let tx = tx.clone();
            let button = button(label, move || {
                command(&tx, action, vec![argument.into()]);
            });
            mount_row.add_suffix(&button);
            mount_buttons.push(button);
        }
        let [open_mount, mount_button, stop_mount] =
            <[gtk4::Button; 3]>::try_from(mount_buttons).expect("three mount buttons");
        mount_button.add_css_class("suggested-action");
        open_mount.set_sensitive(false);
        stop_mount.set_visible(false);
        mount_group.add(&mount_row);
        status.add(&mount_group);
        let sync_group = group(
            "Background Sync",
            "Changes are synced while Drive is mounted.",
        );
        sync_group.set_header_suffix(Some(&button("Sync Now", {
            let tx = tx.clone();
            move || command(&tx, "Retry", vec!["sync".into()])
        })));
        let daemon_row = info_row("Sync Service", "Checking sync status…");
        let pause = button("Pause", {
            let tx = tx.clone();
            move || command(&tx, "Pause", vec!["pause".into()])
        });
        let resume = button("Resume", {
            let tx = tx.clone();
            move || command(&tx, "Resume", vec!["resume".into()])
        });
        resume.set_visible(false);
        daemon_row.add_suffix(&pause);
        daemon_row.add_suffix(&resume);
        sync_group.add(&daemon_row);
        let queue_row = info_row("Queue", "Checking…");
        let retry_failed = button("Retry Failed", {
            let tx = tx.clone();
            move || command(&tx, "Retry failed", vec!["retry".into(), "--all".into()])
        });
        retry_failed.set_sensitive(false);
        queue_row.add_suffix(&retry_failed);
        sync_group.add(&queue_row);
        status.add(&sync_group);
        let transfer_group = group("Transfers", "Active downloads and uploads.");
        let transfer_list = boxed_list();
        let no_transfers = caption("No active transfers");
        no_transfers.set_halign(gtk4::Align::Center);
        no_transfers.set_margin_top(18);
        no_transfers.set_margin_bottom(18);
        transfer_list.set_placeholder(Some(&no_transfers));
        transfer_group.add(&transfer_list);
        status.add(&transfer_group);
        pages.add_named(&status, Some("status"));

        let files_state = Rc::new(RefCell::new(FilesState::default()));
        let files_status = caption("Sign in to browse files.");
        files_status.set_selectable(true);
        let back = icon_button("go-up-symbolic", "Parent Folder", {
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
        files_start.append(&back);
        let create = icon_button("folder-new-symbolic", "New Folder", {
            let state = files_state.clone();
            let tx = tx.clone();
            let window = window.clone();
            move || {
                if let Some(folder) = state.borrow().trail.last() {
                    let uid = folder.uid.clone();
                    let tx = tx.clone();
                    name_dialog(&window, "New Folder", "Create", "", move |name| {
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
        let files_refresh = icon_button("view-refresh-symbolic", "Refresh", {
            let state = files_state.clone();
            let tx = tx.clone();
            let status = files_status.clone();
            move || {
                let trail = state.borrow().trail.clone();
                status.set_text("Loading files…");
                load_files(&tx, &state, trail);
            }
        });
        let offline_filter = gtk4::ToggleButton::builder()
            .icon_name("folder-download-symbolic")
            .tooltip_text("Show Only Files Available Offline")
            .css_classes(["flat"])
            .build();
        offline_filter.connect_clicked({
            let state = files_state.clone();
            let tx = tx.clone();
            let status = files_status.clone();
            move |button| {
                status.set_text("Loading files…");
                let trail = if button.is_active() {
                    vec![offline_view()]
                } else {
                    Vec::new()
                };
                load_files(&tx, &state, trail);
            }
        });
        files_end.append(&offline_filter);
        files_end.append(&create);
        files_end.append(&files_refresh);
        let files = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        let breadcrumb = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        let breadcrumb_scroll = gtk4::ScrolledWindow::new();
        breadcrumb_scroll.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Never);
        breadcrumb_scroll.set_child(Some(&breadcrumb));
        files.append(&breadcrumb_scroll);
        let files_feedback = row();
        files_feedback.set_margin_start(6);
        let files_spinner = gtk4::Spinner::new();
        files_spinner.set_visible(false);
        files_feedback.append(&files_spinner);
        files_feedback.append(&files_status);
        files_status.connect_notify_local(Some("label"), move |label, _| {
            let loading = label.text().starts_with("Loading files");
            files_spinner.set_visible(loading);
            files_spinner.set_spinning(loading);
        });
        files.append(&files_feedback);
        let files_list = gtk4::ListBox::new();
        files_list.set_selection_mode(gtk4::SelectionMode::Single);
        files_list.add_css_class("boxed-list");
        files_list.set_valign(gtk4::Align::Start);
        files_list.set_visible(false);
        files.append(&files_list);

        let share_title = adw::WindowTitle::new("Share", "");
        let share_window = adw::Window::builder()
            .title("Share")
            .transient_for(&window)
            .modal(true)
            .hide_on_close(true)
            .default_width(480)
            .default_height(640)
            .build();
        let sharing_status = info_row("Current Access", "Checking current access…");
        sharing_status.set_subtitle_selectable(true);
        let share_link = gtk4::LinkButton::with_label("https://drive.proton.me", "Open Link");
        share_link.set_valign(gtk4::Align::Center);
        share_link.set_visible(false);
        sharing_status.add_suffix(&share_link);
        let check_sharing = {
            let state = files_state.clone();
            let tx = tx.clone();
            let sharing_status = sharing_status.clone();
            move || {
                if let Some(item) = state.borrow().selected.as_ref() {
                    sharing_status.set_subtitle("Checking current access…");
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
        };
        let share_header = adw::HeaderBar::new();
        share_header.set_title_widget(Some(&share_title));
        share_header.pack_start(&icon_button(
            "view-refresh-symbolic",
            "Check Sharing",
            check_sharing.clone(),
        ));
        let share_page = adw::PreferencesPage::new();
        let access_group = group("Access", "");
        access_group.add(&sharing_status);
        share_page.add(&access_group);
        let permission_group = group(
            "Permission",
            "Applies to the public link and new invitations.",
        );
        let share_role = adw::ComboRow::new();
        share_role.set_title("Role");
        share_role.set_model(Some(&gtk4::StringList::new(&["Viewer", "Editor"])));
        permission_group.add(&share_role);
        share_page.add(&permission_group);
        let link_group = group(
            "Public Link",
            "Updating a link replaces its role, password, and expiry. Blank fields remove existing limits.",
        );
        let share_password = adw::PasswordEntryRow::new();
        share_password.set_title("Password (Optional)");
        let share_expires = entry_row("Expires (RFC 3339, Optional)");
        link_group.add(&share_password);
        link_group.add(&share_expires);
        let link_buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        link_buttons.set_halign(gtk4::Align::End);
        link_buttons.set_margin_top(12);
        link_buttons.append(&button("Remove Link", {
            let state = files_state.clone();
            let tx = tx.clone();
            let share_window = share_window.clone();
            move || {
                if let Some(item) = state
                    .borrow()
                    .selected
                    .as_ref()
                    .filter(|item| !item.degraded)
                {
                    let uid = item.uid.clone();
                    let tx = tx.clone();
                    confirm_dialog(
                        &share_window,
                        "Remove Public Link?",
                        "Anyone using this link will lose access.",
                        "Remove Link",
                        move || {
                            command(
                                &tx,
                                &format!("Share remove {uid}"),
                                vec!["share".into(), "remove".into(), uid.clone()],
                            );
                        },
                    );
                }
            }
        }));
        let update_link = button("Create or Update Link", {
            let state = files_state.clone();
            let tx = tx.clone();
            let role = share_role.clone();
            let share_password = share_password.clone();
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
        update_link.add_css_class("suggested-action");
        link_buttons.append(&update_link);
        link_group.add(&link_buttons);
        share_page.add(&link_group);
        let people_group = group("People", "Invite someone by email, or remove their access.");
        let invite_email = entry_row("Invite by Email");
        invite_email.set_input_purpose(gtk4::InputPurpose::Email);
        invite_email.add_suffix(&button("Invite", {
            let state = files_state.clone();
            let tx = tx.clone();
            let role = share_role.clone();
            let invite_email = invite_email.clone();
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
        }));
        people_group.add(&invite_email);
        let revoke_email = entry_row("Remove Access by Email");
        revoke_email.set_input_purpose(gtk4::InputPurpose::Email);
        revoke_email.add_suffix(&button("Remove", {
            let state = files_state.clone();
            let tx = tx.clone();
            let share_window = share_window.clone();
            let revoke_email = revoke_email.clone();
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
                    let tx = tx.clone();
                    confirm_dialog(
                        &share_window,
                        "Remove Access?",
                        &format!("{email} will no longer be able to open this item."),
                        "Remove Access",
                        move || {
                            command(
                                &tx,
                                &format!("Share revoke {uid}"),
                                vec!["share".into(), "revoke".into(), uid.clone(), email.clone()],
                            );
                        },
                    );
                }
            }
        }));
        people_group.add(&revoke_email);
        share_page.add(&people_group);
        let share_view = adw::ToolbarView::new();
        share_view.add_top_bar(&share_header);
        share_view.set_content(Some(&share_page));
        share_window.set_content(Some(&share_view));

        let file_actions = gtk4::gio::SimpleActionGroup::new();
        let open_folder = gtk4::gio::SimpleAction::new("open", None);
        open_folder.connect_activate({
            let state = files_state.clone();
            let tx = tx.clone();
            let status = files_status.clone();
            move |_, _| {
                let current = state.borrow();
                if let Some(url) = current
                    .selected
                    .as_ref()
                    .and_then(|item| item.web_url.clone())
                {
                    drop(current);
                    open_uri(&url);
                } else if let Some(item) = current.selected.as_ref().filter(|item| {
                    !item.degraded && (item.kind == "folder" || item.kind == "album")
                }) {
                    let mut trail = current.trail.clone();
                    trail.push(Folder {
                        uid: item.uid.clone(),
                        name: item.name.clone(),
                    });
                    drop(current);
                    status.set_text("Loading files…");
                    load_files(&tx, &state, trail);
                }
            }
        });
        let rename = gtk4::gio::SimpleAction::new("rename", None);
        rename.connect_activate({
            let state = files_state.clone();
            let tx = tx.clone();
            let window = window.clone();
            move |_, _| {
                if let Some(item) = state
                    .borrow()
                    .selected
                    .clone()
                    .filter(|item| !item.degraded)
                {
                    let tx = tx.clone();
                    name_dialog(&window, "Rename", "Rename", &item.name, move |name| {
                        command(
                            &tx,
                            "Files rename",
                            vec!["browse".into(), "rename".into(), item.uid.clone(), name],
                        );
                    });
                }
            }
        });
        let share = gtk4::gio::SimpleAction::new("share", None);
        share.connect_activate({
            let state = files_state.clone();
            let share_window = share_window.clone();
            let share_title = share_title.clone();
            let share_link = share_link.clone();
            move |_, _| {
                let name = state
                    .borrow()
                    .selected
                    .as_ref()
                    .map(|item| item.name.clone());
                if let Some(name) = name {
                    share_title.set_subtitle(&name);
                    share_link.set_visible(false);
                    check_sharing();
                    share_window.present();
                }
            }
        });
        let web = gtk4::gio::SimpleAction::new("web-link", None);
        web.connect_activate({
            let state = files_state.clone();
            let tx = tx.clone();
            move |_, _| {
                if let Some(item) = state.borrow().selected.as_ref() {
                    command(
                        &tx,
                        &format!("Files url {}", item.uid),
                        vec!["browse".into(), "url".into(), item.uid.clone()],
                    );
                }
            }
        });
        let trash = gtk4::gio::SimpleAction::new("trash", None);
        trash.connect_activate({
            let state = files_state.clone();
            let tx = tx.clone();
            let window = window.clone();
            move |_, _| {
                if let Some(item) = state
                    .borrow()
                    .selected
                    .clone()
                    .filter(|item| !item.degraded)
                {
                    let tx = tx.clone();
                    confirm_dialog(
                        &window,
                        "Move to Trash?",
                        &format!("“{}” will be moved to the trash.", item.name),
                        "Move to Trash",
                        move || {
                            command(
                                &tx,
                                "Files trash",
                                vec!["browse".into(), "trash".into(), item.uid.clone()],
                            );
                        },
                    );
                }
            }
        });
        let keep_offline = gtk4::gio::SimpleAction::new("keep-offline", None);
        let free_space = gtk4::gio::SimpleAction::new("free-space", None);
        for (action, label, subcommand) in [
            (&keep_offline, "Files keep offline", "keep"),
            (&free_space, "Files free up space", "free"),
        ] {
            action.connect_activate({
                let state = files_state.clone();
                let tx = tx.clone();
                let status = files_status.clone();
                move |_, _| {
                    if let Some(item) = state.borrow().selected.as_ref() {
                        status.set_text(if subcommand == "keep" {
                            "Downloading for offline use…"
                        } else {
                            "Freeing up space…"
                        });
                        command(
                            &tx,
                            label,
                            vec!["offline".into(), subcommand.into(), item.uid.clone()],
                        );
                    }
                }
            });
        }
        for action in [
            &open_folder,
            &rename,
            &share,
            &web,
            &trash,
            &keep_offline,
            &free_space,
        ] {
            action.set_enabled(false);
            file_actions.add_action(action);
        }
        files_list.insert_action_group("file", Some(&file_actions));
        let file_menu_model = gtk4::gio::Menu::new();
        let open_section = gtk4::gio::Menu::new();
        open_section.append(Some("Open"), Some("file.open"));
        file_menu_model.append_section(None, &open_section);
        let edit_section = gtk4::gio::Menu::new();
        edit_section.append(Some("Rename…"), Some("file.rename"));
        edit_section.append(Some("Share…"), Some("file.share"));
        edit_section.append(Some("Copy Web Link"), Some("file.web-link"));
        file_menu_model.append_section(None, &edit_section);
        let availability_section = gtk4::gio::Menu::new();
        availability_section.append(
            Some("Always Keep on This Device"),
            Some("file.keep-offline"),
        );
        availability_section.append(Some("Free Up Space"), Some("file.free-space"));
        file_menu_model.append_section(None, &availability_section);
        let trash_section = gtk4::gio::Menu::new();
        trash_section.append(Some("Move to Trash"), Some("file.trash"));
        file_menu_model.append_section(None, &trash_section);
        let file_menu = gtk4::PopoverMenu::from_model(Some(&file_menu_model));
        file_menu.set_has_arrow(false);
        file_menu.set_halign(gtk4::Align::Start);
        file_menu.set_parent(&files_list);
        let right_click = gtk4::GestureClick::new();
        right_click.set_button(3);
        right_click.set_propagation_phase(gtk4::PropagationPhase::Capture);
        right_click.connect_pressed({
            let list = files_list.clone();
            let menu = file_menu.clone();
            move |_, _, x, y| {
                if let Some(row) = list.row_at_y(y as i32) {
                    list.select_row(Some(&row));
                    menu.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(
                        x as i32, y as i32, 1, 1,
                    )));
                    menu.popup();
                } else {
                    menu.popdown();
                }
            }
        });
        files_list.add_controller(right_click);
        let context_key = gtk4::EventControllerKey::new();
        context_key.set_propagation_phase(gtk4::PropagationPhase::Capture);
        context_key.connect_key_pressed({
            let list = files_list.clone();
            let menu = file_menu.clone();
            move |_, key, _, modifiers| {
                if key == gtk4::gdk::Key::Menu
                    || (key == gtk4::gdk::Key::F10
                        && modifiers.contains(gtk4::gdk::ModifierType::SHIFT_MASK))
                {
                    if let Some(row) = list.selected_row() {
                        let bounds = row.allocation();
                        menu.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(
                            bounds.x(),
                            bounds.y(),
                            1,
                            bounds.height(),
                        )));
                        menu.popup();
                    }
                    return gtk4::glib::Propagation::Stop;
                }
                gtk4::glib::Propagation::Proceed
            }
        });
        files_list.add_controller(context_key);
        files_list.connect_row_selected({
            let state = files_state.clone();
            let open_folder = open_folder.clone();
            let rename = rename.clone();
            let share = share.clone();
            let trash = trash.clone();
            let web = web.clone();
            let keep_offline = keep_offline.clone();
            let free_space = free_space.clone();
            move |_, row| {
                let selected =
                    row.and_then(|row| state.borrow().items.get(row.index() as usize).cloned());
                let mutable = selected.as_ref().is_some_and(|item| !item.degraded);
                // Docs and Sheets have no local copy to keep or free.
                let downloadable =
                    mutable && selected.as_ref().is_some_and(|item| item.web_url.is_none());
                keep_offline.set_enabled(
                    downloadable
                        && selected
                            .as_ref()
                            .is_some_and(|item| item.available_offline != Some(true)),
                );
                free_space.set_enabled(
                    downloadable
                        && selected
                            .as_ref()
                            .is_some_and(|item| item.available_offline != Some(false)),
                );
                open_folder.set_enabled(selected.as_ref().is_some_and(|item| {
                    item.web_url.is_some()
                        || (!item.degraded && (item.kind == "folder" || item.kind == "album"))
                }));
                rename.set_enabled(mutable);
                share.set_enabled(selected.is_some());
                trash.set_enabled(mutable);
                web.set_enabled(selected.is_some());
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
                    if let Some(url) = &item.web_url {
                        // Proton Docs and Sheets only open in their web editor.
                        open_uri(url);
                    } else if (item.kind == "folder" || item.kind == "album") && !item.degraded {
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
        pages.add_named(&clamped(&files), Some("files"));

        let photos_state = Rc::new(RefCell::new(PhotosState::default()));
        let photo_thumbnails = Rc::new(RefCell::new(PhotoThumbnails::default()));
        let photos = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        let photos_controls = row();
        photos_controls.set_margin_start(12);
        photos_controls.set_margin_end(12);
        photos_controls.set_margin_top(12);
        let photos_status = caption("Loading photos…");
        let photos_feedback = row();
        photos_feedback.set_margin_start(6);
        let photos_spinner = gtk4::Spinner::new();
        photos_spinner.set_spinning(true);
        photos_feedback.append(&photos_spinner);
        photos_feedback.append(&photos_status);
        photos_status.connect_notify_local(Some("label"), move |label, _| {
            let loading = label.text().starts_with("Loading ")
                || label.text().starts_with("Importing ")
                || label.text().starts_with("Exporting ");
            photos_spinner.set_visible(loading);
            photos_spinner.set_spinning(loading);
        });
        let photos_views = gtk4::Stack::new();
        photos_views.set_vhomogeneous(false);
        photos_views.set_transition_type(gtk4::StackTransitionType::Crossfade);
        let timeline = gtk4::ListBox::new();
        timeline.add_css_class("boxed-list");
        timeline.set_valign(gtk4::Align::Start);
        let albums = gtk4::ListBox::new();
        albums.add_css_class("boxed-list");
        albums.set_valign(gtk4::Align::Start);
        let album_photos = gtk4::ListBox::new();
        album_photos.add_css_class("boxed-list");
        album_photos.set_valign(gtk4::Align::Start);
        photos_views.add_named(&timeline, Some("timeline"));
        photos_views.add_named(&albums, Some("albums"));
        photos_views.add_named(&album_photos, Some("album"));
        let album_items = Rc::new(RefCell::new(Vec::<AlbumItem>::new()));
        let view_switch = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        view_switch.add_css_class("linked");
        let timeline_button = gtk4::ToggleButton::with_label("Timeline");
        let albums_button = gtk4::ToggleButton::with_label("Albums");
        albums_button.set_group(Some(&timeline_button));
        timeline_button.set_active(true);
        view_switch.append(&timeline_button);
        view_switch.append(&albums_button);
        photos_controls.append(&view_switch);
        timeline_button.connect_clicked({
            let views = photos_views.clone();
            let timeline = timeline.clone();
            let state = photos_state.clone();
            let tx = tx.clone();
            let status = photos_status.clone();
            move |_| {
                views.set_visible_child_name("timeline");
                status.set_text("Loading photos…");
                state.borrow_mut().items.clear();
                state.borrow_mut().selected = None;
                clear_list(&timeline);
                load_photos(&tx, &state, None);
            }
        });
        albums_button.connect_clicked({
            let views = photos_views.clone();
            let state = photos_state.clone();
            let tx = tx.clone();
            let status = photos_status.clone();
            let albums = albums.clone();
            let album_items = album_items.clone();
            move |_| {
                views.set_visible_child_name("albums");
                status.set_text("Loading albums…");
                state.borrow_mut().albums_cursor = None;
                album_items.borrow_mut().clear();
                clear_list(&albums);
                load_albums(&tx, &state, None);
            }
        });
        let back_to_albums = gtk4::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("go-previous-symbolic")
                    .label("All Albums")
                    .build(),
            )
            .build();
        back_to_albums.add_css_class("flat");
        back_to_albums.connect_clicked({
            let views = photos_views.clone();
            move |button| {
                views.set_visible_child_name("albums");
                button.set_visible(false);
            }
        });
        back_to_albums.set_visible(false);
        photos_controls.append(&back_to_albums);
        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        photos_controls.append(&spacer);
        photos_controls.append(&icon_button("list-add-symbolic", "New Album", {
            let tx = tx.clone();
            let window = window.clone();
            move || {
                let tx = tx.clone();
                name_dialog(&window, "New Album", "Create", "", move |name| {
                    command(
                        &tx,
                        "Photos create album",
                        vec!["photos".into(), "create-album".into(), name],
                    );
                });
            }
        }));
        let photo_io = gtk4::gio::Menu::new();
        photo_io.append(Some("Import Photo…"), Some("photos.import"));
        photo_io.append(Some("Export All Photos…"), Some("photos.export"));
        let photo_io_actions = gtk4::gio::SimpleActionGroup::new();
        let import = gtk4::gio::SimpleAction::new("import", None);
        import.connect_activate({
            let window = window.clone();
            let tx = tx.clone();
            let status = photos_status.clone();
            move |_, _| {
                let dialog = gtk4::FileChooserNative::builder()
                    .title("Choose a Photo to Import")
                    .action(gtk4::FileChooserAction::Open)
                    .transient_for(&window)
                    .build();
                let tx = tx.clone();
                let status = status.clone();
                dialog.connect_response(move |dialog, response| {
                    if response == gtk4::ResponseType::Accept {
                        if let Some(path) = dialog.file().and_then(|file| file.path()) {
                            status.set_text("Importing photo…");
                            command(
                                &tx,
                                "Photos import",
                                vec![
                                    "photos".into(),
                                    "import".into(),
                                    path.to_string_lossy().into_owned(),
                                ],
                            );
                        } else {
                            status.set_text("Choose a local photo file to import.");
                        }
                    }
                    dialog.destroy();
                });
                dialog.show();
            }
        });
        let export = gtk4::gio::SimpleAction::new("export", None);
        export.connect_activate({
            let window = window.clone();
            let tx = tx.clone();
            let status = photos_status.clone();
            move |_, _| {
                let dialog = gtk4::FileChooserNative::builder()
                    .title("Choose a Folder for Photos Export")
                    .action(gtk4::FileChooserAction::SelectFolder)
                    .transient_for(&window)
                    .build();
                let tx = tx.clone();
                let status = status.clone();
                dialog.connect_response(move |dialog, response| {
                    if response == gtk4::ResponseType::Accept {
                        if let Some(path) = dialog.file().and_then(|file| file.path()) {
                            status.set_text("Exporting Photos…");
                            command(
                                &tx,
                                "Photos export",
                                vec![
                                    "photos".into(),
                                    "export".into(),
                                    path.to_string_lossy().into_owned(),
                                ],
                            );
                        } else {
                            status.set_text("Choose a local folder for Photos export.");
                        }
                    }
                    dialog.destroy();
                });
                dialog.show();
            }
        });
        photo_io_actions.add_action(&import);
        photo_io_actions.add_action(&export);
        photos.insert_action_group("photos", Some(&photo_io_actions));
        photos_controls.append(
            &gtk4::MenuButton::builder()
                .icon_name("view-more-symbolic")
                .tooltip_text("Import and Export")
                .menu_model(&photo_io)
                .css_classes(["flat"])
                .build(),
        );
        photos.append(&photos_controls);
        let photos_body = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        photos_body.append(&photos_feedback);
        photos_body.append(&photos_views);
        let load_more = gtk4::Button::with_label("Load More");
        load_more.add_css_class("pill");
        load_more.set_halign(gtk4::Align::Center);
        load_more.set_margin_top(6);
        load_more.connect_clicked({
            let state = photos_state.clone();
            let tx = tx.clone();
            let status = photos_status.clone();
            let views = photos_views.clone();
            move |_| match views.visible_child_name().as_deref() {
                Some("timeline") => {
                    let cursor = state.borrow().next_cursor.clone();
                    if let Some(cursor) = cursor {
                        status.set_text("Loading more photos…");
                        load_photos(&tx, &state, Some(cursor));
                    }
                }
                Some("albums") => {
                    let cursor = state.borrow().albums_cursor.clone();
                    if let Some(cursor) = cursor {
                        status.set_text("Loading more albums…");
                        load_albums(&tx, &state, Some(cursor));
                    }
                }
                Some("album") => {
                    let (uid, cursor) = {
                        let state = state.borrow();
                        (state.album_uid.clone(), state.album_cursor.clone())
                    };
                    if let (Some(uid), Some(cursor)) = (uid, cursor) {
                        status.set_text("Loading more album photos…");
                        load_album(&tx, &state, uid, Some(cursor));
                    }
                }
                _ => {}
            }
        });
        photos_body.append(&load_more);
        photos.append(&clamped(&photos_body));
        load_more.set_visible(false);
        load_more.connect_clicked(|button| button.set_sensitive(false));
        for button in [&timeline_button, &albums_button] {
            let more = load_more.clone();
            button.connect_clicked(move |_| {
                more.set_visible(false);
                more.set_sensitive(true);
            });
        }
        back_to_albums.connect_clicked({
            let state = photos_state.clone();
            let more = load_more.clone();
            move |_| {
                state.borrow_mut().request += 1;
                more.set_visible(state.borrow().albums_cursor.is_some());
                more.set_sensitive(true);
            }
        });
        let photo_bar = gtk4::ActionBar::new();
        let favorite = gtk4::Button::with_label("Add to Favorites");
        favorite.connect_clicked({
            let state = photos_state.clone();
            let tx = tx.clone();
            move |_| {
                let state = state.borrow();
                if let Some(item) = state.items.iter().find(|item| {
                    state.selected.as_deref() == Some(item.uid.as_str()) && item.tags.is_some()
                }) {
                    let mut args = vec!["photos".into(), "favorite".into(), item.uid.clone()];
                    let favorite = !item.tags.as_ref().is_some_and(|tags| tags.contains(&0));
                    if !favorite {
                        args.push("--off".into());
                    }
                    command(
                        &tx,
                        &format!(
                            "Photo favorite {} {}",
                            if favorite { "on" } else { "off" },
                            item.uid
                        ),
                        args,
                    );
                }
            }
        });
        favorite.set_sensitive(false);
        favorite.set_tooltip_text(Some("Favorites can be changed from the timeline."));
        photo_bar.pack_start(&favorite);
        let photo_web =
            gtk4::LinkButton::with_label("https://drive.proton.me", "Open in Proton Drive");
        photo_web.set_visible(false);
        photo_bar.pack_end(&photo_web);
        photo_bar.set_revealed(false);
        photos.append(&photo_bar);
        for button in [
            timeline_button.upcast_ref::<gtk4::Button>(),
            albums_button.upcast_ref(),
            &back_to_albums,
        ] {
            let favorite = favorite.clone();
            let photo_web = photo_web.clone();
            let photo_bar = photo_bar.clone();
            let state = photos_state.clone();
            button.connect_clicked(move |_| {
                state.borrow_mut().selected = None;
                favorite.set_sensitive(false);
                photo_web.set_visible(false);
                photo_bar.set_revealed(false);
            });
        }
        for list in [&timeline, &album_photos] {
            // A single click selects for the action bar; opening downloads the original.
            list.set_activate_on_single_click(false);
            list.connect_row_activated({
                let state = photos_state.clone();
                let tx = tx.clone();
                let status = photos_status.clone();
                move |_, row| {
                    let item = state.borrow().items.get(row.index() as usize).cloned();
                    if let Some(item) = item {
                        status.set_text("Loading original photo…");
                        open_photo(&tx, &item);
                    }
                }
            });
            list.connect_row_selected({
                let state = photos_state.clone();
                let tx = tx.clone();
                let favorite = favorite.clone();
                let photo_web = photo_web.clone();
                let photo_bar = photo_bar.clone();
                move |_, row| {
                    let selected =
                        row.and_then(|row| state.borrow().items.get(row.index() as usize).cloned());
                    photo_web.set_visible(false);
                    photo_bar.set_revealed(selected.is_some());
                    state.borrow_mut().selected = selected.as_ref().map(|item| item.uid.clone());
                    favorite
                        .set_sensitive(selected.as_ref().is_some_and(|item| item.tags.is_some()));
                    favorite.set_label(
                        if selected.as_ref().is_some_and(|item| {
                            item.tags.as_ref().is_some_and(|tags| tags.contains(&0))
                        }) {
                            "Remove from Favorites"
                        } else {
                            "Add to Favorites"
                        },
                    );
                    if let Some(item) = selected {
                        command(
                            &tx,
                            &format!("Photo web {}", item.uid),
                            vec!["browse".into(), "url".into(), item.uid.clone()],
                        );
                    }
                }
            });
        }
        albums.connect_row_activated({
            let items = album_items.clone();
            let state = photos_state.clone();
            let tx = tx.clone();
            let status = photos_status.clone();
            let more = load_more.clone();
            let favorite = favorite.clone();
            let photo_web = photo_web.clone();
            let photo_bar = photo_bar.clone();
            move |_, row| {
                if let Some(album) = items.borrow().get(row.index() as usize) {
                    status.set_text("Loading album…");
                    more.set_visible(false);
                    more.set_sensitive(true);
                    favorite.set_sensitive(false);
                    photo_web.set_visible(false);
                    photo_bar.set_revealed(false);
                    let uid = album.uid.clone();
                    state.borrow_mut().album_uid = Some(uid.clone());
                    state.borrow_mut().album_cursor = None;
                    state.borrow_mut().selected = None;
                    load_album(&tx, &state, uid, None);
                }
            }
        });
        pages.add_named(&photos, Some("photos"));

        let computers = adw::PreferencesPage::new();
        let computers_list = group(
            "Your Backups",
            "Each computer and the folders it keeps in Drive.",
        );
        computers_list.set_header_suffix(Some(&icon_button("view-refresh-symbolic", "Refresh", {
            let tx = tx.clone();
            move || command(&tx, "Computers", vec!["computers".into(), "--json".into()])
        })));
        let computer_rows = boxed_list();
        computer_rows.set_visible(false);
        computers_list.add(&computer_rows);
        let computers_status = caption("Loading computers…");
        computers_list.add(&computers_status);
        computers.add(&computers_list);
        let backup = group(
            "Back Up This Computer",
            "Choose a folder to keep a copy in Drive. Deleting local files does not delete the cloud copy.",
        );
        let backup_path = entry_row("Local Folder");
        backup_path.add_suffix(&icon_button("folder-open-symbolic", "Choose Folder", {
            let window = window.clone();
            let backup_path = backup_path.clone();
            move || {
                let dialog = gtk4::FileChooserNative::builder()
                    .title("Choose a Folder to Back Up")
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
        }));
        let add_backup = button("Add Backup", {
            let tx = tx.clone();
            let backup_path = backup_path.clone();
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
        add_backup.add_css_class("suggested-action");
        backup_path.add_suffix(&add_backup);
        backup.add(&backup_path);
        computers.add(&backup);
        let restore = group(
            "Restore a Backup",
            "Download a computer folder to a local path and continue syncing.",
        );
        let computer = entry_row("Computer Name or ID");
        let folder = entry_row("Folder Name");
        let path = entry_row("Local Destination");
        for entry in [&computer, &folder, &path] {
            restore.add(entry);
        }
        let restore_button = button("Restore", {
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
        restore_button.set_halign(gtk4::Align::End);
        restore_button.set_margin_top(12);
        restore.add(&restore_button);
        computers.add(&restore);
        let remove = group("Stop Syncing a Folder", "Local and cloud files are kept.");
        let job = entry_row("Sync Job Name or ID");
        job.add_suffix(&button("Unsync", {
            let tx = tx.clone();
            let job = job.clone();
            move || {
                if !job.text().is_empty() {
                    command(
                        &tx,
                        "Unsync",
                        vec!["computers".into(), "unsync".into(), job.text().to_string()],
                    );
                }
            }
        }));
        remove.add(&job);
        computers.add(&remove);
        pages.add_named(&computers, Some("computers"));

        let avatar = adw::Avatar::new(96, None, true);
        let username = gtk4::Label::new(None);
        username.add_css_class("title-1");
        username.set_selectable(true);
        username.set_wrap(true);
        username.set_justify(gtk4::Justification::Center);
        let account_note =
            caption("Signed in to Proton Drive. Your credentials are stored by pdcli.");
        account_note.set_halign(gtk4::Align::Center);
        account_note.set_justify(gtk4::Justification::Center);
        let sign_out = button("Sign Out", {
            let tx = tx.clone();
            move || command(&tx, "Sign out", vec!["logout".into()])
        });
        sign_out.add_css_class("pill");
        sign_out.add_css_class("destructive-action");
        sign_out.set_halign(gtk4::Align::Center);
        sign_out.set_margin_top(12);
        let account_box = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
        account_box.set_valign(gtk4::Align::Center);
        account_box.set_vexpand(true);
        account_box.append(&avatar);
        account_box.append(&username);
        account_box.append(&account_note);
        account_box.append(&sign_out);
        pages.add_named(&clamped(&account_box), Some("account"));

        let settings = adw::PreferencesPage::new();
        let preferences = load_preferences();
        let startup = group("Startup", "");
        let auto_mount = adw::SwitchRow::new();
        auto_mount.set_title("Mount Drive After Sign-In");
        auto_mount.set_subtitle("Opening the window also starts the mount and tray");
        auto_mount.set_active(preferences.as_ref().map_or(true, |value| value.auto_mount));
        startup.add(&auto_mount);
        let default_page = adw::ComboRow::new();
        default_page.set_title("Default Page");
        default_page.set_model(Some(&gtk4::StringList::new(&[
            "Files",
            "Photos",
            "Computers",
            "Status",
        ])));
        let selected = ["files", "photos", "computers", "status"]
            .iter()
            .position(|page| {
                preferences
                    .as_ref()
                    .is_ok_and(|value| value.start_page == *page)
            })
            .unwrap_or(0);
        default_page.set_selected(selected as u32);
        startup.add(&default_page);
        let save_startup = Rc::new({
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
        auto_mount.connect_active_notify({
            let save = save_startup.clone();
            move |_| save()
        });
        default_page.connect_selected_notify(move |_| save_startup());
        settings.add(&startup);
        settings.add(&group(
            "Offline Storage",
            "Downloaded files are not yet encrypted at rest. Keep the configuration directory on an encrypted filesystem. Safe cache removal is not available in this version.",
        ));
        let ignore = group(
            "Ignored Files",
            "Files matching these patterns are not uploaded from the mount.",
        );
        let editor = gtk4::TextView::new();
        editor.set_monospace(true);
        editor.set_top_margin(12);
        editor.set_bottom_margin(12);
        editor.set_left_margin(12);
        editor.set_right_margin(12);
        let ignore_tools = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        ignore_tools.append(&icon_button("view-refresh-symbolic", "Reload From Disk", {
            let tx = tx.clone();
            move || {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(Event::IgnoreLoaded(pdignore::load_global_text()));
                });
            }
        }));
        ignore_tools.append(&icon_button("edit-undo-symbolic", "Restore Defaults", {
            let buffer = editor.buffer();
            move || buffer.set_text(pdignore::DEFAULT_GLOBAL_PDIGNORE)
        }));
        ignore.set_header_suffix(Some(&ignore_tools));
        let editor_scroll = scroll(&editor);
        editor_scroll.set_min_content_height(260);
        let editor_frame = gtk4::Frame::new(None);
        editor_frame.set_child(Some(&editor_scroll));
        ignore.add(&editor_frame);
        let ignore_footer = row();
        ignore_footer.set_margin_top(12);
        let path_label = caption(&pdignore::global_path().display().to_string());
        path_label.set_selectable(true);
        path_label.set_hexpand(true);
        path_label.set_ellipsize(gtk4::pango::EllipsizeMode::Middle);
        ignore_footer.append(&path_label);
        let save_ignore = button("Save", {
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
        save_ignore.add_css_class("suggested-action");
        ignore_footer.append(&save_ignore);
        ignore.add(&ignore_footer);
        settings.add(&ignore);
        pages.add_named(&settings, Some("settings"));

        let selected_page = match initial_page {
            Some("mount") => "status",
            Some(page) if page != "about" => page,
            _ => preferences
                .as_ref()
                .map_or("files", |value| value.start_page.as_str()),
        };
        show_page(selected_page);

        notifications.set_child(Some(&root));
        window.set_content(Some(&notifications));
        let compact = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 600sp").expect("valid breakpoint"),
        );
        compact.add_setter(&split_view, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(compact);
        window.present();
        if initial_page == Some("about") {
            present_about(&window);
        }
        if let Err(error) = preferences {
            notifications.add_toast(toast(&format!("Could not load desktop settings: {error}")));
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
                    Event::Thumbnail(uid, bytes) => {
                        photo_thumbnails.borrow_mut().received(uid, bytes);
                    }
                    Event::PhotoDownloaded(result) => match result {
                        Ok(path) => {
                            photos_status.set_text(&plural(
                                photos_state.borrow().items.len(),
                                "photo",
                                "photos",
                            ));
                            open_uri(&gtk4::gio::File::for_path(&path).uri());
                        }
                        Err(error) => {
                            photos_status.set_text(&format!("Could not open photo: {error}"))
                        }
                    },
                    Event::Account(Some(name)) => {
                        authenticated.set(true);
                        username.set_text(&name);
                        avatar.set_text(Some(&name));
                        sidebar_title.set_subtitle(&name);
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
                        sidebar_title.set_subtitle("");
                        root.set_visible_child_name("login");
                        files_state.borrow_mut().request += 1;
                        files_state.borrow_mut().selected = None;
                        file_menu.popdown();
                        share_window.close();
                        photos_state.borrow_mut().request += 1;
                        photos_state.borrow_mut().selected = None;
                        favorite.set_sensitive(false);
                        photo_bar.set_revealed(false);
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
                        file_menu.popdown();
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
                                    if index > 0 {
                                        let separator = gtk4::Label::new(Some("/"));
                                        separator.add_css_class("dim-label");
                                        breadcrumb.append(&separator);
                                    }
                                    let tx = tx.clone();
                                    let state = files_state.clone();
                                    let destination = trail[..=index].to_vec();
                                    let status = files_status.clone();
                                    let crumb = button(&folder.name, move || {
                                        status.set_text("Loading files…");
                                        load_files(&tx, &state, destination.clone());
                                    });
                                    crumb.add_css_class("flat");
                                    if index + 1 == trail.len() {
                                        crumb.add_css_class("heading");
                                    }
                                    breadcrumb.append(&crumb);
                                }
                                clear_list(&files_list);
                                let count = listing.items.len();
                                files_state.borrow_mut().trail = trail;
                                files_state.borrow_mut().items = listing.items.clone();
                                files_state.borrow_mut().selected = None;
                                for action in [
                                    &open_folder,
                                    &rename,
                                    &share,
                                    &web,
                                    &trash,
                                    &keep_offline,
                                    &free_space,
                                ] {
                                    action.set_enabled(false);
                                }
                                for item in listing.items {
                                    let folder = item.kind == "folder" || item.kind == "album";
                                    let subtitle = if item.degraded {
                                        item.error.as_ref().map_or_else(
                                            || "Unavailable".to_string(),
                                            |error| format!("Unavailable: {error}"),
                                        )
                                    } else if let Some(size) = item.size.filter(|size| *size >= 0) {
                                        gtk4::glib::format_size(size as u64).to_string()
                                    } else {
                                        capitalized(&item.kind)
                                    };
                                    let list_row = info_row(&item.name, &subtitle);
                                    list_row.add_prefix(&gtk4::Image::from_icon_name(file_icon(
                                        &item.kind,
                                    )));
                                    if item.degraded {
                                        list_row.add_css_class("dim-label");
                                    }
                                    if let Some(offline) = item.available_offline {
                                        let (icon, tooltip) = if offline {
                                            ("emblem-ok-symbolic", "Available offline")
                                        } else {
                                            (
                                                "weather-overcast-symbolic",
                                                "Online only; downloads when opened",
                                            )
                                        };
                                        let availability = gtk4::Image::from_icon_name(icon);
                                        availability.set_tooltip_text(Some(tooltip));
                                        availability.add_css_class("dim-label");
                                        list_row.add_suffix(&availability);
                                    }
                                    let more = icon_button("view-more-symbolic", "More Actions", {
                                        let list = files_list.clone();
                                        let menu = file_menu.clone();
                                        let row = list_row.downgrade();
                                        move || {
                                            let Some(row) = row.upgrade() else { return };
                                            list.select_row(Some(&row));
                                            if let Some(bounds) = row.compute_bounds(&list) {
                                                menu.set_pointing_to(Some(
                                                    &gtk4::gdk::Rectangle::new(
                                                        bounds.x() as i32 + bounds.width() as i32
                                                            - 24,
                                                        bounds.y() as i32,
                                                        1,
                                                        bounds.height() as i32,
                                                    ),
                                                ));
                                            }
                                            menu.popup();
                                        }
                                    });
                                    list_row.add_suffix(&more);
                                    if folder && !item.degraded {
                                        list_row.set_activatable(true);
                                        list_row.add_suffix(&gtk4::Image::from_icon_name(
                                            "go-next-symbolic",
                                        ));
                                    }
                                    files_list.append(&list_row);
                                }
                                files_list.set_visible(count > 0);
                                files_status.set_text(&if count == 0 {
                                    "This folder is empty.".to_string()
                                } else {
                                    plural(count, "item", "items")
                                });
                                let offline_only = files_state
                                    .borrow()
                                    .trail
                                    .last()
                                    .is_some_and(|folder| folder.uid == OFFLINE_VIEW);
                                if offline_only && count == 0 {
                                    files_status.set_text("No files are kept on this device.");
                                }
                                offline_filter.set_active(offline_only);
                                back.set_sensitive(files_state.borrow().trail.len() > 1);
                                create.set_sensitive(!offline_only);
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
                        load_more.set_sensitive(true);
                        match result {
                            Ok(page) => {
                                back_to_albums.set_visible(false);
                                let append = cursor.is_some();
                                show_photos(&timeline, &page.items, append, &photo_thumbnails, &tx);
                                let mut state = photos_state.borrow_mut();
                                if !append {
                                    state.items.clear();
                                    state.selected = None;
                                }
                                state.items.extend(page.items);
                                state.next_cursor = page.next_cursor;
                                load_more.set_visible(state.next_cursor.is_some());
                                photos_status.set_text(&if state.items.is_empty() {
                                    "No photos in the timeline.".to_string()
                                } else {
                                    plural(state.items.len(), "photo", "photos")
                                });
                                photos_views.set_visible_child_name("timeline");
                            }
                            Err(error) => {
                                photos_status.set_text(&format!("Could not load photos: {error}"))
                            }
                        }
                    }
                    Event::Albums(request, cursor, result) => {
                        if request != photos_state.borrow().request {
                            continue;
                        }
                        load_more.set_sensitive(true);
                        match result {
                            Ok(result) => {
                                back_to_albums.set_visible(false);
                                if cursor.is_none() {
                                    album_items.borrow_mut().clear();
                                    clear_list(&albums);
                                }
                                for album in &result.albums {
                                    let row = info_row(
                                        album.name.as_deref().unwrap_or("Untitled album"),
                                        &plural(album.photo_count as usize, "photo", "photos"),
                                    );
                                    row.add_prefix(&gtk4::Image::from_icon_name(
                                        "folder-pictures-symbolic",
                                    ));
                                    row.add_suffix(&gtk4::Image::from_icon_name(
                                        "go-next-symbolic",
                                    ));
                                    row.set_activatable(true);
                                    albums.append(&row);
                                }
                                album_items.borrow_mut().extend(result.albums);
                                photos_state.borrow_mut().albums_cursor = result.next_cursor;
                                load_more
                                    .set_visible(photos_state.borrow().albums_cursor.is_some());
                                photos_status.set_text(&if album_items.borrow().is_empty() {
                                    "No albums yet.".to_string()
                                } else {
                                    plural(album_items.borrow().len(), "album", "albums")
                                });
                                photos_views.set_visible_child_name("albums");
                            }
                            Err(error) => {
                                photos_status.set_text(&format!("Could not load albums: {error}"))
                            }
                        }
                    }
                    Event::Album(request, uid, cursor, result) => {
                        if request != photos_state.borrow().request
                            || photos_state.borrow().album_uid.as_deref() != Some(&uid)
                        {
                            continue;
                        }
                        load_more.set_sensitive(true);
                        match result {
                            Ok(result) => {
                                if result.album.uid != uid {
                                    photos_status.set_text(
                                        "Album response did not match the selected album.",
                                    );
                                    continue;
                                }
                                back_to_albums.set_visible(true);
                                let append = cursor.is_some();
                                show_photos(
                                    &album_photos,
                                    &result.items,
                                    append,
                                    &photo_thumbnails,
                                    &tx,
                                );
                                let mut state = photos_state.borrow_mut();
                                if !append {
                                    state.items.clear();
                                    state.selected = None;
                                }
                                state.items.extend(result.items);
                                state.album_cursor = result.next_cursor;
                                load_more.set_visible(state.album_cursor.is_some());
                                photos_status.set_text(&format!(
                                    "{}, {}",
                                    result.album.name.as_deref().unwrap_or("Album"),
                                    plural(state.items.len(), "photo", "photos")
                                ));
                                photos_views.set_visible_child_name("album");
                            }
                            Err(error) => {
                                photos_status.set_text(&format!("Could not open album: {error}"))
                            }
                        }
                    }
                    Event::Done(action, result) => {
                        let routine = result.is_ok()
                            && (action == "Status"
                                || action == "Computers"
                                || action == "Preferences"
                                || action.starts_with("Share status ")
                                || action.starts_with("Share link ")
                                || action.starts_with("Photo web ")
                                || action.starts_with("Files url "));
                        let message = match result {
                            Ok(text) => {
                                if action == "Status" {
                                    match serde_json::from_str::<Status>(&text) {
                                        Ok(status) if !status.signed_in => {
                                            // The CLI removes credentials whose refresh token
                                            // Proton rejected; finish signing out here.
                                            if authenticated.replace(false) {
                                                notifications.add_toast(toast(
                                                    "Session expired. Sign in again.",
                                                ));
                                                command(&tx, "Sign out", vec!["logout".into()]);
                                            }
                                        }
                                        Ok(status) => {
                                            clear_list(&transfer_list);
                                            for transfer in status.transfers {
                                                let row = info_row(
                                                    &transfer.filename,
                                                    &transfer_label(&transfer),
                                                );
                                                let progress = gtk4::ProgressBar::new();
                                                progress.set_valign(gtk4::Align::Center);
                                                progress.set_size_request(120, -1);
                                                if transfer.total_bytes > 0 {
                                                    progress.set_fraction(
                                                        (transfer.bytes_transferred as f64
                                                            / transfer.total_bytes as f64)
                                                            .clamp(0.0, 1.0),
                                                    );
                                                }
                                                row.add_suffix(&progress);
                                                if transfer.cancellable {
                                                    let tx = tx.clone();
                                                    let id = transfer.id;
                                                    row.add_suffix(&icon_button(
                                                        "window-close-symbolic",
                                                        "Cancel Download",
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
                                                    ));
                                                }
                                                transfer_list.append(&row);
                                            }
                                            let paused = status.daemon == "paused";
                                            daemon_row.set_subtitle(&capitalized(&status.daemon));
                                            pause.set_visible(!paused);
                                            resume.set_visible(paused);
                                            match status.journal {
                                                Some(journal) => {
                                                    queue_row.set_subtitle(&format!(
                                                        "{} pending, {} failed",
                                                        journal.pending, journal.failed
                                                    ));
                                                    retry_failed.set_sensitive(journal.failed > 0);
                                                }
                                                None => {
                                                    queue_row.set_subtitle("Queue unavailable");
                                                    retry_failed.set_sensitive(false);
                                                }
                                            }
                                            mount_row.set_title(if status.mounted {
                                                "Mounted"
                                            } else {
                                                "Not Mounted"
                                            });
                                            mount_row.set_subtitle(&status.mountpoint);
                                            open_mount.set_sensitive(status.mounted);
                                            mount_button.set_visible(!status.mounted);
                                            stop_mount.set_visible(status.mounted);
                                        }
                                        Err(error) => {
                                            let message =
                                                format!("Invalid status response: {error}");
                                            daemon_row.set_subtitle(&message);
                                            mount_row.set_subtitle(&message);
                                            notifications.add_toast(toast(&message));
                                        }
                                    }
                                } else if action == "Computers" {
                                    match serde_json::from_str::<ComputerList>(&text) {
                                        Ok(snapshot) => {
                                            clear_list(&computer_rows);
                                            computer_rows
                                                .set_visible(!snapshot.computers.is_empty());
                                            computers_status
                                                .set_visible(snapshot.computers.is_empty());
                                            computers_status.set_text(
                                                "No computers registered. Add a backup to register this device.",
                                            );
                                            for computer in snapshot.computers {
                                                let this_device =
                                                    snapshot.this_device_id.as_deref()
                                                        == Some(&computer.id);
                                                let expander = adw::ExpanderRow::new();
                                                expander.set_use_markup(false);
                                                expander.set_title(&computer.name);
                                                expander.set_subtitle(&match (
                                                    this_device,
                                                    computer.last_sync_time,
                                                ) {
                                                    (true, Some(time)) => {
                                                        format!("This computer, last synced {time}")
                                                    }
                                                    (true, None) => {
                                                        "This computer, not synced yet".into()
                                                    }
                                                    (false, Some(time)) => {
                                                        format!("Last synced {time}")
                                                    }
                                                    (false, None) => "Not synced yet".into(),
                                                });
                                                expander.add_prefix(&gtk4::Image::from_icon_name(
                                                    "computer-symbolic",
                                                ));
                                                expander.set_expanded(this_device);
                                                let jobs = snapshot
                                                    .jobs
                                                    .iter()
                                                    .filter(|job| job.device_id == computer.id)
                                                    .collect::<Vec<_>>();
                                                expander.set_enable_expansion(!jobs.is_empty());
                                                for job in jobs {
                                                    let job_row =
                                                        info_row(&job.name, &job.local_path);
                                                    job_row.set_subtitle_selectable(true);
                                                    job_row.add_prefix(
                                                        &gtk4::Image::from_icon_name(
                                                            "folder-symbolic",
                                                        ),
                                                    );
                                                    expander.add_row(&job_row);
                                                }
                                                computer_rows.append(&expander);
                                            }
                                        }
                                        Err(error) => {
                                            computers_status.set_visible(true);
                                            computers_status.set_text(&format!(
                                                "Invalid computer list: {error}"
                                            ))
                                        }
                                    }
                                } else if action == "Mount" || action == "Stop" {
                                    refresh_status(&tx);
                                } else if action == "Cancel download" {
                                    refresh_status(&tx);
                                } else if action == "Photos create album" {
                                    albums_button.emit_clicked();
                                } else if action == "Photos import" {
                                    timeline_button.emit_clicked();
                                } else if action == "Photos export" {
                                    photos_status.set_text("Photos export complete.");
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
                                    avatar.set_text(None);
                                    sidebar_title.set_subtitle("");
                                    root.set_visible_child_name("login");
                                    share_window.close();
                                    files_state.borrow_mut().request += 1;
                                    files_state.borrow_mut().trail.clear();
                                    files_state.borrow_mut().items.clear();
                                    files_state.borrow_mut().selected = None;
                                    clear_list(&files_list);
                                    while let Some(child) = breadcrumb.first_child() {
                                        breadcrumb.remove(&child);
                                    }
                                    back.set_sensitive(false);
                                    create.set_sensitive(false);
                                    for action in [&open_folder, &rename, &share, &web, &trash] {
                                        action.set_enabled(false);
                                    }
                                    files_status.set_text("Sign in to browse files.");
                                    login_link.set_visible(false);
                                    login_details.set_text("Signed out.");
                                    login_button.set_sensitive(true);
                                } else if let Some(uid) = action.strip_prefix("Files url ") {
                                    if !text.starts_with("https://") {
                                        files_status.set_text("Invalid web URL returned by pdcli.");
                                    } else if files_state
                                        .borrow()
                                        .selected
                                        .as_ref()
                                        .is_some_and(|item| item.uid == uid)
                                    {
                                        window.clipboard().set_text(&text);
                                        let copied = toast("Web link copied");
                                        copied.set_button_label(Some("Open"));
                                        copied.connect_button_clicked(move |_| open_uri(&text));
                                        notifications.add_toast(copied);
                                    }
                                    continue;
                                } else if let Some(uid) = action.strip_prefix("Photo web ") {
                                    if text.starts_with("https://")
                                        && photos_state.borrow().selected.as_deref() == Some(uid)
                                    {
                                        photo_web.set_uri(&text);
                                        photo_web.set_visible(true);
                                    }
                                } else if let Some(change) = action.strip_prefix("Photo favorite ")
                                {
                                    if let Some((mode, uid)) = change.split_once(' ') {
                                        let mut state = photos_state.borrow_mut();
                                        if state.selected.as_deref() == Some(uid) {
                                            if let Some(item) =
                                                state.items.iter_mut().find(|item| item.uid == uid)
                                            {
                                                if let Some(tags) = &mut item.tags {
                                                    tags.retain(|tag| *tag != 0);
                                                    if mode == "on" {
                                                        tags.push(0);
                                                    }
                                                }
                                                favorite.set_label(if mode == "on" {
                                                    "Remove from Favorites"
                                                } else {
                                                    "Add to Favorites"
                                                });
                                            }
                                        }
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
                                                sharing_status
                                                    .set_subtitle(&sharing_summary(&info));
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
                                                sharing_status.set_subtitle("Not shared");
                                                share_link.set_visible(false);
                                            }
                                            Err(error) => sharing_status.set_subtitle(&format!(
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
                                            sharing_status.set_subtitle("Public link ready");
                                        } else {
                                            sharing_status.set_subtitle(
                                                "Invalid public link returned by pdcli.",
                                            );
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
                                        "Files mkdir"
                                            | "Files rename"
                                            | "Files trash"
                                            | "Files keep offline"
                                            | "Files free up space"
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
                                    computers_status.set_visible(true);
                                    computers_status
                                        .set_text(&format!("Could not load computers: {error}"));
                                }
                                if action == "Status" {
                                    daemon_row
                                        .set_subtitle(&format!("Could not check status: {error}"));
                                    mount_row
                                        .set_subtitle(&format!("Could not check mount: {error}"));
                                }
                                if action.starts_with("Files ") {
                                    files_status.set_text(&format!("{action} failed: {error}"));
                                }
                                if action.starts_with("Share ") {
                                    sharing_status
                                        .set_subtitle(&format!("{action} failed: {error}"));
                                }
                                if action.starts_with("Photo ") || action.starts_with("Photos ") {
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
                            notifications.add_toast(toast(&message));
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
        fn photos_pages_keep_cursors_and_only_timeline_has_favorite_state() {
            let albums: AlbumList = serde_json::from_str(
                r#"{"albums":[{"uid":"v~a","name":"Trips","photo_count":2}],"next_cursor":"more"}"#,
            )
            .unwrap();
            assert_eq!(albums.next_cursor.as_deref(), Some("more"));

            let album: AlbumDetails = serde_json::from_str(
                r#"{"album":{"uid":"v~a","name":"Trips"},"items":[{"uid":"v~p","capture_time":"2026-01-01T00:00:00Z"}],"next_cursor":"next"}"#,
            )
            .unwrap();
            assert_eq!(album.next_cursor.as_deref(), Some("next"));
            assert!(album.items[0].tags.is_none());

            let timeline: PhotoPage = serde_json::from_str(
                r#"{"items":[{"uid":"v~p","capture_time":"2026-01-01T00:00:00Z","tags":[0]}],"next_cursor":null}"#,
            )
            .unwrap();
            assert_eq!(timeline.items[0].tags.as_deref(), Some([0].as_slice()));
        }

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
        fn sharing_summary_lists_members_and_link() {
            let info: SharingInfo = serde_json::from_str(
                r#"{"members":[{"email":"a@example.com","role":"viewer"}],"pending_invitations":2,"public_link":{"url":"https://example.com/link","role":"editor","expires":"2026-12-01"}}"#,
            )
            .unwrap();
            assert_eq!(
                sharing_summary(&info),
                "Shared with 1 person, 2 pending invitations\na@example.com (viewer)\nPublic link for editors, expires 2026-12-01"
            );
            assert_eq!(capitalized("not running"), "Not running");
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
    eprintln!("pdcli Desktop (pdcli-gui) is available on Linux only.");
}
