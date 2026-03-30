use futures::StreamExt;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::device_ops::Device;
use proton_drive_sdk::node::NodeUid;
use proton_drive_sdk::photo::ProtonPhotosClient;
use proton_drive_sdk::volume::VolumeId;
use proton_sdk_rs2::session::ProtonAPISession;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use crate::index::{IndexEntry, NodeIndex};
use crate::settings::Settings;
use crate::vfs::{VfsSection, VirtualPath};

/// A lightweight record of a trashed item, used for tab-completion in /Trash.
#[derive(Debug, Clone)]
pub struct TrashRecord {
    pub uid: NodeUid,
    pub name: String,
    pub is_folder: bool,
}

pub struct AppState {
    pub drive: Arc<ProtonDriveClient>,
    pub photos: Arc<ProtonPhotosClient>,
    pub session: ProtonAPISession,
    pub cwd: VirtualPath,
    pub index: Arc<NodeIndex>,
    pub settings: Settings,
    pub root_uid: NodeUid,
    pub photos_root_uid: NodeUid,
    pub volume_id: VolumeId,
    pub should_quit: bool,
    /// Cancellation token for the currently-running command.
    /// A fresh token is installed before each dispatch; commands can use it
    /// for cooperative cancellation. The REPL loop cancels it on Ctrl+C.
    pub cancel: CancellationToken,
    /// Cached device list — populated on first `ls /Computers` or `cd <device>`.
    pub devices: Arc<parking_lot::RwLock<Vec<Device>>>,
    /// Cached trash items — populated whenever `ls /Trash` runs.
    pub trash_items: Arc<parking_lot::RwLock<Vec<TrashRecord>>>,
    /// UIDs of known album nodes — so we use the photos API for their children.
    pub album_uids: Arc<dashmap::DashSet<NodeUid>>,
}

impl AppState {
    pub async fn new(
        mut session: ProtonAPISession,
    ) -> anyhow::Result<Self> {
        let pb = crate::ui::spinner("Connecting to Proton Drive…");
        let drive = Arc::new(
            ProtonDriveClient::new_with_preflight_auth(&mut session, None).await?,
        );
        pb.set_message("Connecting to Proton Photos…");
        let photos = Arc::new(ProtonPhotosClient::new(&session, None)?);

        pb.set_message("Fetching root folders…");
        let (my_files, photos_root) = tokio::try_join!(
            drive.get_my_files_folder(),
            photos.get_photos_root_folder(),
        )?;
        pb.set_message("Loading cached index…");
        let index = Arc::new(
            match crate::db::open_and_init(&crate::app_paths::db_path()) {
                Ok(conn) => NodeIndex::with_db(conn).unwrap_or_else(|e| {
                    tracing::warn!("Could not load index from DB: {e}");
                    NodeIndex::new()
                }),
                Err(e) => {
                    tracing::warn!("Could not open index DB: {e}");
                    NodeIndex::new()
                }
            },
        );
        pb.finish_and_clear();

        let root_uid = my_files.base.uid.clone();
        let photos_root_uid = photos_root.base.uid.clone();
        let volume_id = root_uid.volume_id.clone();

        index.insert(IndexEntry {
            uid: root_uid.clone(),
            parent_uid: None,
            name: "MyFiles".to_string(),
            is_folder: true,
            size: None,
            modification_time: None,
            media_type: None,
        });
        index.insert(IndexEntry {
            uid: photos_root_uid.clone(),
            parent_uid: None,
            name: "Photos".to_string(),
            is_folder: true,
            size: None,
            modification_time: None,
            media_type: None,
        });

        let settings = Settings::load().unwrap_or_default();

        // Hydrate in-memory caches from SQLite so first `ls` is instant.
        let trash_items_cached = index.load_trash_cache();
        let initial_trash: Vec<TrashRecord> = trash_items_cached
            .into_iter()
            .map(|(uid, name, is_folder)| TrashRecord { uid, name, is_folder })
            .collect();

        let devices_cached = index.load_devices_cache();
        let initial_devices: Vec<proton_drive_sdk::device_ops::Device> = devices_cached
            .into_iter()
            .filter_map(|row| {
                use proton_drive_sdk::api::devices::DeviceType;
                let device_type = match row.device_type_raw {
                    1 => DeviceType::Windows,
                    2 => DeviceType::MacOS,
                    _ => DeviceType::Linux,
                };
                let last_sync_time = row.last_sync_time_rfc.as_deref()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&chrono::Utc));
                Some(proton_drive_sdk::device_ops::Device {
                    device_id: row.device_id,
                    // volume_id and share_id are not needed for ls/cd — fill with root_uid's volume.
                    volume_id: row.root_uid.volume_id.clone(),
                    share_id: proton_drive_sdk::share::ShareId::new(row.root_uid.link_id.raw().to_string()),
                    root_uid: row.root_uid,
                    device_type,
                    name: row.name,
                    create_time: chrono::Utc::now(),
                    last_sync_time,
                })
            })
            .collect();

        Ok(Self {
            drive,
            photos,
            session,
            cwd: VirtualPath::my_files(),
            index,
            settings,
            root_uid,
            photos_root_uid,
            volume_id,
            should_quit: false,
            cancel: CancellationToken::new(),
            devices: Arc::new(parking_lot::RwLock::new(initial_devices)),
            trash_items: Arc::new(parking_lot::RwLock::new(initial_trash)),
            album_uids: Arc::new(dashmap::DashSet::new()),
        })
    }

    pub fn cwd_display(&self) -> String {
        self.cwd.display()
    }

    pub fn section_root_uid(&self) -> Option<NodeUid> {
        match &self.cwd.section {
            VfsSection::MyFiles => Some(self.root_uid.clone()),
            VfsSection::Photos => Some(self.photos_root_uid.clone()),
            _ => None,
        }
    }

    /// Resolves a VirtualPath to a NodeUid by walking the index, loading children on demand.
    pub async fn resolve_uid(&self, path: &VirtualPath) -> anyhow::Result<Option<NodeUid>> {
        // Handle the Computers section specially: component[0] is the device name.
        if path.section == VfsSection::Computers {
            if path.components.is_empty() {
                return Ok(None);
            }
            let device_name = path.components[0].clone();
            let root = {
                let cached = self.devices.read();
                cached.iter().find(|d| d.name.eq_ignore_ascii_case(&device_name)).map(|d| d.root_uid.clone())
            };
            let root = match root {
                Some(r) => r,
                None => {
                    // Not in cache yet — fetch and cache.
                    match self.drive.list_devices().await {
                        Ok(devs) => {
                            let r = devs.iter()
                                .find(|d| d.name.eq_ignore_ascii_case(&device_name))
                                .map(|d| d.root_uid.clone());
                            *self.devices.write() = devs;
                            match r {
                                Some(uid) => uid,
                                None => return Ok(None),
                            }
                        }
                        Err(_) => return Ok(None),
                    }
                }
            };
            let mut current = root;
            for component in &path.components[1..] {
                self.ensure_children_loaded(&current).await?;
                match self.index.find_child_by_name(&current, component) {
                    Some(uid) => current = uid,
                    None => return Ok(None),
                }
            }
            return Ok(Some(current));
        }

        let root = match &path.section {
            VfsSection::MyFiles => self.root_uid.clone(),
            VfsSection::Photos => self.photos_root_uid.clone(),
            _ => return Ok(None),
        };

        let mut current = root.clone();
        for (i, component) in path.components.iter().enumerate() {
            // At the Photos root, load albums only (not all individual photos).
            if i == 0 && path.section == VfsSection::Photos && current == root {
                self.ensure_photos_root_indexed().await?;
            } else {
                self.ensure_children_loaded(&current).await?;
            }
            match self.index.find_child_by_name(&current, component) {
                Some(uid) => current = uid,
                None => return Ok(None),
            }
        }
        Ok(Some(current))
    }

    /// Loads only album nodes into the index for the Photos root, ignoring individual photos.
    /// Marks the photos root as indexed so repeat calls are no-ops.
    pub async fn ensure_photos_root_indexed(&self) -> anyhow::Result<()> {
        if self.index.is_indexed(&self.photos_root_uid) {
            // Warm up album_uids from whatever is already cached under the photos root.
            // We treat every folder-typed child of the photos root as an album.
            if self.album_uids.is_empty() {
                for entry in self.index.get_children(&self.photos_root_uid) {
                    if entry.is_folder {
                        self.album_uids.insert(entry.uid);
                    }
                }
            }
            return Ok(());
        }

        let pb = crate::ui::spinner("Fetching albums…");
        let album_infos = match self.photos.iterate_albums().await {
            Ok(v) => { v }
            Err(e) => { pb.finish_and_clear(); return Err(e); }
        };

        let uids: Vec<NodeUid> = album_infos.iter().map(|a| a.uid.clone()).collect();
        if !uids.is_empty() {
            let nodes = match self.photos.enumerate_nodes(uids).await {
                Ok(v) => v,
                Err(e) => { pb.finish_and_clear(); return Err(e); }
            };
            for node in &nodes {
                // Track every album UID so ensure_children_loaded can use the photos API.
                if let proton_drive_sdk::utils::PotentialObject::Node(
                    proton_drive_sdk::node::Node::Album(a)
                ) = node {
                    self.album_uids.insert(a.base.uid.clone());
                }
                self.index.insert_node(node, Some(self.photos_root_uid.clone()));
            }
        }

        self.index.mark_indexed(&self.photos_root_uid);
        pb.finish_and_clear();
        Ok(())
    }

    /// Loads the direct children of `uid` from the server into the index if not already done.
    pub async fn ensure_children_loaded(&self, uid: &NodeUid) -> anyhow::Result<()> {
        if self.index.is_indexed(uid) {
            return Ok(());
        }
        // Albums use a different API endpoint from regular folders.
        if self.album_uids.contains(uid) {
            return self.ensure_album_children_loaded(uid).await;
        }

        let folder_name = self.index.get(uid)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| "folder".to_string());
        let pb = crate::ui::spinner(format!("Fetching '{}'…", folder_name));
        let stream = match self.drive.enumerate_folder_children(uid.clone()).await {
            Ok(s) => s,
            Err(e) => {
                pb.finish_and_clear();
                return Err(e);
            }
        };
        tokio::pin!(stream);
        while let Some(result) = stream.next().await {
            match result {
                Ok(node) => self.index.insert_node(&node, Some(uid.clone())),
                Err(e) => tracing::warn!("Degraded node in {uid}: {e}"),
            }
        }

        self.index.mark_indexed(uid);
        pb.finish_and_clear();
        Ok(())
    }

    /// Loads the photos inside an album into the index using the Photos API.
    pub async fn ensure_album_children_loaded(&self, album_uid: &NodeUid) -> anyhow::Result<()> {
        if self.index.is_indexed(album_uid) {
            return Ok(());
        }
        let album_name = self.index.get(album_uid)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| "album".to_string());
        let pb = crate::ui::spinner(format!("Fetching album '{}' — not yet cached…", album_name));

        let items = match self.photos.iterate_album(album_uid.clone()).await {
            Ok(v) => { v }
            Err(e) => { pb.finish_and_clear(); return Err(e); }
        };

        if !items.is_empty() {
            let uids: Vec<NodeUid> = items.into_iter().map(|i| i.uid).collect();
            match self.photos.enumerate_nodes(uids).await {
                Ok(nodes) => {
                    for node in &nodes {
                        // Force album_uid as parent — photo nodes carry their own
                        // parent_uid (photos volume root), which would override the
                        // fallback in insert_node and make them invisible under the album.
                        self.index.insert_node_force_parent(node, album_uid.clone());
                    }
                }
                Err(e) => { pb.finish_and_clear(); return Err(e); }
            }
        }

        self.index.mark_indexed(album_uid);
        pb.finish_and_clear();
        Ok(())
    }

    /// Spawns background tasks (event watcher only).
    pub fn spawn_background_tasks(&self) {
        crate::events::spawn_event_watcher(
            self.drive.clone(),
            self.index.clone(),
            self.volume_id.clone(),
        );
    }
}
