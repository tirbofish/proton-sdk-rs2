use std::path::PathBuf;

use egui::Ui;

use crate::{prefs, mount, ProtonDrive};

/// A single tracked operation (upload, download, sync, etc.).
pub struct TransferItem {
    /// Human-readable label, e.g. file name.
    pub label: String,
    /// What kind of operation this is.
    pub kind: TransferKind,
    /// Progress in `0.0..=1.0`. `None` means indeterminate.
    pub progress: Option<f32>,
    /// Current status of the transfer.
    pub status: TransferStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    Upload,
    Download,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStatus {
    /// Waiting in queue.
    Pending,
    /// Actively transferring.
    InProgress,
    /// Finished successfully.
    Done,
    /// Failed.
    Failed,
}

impl std::fmt::Display for TransferKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferKind::Upload => f.write_str("⬆ Upload"),
            TransferKind::Download => f.write_str("⬇ Download"),
        }
    }
}

impl std::fmt::Display for TransferStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferStatus::Pending => f.write_str("Pending"),
            TransferStatus::InProgress => f.write_str("In progress"),
            TransferStatus::Done => f.write_str("Done"),
            TransferStatus::Failed => f.write_str("Failed"),
        }
    }
}

impl ProtonDrive {
    pub fn status_page(&self, ui: &mut Ui) {
        ui.heading("Status");
        ui.add_space(8.0);

        if self.transfers.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.label("No active transfers.");
                ui.add_space(4.0);
                ui.weak("Uploads, downloads and sync activity will appear here.");
            });
            return;
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            for item in &self.transfers {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        // Kind icon + label
                        let kind_label = match item.kind {
                            TransferKind::Upload => "⬆",
                            TransferKind::Download => "⬇",
                        };
                        ui.label(kind_label);
                        ui.strong(&item.label);

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let status_color = match item.status {
                                TransferStatus::Pending => egui::Color32::GRAY,
                                TransferStatus::InProgress => ui.visuals().text_color(),
                                TransferStatus::Done => egui::Color32::from_rgb(80, 200, 80),
                                TransferStatus::Failed => egui::Color32::from_rgb(220, 60, 60),
                            };
                            ui.colored_label(status_color, item.status.to_string());
                        });
                    });

                    // Progress bar or spinner
                    match item.progress {
                        Some(p) => {
                            ui.add(egui::ProgressBar::new(p).show_percentage());
                        }
                        None if item.status == TransferStatus::InProgress => {
                            ui.spinner();
                        }
                        _ => {}
                    }
                });
                ui.add_space(4.0);
            }
        });

        // Request repaint while any transfer is active
        if self.transfers.iter().any(|t| {
            t.status == TransferStatus::InProgress || t.status == TransferStatus::Pending
        }) {
            ui.ctx().request_repaint();
        }
    }

    pub fn account_page(ui: &mut Ui, sign_out: &mut bool, username: &str, mount_path: &str) {
        ui.heading("Account");
        ui.label(format!("Signed in as {username}"));
        ui.label(format!("Mounted at {mount_path}"));
        ui.add_space(8.0);
        if ui.button("Sign out").clicked() {
            *sign_out = true;
        }
    }

    pub fn settings_page(&mut self, ui: &mut Ui) {
        ui.heading("Settings");
        ui.add_space(16.0);

        ui.heading("Mount");
        ui.add_space(8.0);
        ui.label("FUSE mount path (takes effect on next login):");
        ui.add_space(4.0);

        let mut path_str = self.prefs.mount_path.display().to_string();
        ui.horizontal(|ui| {
            let response = ui.add_sized([350.0, 28.0], egui::TextEdit::singleline(&mut path_str).hint_text("~/ProtonDrive"));
            if response.changed() {
                self.prefs.mount_path = PathBuf::from(&path_str);
            }
            if ui.button("Browse…").clicked() {
                if let Some(folder) = rfd::FileDialog::new()
                    .set_directory(&self.prefs.mount_path)
                    .pick_folder()
                {
                    self.prefs.mount_path = folder;
                }
            }
        });

        ui.add_space(4.0);

        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                if let Err(e) = prefs::save(&self.prefs) {
                    tracing::error!(error = %e, "failed to save preferences");
                }
            }
            if ui.button("Reset to default").clicked() {
                self.prefs.mount_path = mount::default_mount_path();
                if let Err(e) = prefs::save(&self.prefs) {
                    tracing::error!(error = %e, "failed to save preferences");
                }
            }
        });
    }
}