//! The controls and the preview pane, shared by the floating window and the main window.
//!
//! Every control goes through [`crate::form`], so labels, dropdowns and sliders line up. The
//! sections run top to bottom in the order of the workflow; Preview / Scan / Cancel are a bar
//! pinned at the bottom of the controls.

use crate::offset;
use crate::settings::SaveMode;
use crate::settings::{DetectSettings, CONFIDENCE, MARGIN_MM, REFINE_MM, SEARCH_MM};
use crate::worker::PickKind;
use crate::{canvas, crop, frames, text, view, ScanUi, ACCENT};
use egui::{Color32, ComboBox, RichText, Ui};
use latent_image_edit::{BitDepth, OutputFormat};
use latent_image_edit_ui::form::{button_row, form, form_buttons, Btn, SliderOpts};
use latent_image_edit_ui::sliders::{adjust_groups, preset_row, section};
use latent_scan_core::{ColorMode, Source};

/// Height reserved under the preview for its info line.
const INFO_HEIGHT: f32 = 24.0;
/// Width of the small ⟳ button next to the scanner dropdown, with its spacing.
const REFRESH_W: f32 = 36.0;
/// Height of the settings of the floating window before they scroll.
const WINDOW_SETTINGS_H: f32 = 470.0;

impl ScanUi {
    /// Layout of the floating window: controls on the left, preview on the right.
    pub(crate) fn window_ui(&mut self, ui: &mut Ui) {
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(310.0);
                self.sections(ui, Some(WINDOW_SETTINGS_H));
                ui.separator();
                self.action_bar(ui);
            });
            ui.separator();
            ui.vertical(|ui| self.preview_pane(ui));
        });
    }

    /// The settings with the action bar pinned below them (side panel of the main window).
    pub(crate) fn controls_panel(&mut self, ui: &mut Ui) {
        egui::TopBottomPanel::bottom("scan-actions")
            .resizable(false)
            .show_inside(ui, |ui| {
                ui.add_space(4.0);
                self.action_bar(ui);
                ui.add_space(2.0);
            });
        self.sections(ui, None);
    }

    /// The settings as collapsing sections, in workflow order.
    fn sections(&mut self, ui: &mut Ui, max_height: Option<f32>) {
        let idle = self.busy.is_none();
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(h) = max_height {
            area = area.max_height(h);
        }
        area.show(ui, |ui| {
            section(ui, "Scanner & profile", |ui| {
                ui.add_enabled_ui(idle, |ui| self.section_scanner(ui));
            });
            section(ui, "Auto-adjust frames", |ui| {
                ui.add_enabled_ui(idle, |ui| self.section_auto_adjust(ui));
            });
            section(ui, "Save profile", |ui| {
                ui.add_enabled_ui(idle, |ui| self.profile_save_row(ui));
            });
            section(ui, "Image", |ui| self.section_image(ui));
            section(ui, "Save to", |ui| {
                ui.add_enabled_ui(idle, |ui| self.section_save_to(ui));
            });
        });
    }

    /// Status, progress, the names the batch will write, and the buttons: the last thing of
    /// the workflow, so it is the bottom of the panel.
    fn action_bar(&mut self, ui: &mut Ui) {
        let idle = self.busy.is_none();
        if let Some((is_error, msg)) = &self.status {
            let text = RichText::new(msg);
            ui.label(if *is_error {
                text.color(Color32::LIGHT_RED)
            } else {
                text.weak()
            });
        }
        if let Some(b) = &self.busy {
            ui.add(
                egui::ProgressBar::new(b.progress)
                    .desired_width(ui.available_width())
                    .text(b.what.clone()),
            );
        }
        let frames = self.frame_areas().len();
        let names = latent_scan_archive::naming::range_label(&self.prefix, self.start, frames);
        if !names.is_empty() {
            ui.weak(names);
        }

        let have_device = !self.devices.is_empty();
        let can_scan = have_device && self.dest().is_some() && frames > 0;
        let scan_label = if frames > 1 {
            format!("Scan {frames} frames")
        } else {
            "Scan".into()
        };
        let mut buttons = vec![
            Btn::new("Preview")
                .enabled(idle && have_device)
                .hover("Scan the whole bed at low resolution to place the frames"),
            Btn::new(&scan_label)
                .enabled(idle && can_scan)
                .primary()
                .hover("Scan every frame at full resolution and save it"),
        ];
        let multi = frames > 1;
        if multi {
            buttons.push(
                Btn::new("Scan active")
                    .enabled(idle && can_scan)
                    .hover("Scan only the selected frame at full resolution and save it"),
            );
        }
        if !idle {
            buttons.push(Btn::new("Cancel"));
        }
        match button_row(ui, &buttons) {
            Some(0) => self.start_preview(ui.ctx()),
            Some(1) => self.start_scan(ui.ctx(), false),
            Some(2) if multi => self.start_scan(ui.ctx(), true),
            Some(2) | Some(3) => {
                if let Some(b) = &self.busy {
                    b.cancel.cancel();
                }
            }
            _ => {}
        }
    }

    fn section_scanner(&mut self, ui: &mut Ui) {
        let device_name = self
            .devices
            .get(self.device)
            .map_or(text::NO_SCANNER.to_string(), |(d, _)| {
                format!("{} {}", d.vendor, d.model)
            });
        let profile_name = self
            .profiles
            .all()
            .get(self.profile)
            .map_or(String::new(), |p| p.name.clone());
        let resolutions = self.resolutions();
        let preview_resolutions = self.preview_resolutions();
        let source = self
            .profiles
            .all()
            .get(self.profile)
            .map_or(Source::Flatbed, |p| p.source);
        let profile_before = self.profile;
        let device_before = self.device;
        let mut rediscover = false;

        form(ui, "scanner", |f| {
            f.row("Scanner", |ui, rest| {
                ui.horizontal(|ui| {
                    ComboBox::from_id_salt("device")
                        .width((rest - REFRESH_W).max(40.0))
                        .selected_text(device_name)
                        .show_ui(ui, |ui| {
                            for (i, (d, _)) in self.devices.iter().enumerate() {
                                ui.selectable_value(
                                    &mut self.device,
                                    i,
                                    format!("{} {}", d.vendor, d.model),
                                );
                            }
                        });
                    if ui.button("⟳").on_hover_text("Look for scanners").clicked() {
                        rediscover = true;
                    }
                });
            });
            f.combo("Profile", "profile", profile_name, |ui| {
                let device = self.devices.get(self.device).map(|(d, _)| d.clone());
                let shown = self
                    .profiles
                    .all()
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| device.as_ref().is_none_or(|d| p.fits(d)));
                for (i, p) in shown {
                    ui.selectable_value(&mut self.profile, i, &p.name)
                        .on_hover_text(&p.description);
                }
            });
            f.combo("Resolution", "dpi", format!("{} dpi", self.dpi), |ui| {
                for d in resolutions {
                    ui.selectable_value(&mut self.dpi, d, format!("{d} dpi"));
                }
            });
            let shown = self.preview_dpi_for(source);
            let window = self
                .source_caps(source)
                .and_then(|c| c.max_area)
                .map_or((216.0, 250.0), |a| (a.width, a.height));
            let before = self.preview_dpi;
            f.combo("Preview", "preview-dpi", format!("{shown} dpi"), |ui| {
                for d in preview_resolutions {
                    let hover = preview_hint(d, window);
                    if ui
                        .selectable_label(shown == d, format!("{d} dpi"))
                        .on_hover_text(hover)
                        .clicked()
                    {
                        self.preview_dpi = Some(d);
                    }
                }
            })
            .on_hover_text("Resolution of the overview scan; higher shows the frames in more detail but takes longer");
            if self.preview_dpi != before {
                self.settings.preview_dpi = self.preview_dpi;
                self.settings_dirty = true;
            }
            f.combo(
                "Colour",
                "color",
                match self.color {
                    ColorMode::Color => "Colour",
                    ColorMode::Gray => "Gray (B&W)",
                },
                |ui| {
                    ui.selectable_value(&mut self.color, ColorMode::Color, "Colour");
                    ui.selectable_value(&mut self.color, ColorMode::Gray, "Gray (B&W)");
                },
            );
            f.check(
                "Film",
                "negative",
                &mut self.negative,
                "Invert and remove the orange mask",
            );
            f.combo("Format", "format", self.format.label(), |ui| {
                for format in [OutputFormat::Tiff, OutputFormat::Png, OutputFormat::Jpeg] {
                    ui.selectable_value(&mut self.format, format, format.label());
                }
            });
            let before = self.depth;
            if self.format == OutputFormat::Jpeg {
                f.value("Bit depth", "8 bit (JPEG is always 8 bit)");
            } else {
                f.combo(
                    "Bit depth",
                    "depth",
                    format!("{} bit", self.depth.bits()),
                    |ui| {
                        ui.selectable_value(&mut self.depth, BitDepth::Sixteen, "16 bit");
                        ui.selectable_value(
                            &mut self.depth,
                            BitDepth::Eight,
                            "8 bit (half the size)",
                        );
                    },
                );
            }
            if self.depth != before {
                self.remember_destination();
            }
        });

        self.offset_rows(ui);

        if rediscover {
            self.start_discover(ui.ctx());
        }
        if self.device != device_before {
            self.keep_profile_for_scanner();
        }
        if self.profile != profile_before {
            self.select_profile(self.profile);
        }
    }

    /// The scan offset: two sliders to fine-tune by hand and what was learned from the checks.
    fn offset_rows(&mut self, ui: &mut Ui) {
        let key = self.offset_key();
        let mut changed = false;
        let opts = SliderOpts::default().suffix(" mm").decimals(2).step(0.05);
        form(ui, "scan-offset", |f| {
            for (axis, label) in ["Offset X", "Offset Y"].into_iter().enumerate() {
                changed |= f.slider(
                    label,
                    &mut self.settings.offset.manual[axis],
                    -offset::LIMIT_MM..=offset::LIMIT_MM,
                    opts.default_value(0.0),
                    "Fine-tune where full-resolution scans start, on top of what was learned; positive moves the scan right / down on the bed",
                );
            }
            let learned = key
                .and_then(|k| self.settings.offset.learned_for(k))
                .map_or_else(
                    || "nothing yet".to_string(),
                    |l| {
                        let (x, y) = l.offset();
                        format!("{x:+.2} / {y:+.2} mm ({} scans)", l.errors.len())
                    },
                );
            f.value("Learned", &learned);
        });
        if let Some(k) = key.filter(|k| self.settings.offset.learned_for(*k).is_some()) {
            if form_buttons(
                ui,
                &[Btn::new("Forget learned offset")
                    .hover("Start measuring again for this source and these resolutions")],
            )
            .is_some()
            {
                self.settings.offset.forget(k);
                changed = true;
            }
        }
        if changed {
            self.settings_dirty = true;
        }
    }

    /// Name field and "Save profile": stores the frames on the preview, the colour mode, film type,
    /// resolution and rotation under that name.
    fn profile_save_row(&mut self, ui: &mut Ui) {
        form(ui, "profile-save", |f| {
            f.text("Profile name", &mut self.profile_name);
        });
        let name_given = !self.profile_name.trim().is_empty();
        let hover = if self.preview.is_none() {
            "Scan a preview first: the frames are saved as placed on it"
        } else if !name_given {
            "Type a name for the profile"
        } else {
            "Store the frames, colour mode, film type and rotation under this name"
        };
        let save = Btn::new("Save profile")
            .enabled(self.preview.is_some() && name_given)
            .hover(hover);
        if form_buttons(ui, &[save]).is_some() {
            self.save_profile();
        }
    }

    fn section_auto_adjust(&mut self, ui: &mut Ui) {
        let buttons = [
            Btn::new("Auto-adjust frames")
                .enabled(self.preview.is_some())
                .hover("Fit the frames to the film on this preview"),
            Btn::new("Defaults").hover("Back to the default values below"),
        ];
        match form_buttons(ui, &buttons) {
            Some(0) => self.auto_adjust(ui.ctx()),
            Some(1) => {
                self.settings.detect = DetectSettings::default();
                self.settings_dirty = true;
            }
            _ => {}
        }
        ui.add_space(4.0);

        form(ui, "auto-adjust-check", |f| {
            f.check(
                "Before scan",
                "adjust each frame",
                &mut self.adjust_before_scan,
                "Pre-scan each frame at 600 dpi and fit it to the film first",
            );
        });
        if detect_sliders(ui, &mut self.settings.detect) {
            self.settings_dirty = true;
        }
    }

    /// Minimal image adjustments; they show in the preview and apply to the saved scans.
    fn section_image(&mut self, ui: &mut Ui) {
        let colour = self.color == ColorMode::Color;
        let (have_preview, frame_count, active) =
            (self.preview.is_some(), self.frames.len(), self.active);
        ui.weak(format!(
            "Frame {} of {frame_count}: these values are for this frame only",
            active + 1
        ));
        ui.add_space(2.0);

        let current = *self.active_adjust_mut();
        let loaded = preset_row(ui, "image-preset", &mut self.presets, (&current, None));
        ui.add_space(4.0);
        let a = self.active_adjust_mut();
        if let Some(p) = loaded {
            // Warmth and tint only exist for colour; keep the frame's own values for gray.
            let mut v = p.adjust();
            if !colour {
                (v.warmth, v.tint) = (a.warmth, a.tint);
            }
            *a = v;
        }
        adjust_groups(ui, "image", a, None, colour);
        let neutral = a.is_neutral();
        ui.add_space(4.0);

        // The buttons act on `self`, so they run after the sliders' borrow of the values ended.
        let auto = [
            Btn::new("Auto")
                .enabled(have_preview)
                .hover("Sets black and white point from this frame's pixels on the preview"),
            Btn::new("Auto all")
                .enabled(have_preview)
                .hover("Auto for every frame, each measured on its own area"),
        ];
        let manage = [
            Btn::new("Reset")
                .enabled(!neutral)
                .hover("Back to neutral for this frame"),
            Btn::new("Copy to all")
                .enabled(frame_count > 1)
                .hover("Give every frame this frame's values"),
        ];
        match (form_buttons(ui, &auto), form_buttons(ui, &manage)) {
            (Some(0), _) => self.auto_active(),
            (Some(_), _) => self.auto_all(),
            (_, Some(0)) => self.reset_adjust(),
            (_, Some(_)) => self.copy_adjust_to_all(),
            _ => {}
        }
    }

    fn section_save_to(&mut self, ui: &mut Ui) {
        if self.standalone {
            ui.horizontal(|ui| {
                let mut mode = self.mode;
                ui.radio_value(&mut mode, SaveMode::Folder, "Folder");
                ui.radio_value(&mut mode, SaveMode::Archive, "Latent archive");
                if mode != self.mode {
                    self.set_mode(mode, ui.ctx());
                }
            });
            ui.add_space(2.0);
        }
        match self.mode {
            SaveMode::Folder => self.folder_chooser(ui),
            SaveMode::Archive => self.archive_chooser(ui),
        }
        form(ui, "naming", |f| {
            f.text("Prefix", &mut self.prefix);
            f.number("Start no.", &mut self.start, 0..=9999);
        });
    }

    /// The plain save folder: its path, `Change…` and `Open`.
    fn folder_chooser(&mut self, ui: &mut Ui) {
        form(ui, "folder", |f| {
            f.value("Folder", &self.folder.display().to_string());
        });
        let is_dir = self.folder.is_dir();
        let buttons = [
            Btn::new("Change…").hover("Choose another folder"),
            Btn::new("Open")
                .enabled(is_dir)
                .hover("Show the folder in the file manager"),
        ];
        match form_buttons(ui, &buttons) {
            Some(0) => self.browse(PickKind::SaveFolder, ui.ctx()),
            Some(_) => open_in_file_manager(&self.folder),
            None => {}
        }
        if !is_dir {
            ui.weak("The folder is created with the first scan.");
        }
        ui.add_space(4.0);
    }

    /// The latent archive: its root (`Browse…`), then the film (table and row) to attach to.
    fn archive_chooser(&mut self, ui: &mut Ui) {
        if self.standalone {
            let shown = self
                .archive
                .as_ref()
                .map_or("not chosen yet".to_string(), |p| p.display().to_string());
            form(ui, "archive", |f| {
                f.value("Archive", &shown);
            });
            if form_buttons(
                ui,
                &[Btn::new("Browse…").hover("Choose the archive's root folder")],
            )
            .is_some()
            {
                self.browse(PickKind::ArchiveRoot, ui.ctx());
            }
            ui.add_space(4.0);
        }
        if self.archive.is_none() {
            return;
        }
        if self.targets.is_empty() {
            ui.weak(text::NO_TARGET);
        } else {
            self.target_pickers(ui);
        }
        ui.add_space(4.0);
    }

    /// Table and film (row) of the archive.
    fn target_pickers(&mut self, ui: &mut Ui) {
        self.table = self.table.min(self.targets.len() - 1);
        let table_before = self.table;
        let table_name = self.targets[self.table].name.clone();
        let rows = self.targets[self.table].rows.clone();
        let label = |r: &(u64, String)| {
            if r.1.is_empty() {
                format!("{}", r.0)
            } else {
                format!("{}  {}", r.0, r.1)
            }
        };
        let film = rows
            .iter()
            .find(|r| Some(r.0) == self.row)
            .map(label)
            .unwrap_or_default();

        form(ui, "targets", |f| {
            f.combo("Table", "table", table_name, |ui| {
                for (i, t) in self.targets.iter().enumerate() {
                    ui.selectable_value(&mut self.table, i, &t.name);
                }
            });
            f.combo("Film", "row", film, |ui| {
                for r in &rows {
                    ui.selectable_value(&mut self.row, Some(r.0), label(r));
                }
            });
        });

        // Another table: its first film, until the user picks one.
        let rows = &self.targets[self.table].rows;
        if self.table != table_before || !rows.iter().any(|r| Some(r.0) == self.row) {
            self.row = rows.first().map(|r| r.0);
        }
    }

    /// Above the preview, in one line that only breaks when the pane is too narrow: the
    /// frames (add, delete, select) and the rotation of the picture.
    fn frame_bar(&mut self, ui: &mut Ui) {
        let idle = self.busy.is_none();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Frames").strong());
            if ui
                .add_enabled(idle, egui::Button::new("Add frame"))
                .clicked()
            {
                self.add_frame();
            }
            if ui
                .add_enabled(
                    idle && self.frames.len() > 1,
                    egui::Button::new("Delete frame"),
                )
                .clicked()
            {
                self.remove_frame();
            }
            for i in 0..self.frames.len() {
                ui.selectable_value(&mut self.active, i, (i + 1).to_string());
            }
            let zoom_hint = "Zoom onto the active frame and follow it. Mouse wheel: zoom, middle or right button drag: pan.";
            if ui
                .add_enabled(
                    self.preview.is_some(),
                    egui::SelectableLabel::new(self.zoom.is_some(), "Zoom to frame"),
                )
                .on_hover_text(zoom_hint)
                .clicked()
            {
                self.toggle_zoom_to_frame();
            }
            ui.separator();
            if ui
                .add_enabled(idle, egui::Button::new("⟲"))
                .on_hover_text("Rotate left")
                .clicked()
            {
                self.rotate = (self.rotate + 270) % 360;
            }
            if ui
                .add_enabled(idle, egui::Button::new("⟳"))
                .on_hover_text("Rotate right")
                .clicked()
            {
                self.rotate = (self.rotate + 90) % 360;
            }
            ui.weak(format!("{}°", self.rotate))
                .on_hover_text("Turns the preview and the saved scans");
        });
        ui.separator();
    }

    pub(crate) fn preview_pane(&mut self, ui: &mut Ui) {
        self.frame_bar(ui);
        let mut avail = ui.available_size();
        avail.y = (avail.y - INFO_HEIGHT).max(50.0);
        let Some(p) = &self.preview else {
            ui.allocate_ui(avail, |ui| ui.weak(text::PREVIEW_HINT));
            return;
        };
        // A picture turned by 90 or 270 degrees has width and height swapped.
        let (aw, ah) = if self.rotate.is_multiple_of(180) {
            (p.area.width, p.area.height)
        } else {
            (p.area.height, p.area.width)
        };
        let aspect = (aw / ah.max(1e-6)) as f32;
        let (tex, area, dpi) = (p.tex.id(), p.area, p.dpi);
        let (frames, active, rotate) = (&self.frames, self.active, self.rotate);
        // The frames own the primary drag; hold Space or Ctrl to grab the picture with it instead
        // (the middle and right buttons always do).
        let panning = ui.input(|i| i.modifiers.command || i.key_down(egui::Key::Space));
        let canvas = canvas::show(
            ui,
            avail,
            tex,
            aspect,
            &mut self.zoom,
            panning,
            |zoom, shape| {
                view::follow_frame(zoom, frames, active, rotate, shape);
            },
        );
        let (rect, resp, window) = (canvas.rect, canvas.resp, canvas.window);
        // Frames live in bed coordinates; they are edited on the turned picture.
        let rot = self.rotate;
        let mut view: Vec<_> = self.frames.iter().map(|c| crop::to_view(*c, rot)).collect();
        let before = view.clone();
        let active_before = self.active;
        crop::overlay(
            ui,
            &resp,
            rect,
            window,
            &mut view,
            &mut self.active,
            &mut self.grab,
            ACCENT,
        );
        if panning {
            // The drag moved the picture: the frames stay as they were.
            self.active = active_before;
            self.grab = None;
            if resp.hovered() && self.zoom.is_some() {
                let icon = if resp.dragged() {
                    egui::CursorIcon::Grabbing
                } else {
                    egui::CursorIcon::Grab
                };
                ui.ctx().set_cursor_icon(icon);
            }
        } else if view != before {
            self.frames = view.iter().map(|c| crop::from_view(*c, rot)).collect();
        }
        let c = frames::crop_to_mm(area, self.frames[self.active.min(self.frames.len() - 1)]);
        let zoom = self
            .zoom
            .map_or(String::new(), |z| format!(" · zoom {:.1}×", z.zoom));
        ui.weak(format!(
            "frame {} of {}: {:.1} × {:.1} mm at {:.1}, {:.1} · preview {:.0} × {:.0} mm at {dpi} dpi{zoom}",
            self.active + 1,
            self.frames.len(),
            c.width,
            c.height,
            c.x,
            c.y,
            area.width,
            area.height
        ));
    }
}

/// Size of the whole window (mm) scanned at `dpi`, 8-bit colour.
fn preview_hint(dpi: u32, (width, height): (f64, f64)) -> String {
    let px = |mm: f64| (mm / 25.4 * dpi as f64).round() as u64;
    let (w, h) = (px(width), px(height));
    format!(
        "{w} x {h} px, about {} MB while scanning",
        w * h * 3 / 1_000_000
    )
}

/// Shows a folder in the system's file manager (best effort).
fn open_in_file_manager(dir: &std::path::Path) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener).arg(dir).spawn();
}

/// The four auto-adjust values as sliders; returns whether one changed (the values are then
/// clamped to their ranges).
pub(crate) fn detect_sliders(ui: &mut Ui, d: &mut DetectSettings) -> bool {
    let defaults = DetectSettings::default();
    let mut changed = false;
    let mm = SliderOpts::default().suffix(" mm");
    form(ui, "auto-adjust", |f| {
        changed |= f.slider(
            "Search range",
            &mut d.search_mm,
            SEARCH_MM.0..=SEARCH_MM.1,
            mm.default_value(defaults.search_mm),
            "How far the whole strip may have moved",
        );
        changed |= f.slider(
            "Refine",
            &mut d.refine_mm,
            REFINE_MM.0..=REFINE_MM.1,
            mm.default_value(defaults.refine_mm),
            "How far a single frame may differ from the strip (0 = same shift)",
        );
        changed |= f.slider(
            "Min. confidence",
            &mut d.min_confidence,
            CONFIDENCE.0..=CONFIDENCE.1,
            SliderOpts::default()
                .decimals(2)
                .default_value(defaults.min_confidence as f64),
            "Weaker matches keep the saved position",
        );
        changed |= f.slider(
            "Margin",
            &mut d.inset_mm,
            MARGIN_MM.0..=MARGIN_MM.1,
            mm.default_value(defaults.inset_mm),
            "How far the frames lie inside the visible frame edge",
        );
    });
    if changed {
        *d = d.clamped();
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    use egui::{pos2, Pos2};
    use latent_image_edit::tone::Adjust;
    use latent_image_edit_ui::form::{test_support::Harness, LABEL_W};

    /// Where the sliders of a form are, one position per row (inside the track). The form
    /// starts at the panel's top-left; rows are `row_h` high with a 6 px gap.
    fn row_positions(origin: Pos2, row_h: f32, rows: usize) -> Vec<Pos2> {
        (0..rows)
            .map(|i| {
                pos2(
                    origin.x + LABEL_W + 40.0,
                    origin.y + i as f32 * (row_h + 6.0) + row_h / 2.0,
                )
            })
            .collect()
    }
    use latent_image_edit_ui::presets::PresetUi;

    /// Where the sliders of a form are, one position per row (inside the track). The form
    /// starts at the panel's top-left; rows are `row_h` high with a 6 px gap.
    #[test]
    fn the_save_profile_section_has_the_profile_name_and_a_save_button_that_waits_for_a_preview() {
        let mut h = Harness::new();
        let mut ui = ScanUi::new(std::env::temp_dir());
        ui.profile_name = "mine".into();
        for _ in 0..3 {
            h.frame(vec![], |u| ui.profile_save_row(u));
        }
        // Without a preview nothing can be saved, and nothing is reported.
        ui.save_profile();
        assert!(ui.status.is_none());
        ui.profile_name = "  ".into();
        ui.save_profile();
        assert!(ui.status.is_none());
    }

    #[test]
    fn the_preset_row_draws_and_loads_nothing_until_a_preset_is_chosen() {
        let mut h = Harness::new();
        let mut p = PresetUi::default();
        let a = Adjust::default();
        for _ in 0..3 {
            h.frame(vec![], |ui| {
                assert!(preset_row(ui, "t", &mut p, (&a, None)).is_none());
            });
        }
        assert!(p.error.is_none());
        assert!(p.presets.get(crate::presets::SILVERFAST_FP4).is_some());
    }

    #[test]
    fn the_auto_adjust_sliders_reset_to_the_detector_defaults() {
        let mut h = Harness::new();
        let mut d = DetectSettings {
            search_mm: 6.0,
            refine_mm: 2.0,
            min_confidence: 0.6,
            inset_mm: 1.5,
        };
        let mut grid = (Pos2::ZERO, 0.0_f32);
        let mut changed = vec![];
        for _ in 0..3 {
            h.frame(vec![], |ui| {
                detect_sliders(ui, &mut d);
                grid = (ui.min_rect().min, ui.spacing().interact_size.y);
            });
        }
        for pos in row_positions(grid.0, grid.1, 4) {
            h.double_click(pos, |ui| changed.push(detect_sliders(ui, &mut d)));
        }
        assert_eq!(d, DetectSettings::default());
        assert!(
            changed.iter().any(|&c| c),
            "a reset is reported so the settings get saved"
        );
    }
}
