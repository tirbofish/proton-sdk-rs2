use std::path::PathBuf;

use egui::Ui;

use crate::{prefs, mount, transfers, ProtonDrive};

impl ProtonDrive {
    pub fn status_page(&self, ui: &mut Ui) {
        ui.heading("Status");
        ui.add_space(8.0);

        // Poll the transfer log (synchronous — uses std::sync::RwLock)
        let snapshot: Vec<transfers::TransferEntry> = self.transfer_log.snapshot();

        if snapshot.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.label("No active transfers.");
                ui.add_space(4.0);
                ui.weak("Uploads, downloads and sync activity will appear here.");
            });
            return;
        }

        let mut cancel_indices: Vec<usize> = Vec::new();

        egui::ScrollArea::vertical().show(ui, |ui| {
            for (idx, item) in snapshot.iter().enumerate() {
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        let kind_label = match item.kind {
                            transfers::TransferKind::Upload => "⬆",
                            transfers::TransferKind::Download => "⬇",
                        };
                        ui.label(kind_label);
                        ui.strong(&item.name);

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // Cancel button for active transfers
                            if matches!(item.status, transfers::TransferStatus::InProgress | transfers::TransferStatus::Pending) {
                                if ui.small_button("✕ Cancel").clicked() {
                                    cancel_indices.push(idx);
                                }
                            }

                            let (color, text) = match item.status {
                                transfers::TransferStatus::Pending => (egui::Color32::GRAY, "Pending"),
                                transfers::TransferStatus::InProgress => (ui.visuals().text_color(), "In progress"),
                                transfers::TransferStatus::Done => (egui::Color32::from_rgb(80, 200, 80), "Done"),
                                transfers::TransferStatus::Failed => (egui::Color32::from_rgb(220, 60, 60), "Failed"),
                            };
                            ui.colored_label(color, text);
                        });
                    });

                    match item.progress {
                        Some(p) => {
                            let bar = egui::ProgressBar::new(p).show_percentage();
                            // Show bytes if we have total_bytes
                            let bar = if item.total_bytes > 0 {
                                let transferred_mb = item.bytes_transferred as f64 / 1_048_576.0;
                                let total_mb = item.total_bytes as f64 / 1_048_576.0;
                                bar.text(format!("{transferred_mb:.1} / {total_mb:.1} MB"))
                            } else {
                                bar
                            };
                            ui.add(bar);
                        }
                        None if item.status == transfers::TransferStatus::InProgress => {
                            ui.spinner();
                        }
                        _ => {}
                    }

                    if let Some(ref err) = item.error {
                        ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err);
                    }
                });
                ui.add_space(4.0);
            }
        });

        // Process cancel requests
        for idx in cancel_indices {
            self.transfer_log.cancel(idx);
        }

        // Repaint while active transfers exist
        if snapshot.iter().any(|t| {
            matches!(t.status, transfers::TransferStatus::InProgress | transfers::TransferStatus::Pending)
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