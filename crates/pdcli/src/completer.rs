use reedline::{Completer, Span, Suggestion};
use std::sync::Arc;

use proton_drive_sdk::device_ops::Device;

use crate::app::TrashRecord;
use crate::index::NodeIndex;
use crate::vfs::{VfsSection, VirtualPath};

pub struct DriveCompleter {
    pub index: Arc<NodeIndex>,
    pub cwd: Arc<parking_lot::RwLock<VirtualPath>>,
    pub devices: Arc<parking_lot::RwLock<Vec<Device>>>,
    pub trash_items: Arc<parking_lot::RwLock<Vec<TrashRecord>>>,
}

impl DriveCompleter {
    pub fn new(
        index: Arc<NodeIndex>,
        cwd: Arc<parking_lot::RwLock<VirtualPath>>,
        devices: Arc<parking_lot::RwLock<Vec<Device>>>,
        trash_items: Arc<parking_lot::RwLock<Vec<TrashRecord>>>,
    ) -> Self {
        Self { index, cwd, devices, trash_items }
    }
}

impl Completer for DriveCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let line = &line[..pos];

        let (word_start, partial, in_single_quote) = extract_word(line);
        let span = Span::new(word_start, pos);

        let cwd = self.cwd.read().clone();

        // /Trash section: complete from the last-seen trash listing.
        if cwd.section == VfsSection::Trash {
            let items = self.trash_items.read();
            let prefix = partial.to_lowercase();
            return items.iter()
                .filter(|r| r.name.to_lowercase().starts_with(&prefix))
                .map(|r| {
                    let value = quote_if_needed(&r.name, in_single_quote, r.is_folder);
                    Suggestion {
                        value,
                        description: None,
                        style: None,
                        extra: None,
                        span,
                        append_whitespace: !r.is_folder,
                    }
                })
                .collect();
        }

        // /Computers section: complete device names or device's folder tree.
        if cwd.section == VfsSection::Computers {
            if cwd.components.is_empty() {
                // At /Computers — complete device names from cache.
                let devs = self.devices.read();
                let prefix = partial.to_lowercase();
                return devs.iter()
                    .filter(|d| d.name.to_lowercase().starts_with(&prefix))
                    .map(|d| {
                        let value = quote_if_needed(&d.name, in_single_quote, true);
                        Suggestion {
                            value,
                            description: Some(format!("{:?}", d.device_type)),
                            style: None,
                            extra: None,
                            span,
                            append_whitespace: false,
                        }
                    })
                    .collect();
            }
            // Inside /Computers/<device>/... — complete using the device's index.
            let device_name = &cwd.components[0];
            let root_uid = {
                let devs = self.devices.read();
                devs.iter()
                    .find(|d| d.name.eq_ignore_ascii_case(device_name))
                    .map(|d| d.root_uid.clone())
            };
            let Some(mut current) = root_uid else { return vec![]; };
            for component in &cwd.components[1..] {
                match self.index.find_child_by_name(&current, component) {
                    Some(uid) => current = uid,
                    None => return vec![],
                }
            }
            if !self.index.is_indexed(&current) { return vec![]; }
            let prefix = partial.to_lowercase();
            return self.index.get_children(&current).into_iter()
                .filter(|e| e.name.to_lowercase().starts_with(&prefix))
                .map(|e| make_suggestion(&e, in_single_quote, span))
                .collect();
        }

        // MyFiles / Photos: complete via index.
        let root_uid = match &cwd.section {
            VfsSection::MyFiles | VfsSection::Photos => find_uid_in_index(&self.index, &cwd),
            _ => return vec![],
        };

        let Some(parent_uid) = root_uid else { return vec![]; };
        if !self.index.is_indexed(&parent_uid) { return vec![]; }

        let prefix_lower = partial.to_lowercase();
        self.index
            .get_children(&parent_uid)
            .into_iter()
            .filter(|e| e.name.to_lowercase().starts_with(&prefix_lower))
            .map(|e| make_suggestion(&e, in_single_quote, span))
            .collect()
    }
}

fn make_suggestion(e: &crate::index::IndexEntry, in_single_quote: bool, span: Span) -> Suggestion {
    let value = quote_if_needed(&e.name, in_single_quote, e.is_folder);
    Suggestion {
        value,
        description: e.size.map(|s| format_size(s)),
        style: None,
        extra: None,
        span,
        append_whitespace: !e.is_folder,
    }
}

fn quote_if_needed(name: &str, in_single_quote: bool, is_folder: bool) -> String {
    let suffix = if is_folder { "/" } else { "" };
    if in_single_quote || name.contains(' ') {
        format!("'{}{}'", name, suffix)
    } else {
        format!("{}{}", name, suffix)
    }
}

fn extract_word(line: &str) -> (usize, &str, bool) {
    let bytes = line.as_bytes();
    let mut i = line.len();

    // Scan backward looking for an unescaped opening single quote or space boundary.
    while i > 0 {
        let c = bytes[i - 1];
        if c == b'\'' {
            // Found an opening single quote — span includes the quote so the
            // replacement also removes it, preventing double-quote artifacts.
            return (i - 1, &line[i..], true);
        }
        if c == b' ' {
            return (i, &line[i..], false);
        }
        i -= 1;
    }
    (0, line, false)
}

/// Walk the index to find the NodeUid for the *current directory* of `path`.
/// Returns the UID of the final component (not awaiting any IO).
fn find_uid_in_index(index: &NodeIndex, path: &VirtualPath) -> Option<proton_drive_sdk::node::NodeUid> {
    // The root UIDs are stored under the section root names in the index.
    // We walk the components from the section root.
    // The section roots were inserted with names "MyFiles" / "Photos" - find them by name.
    let section_name = match path.section {
        VfsSection::MyFiles => "MyFiles",
        VfsSection::Photos => "Photos",
        _ => return None,
    };

    // Find the section root uid: look for an entry with that name and no parent.
    let root_uid = index
        .find_root_by_name(section_name)?;

    let mut current = root_uid;
    for component in &path.components {
        current = index.find_child_by_name(&current, component)?;
    }
    Some(current)
}

fn format_size(bytes: i64) -> String {
    let b = bytes as f64;
    if b < 1024.0 { format!("{b:.0} B") }
    else if b < 1_048_576.0 { format!("{:.1} KB", b / 1024.0) }
    else if b < 1_073_741_824.0 { format!("{:.1} MB", b / 1_048_576.0) }
    else { format!("{:.2} GB", b / 1_073_741_824.0) }
}
