//! Image strip of the selected row's scans (bottom panel of the main window), modelled on the
//! strip in latent, plus a large view of one image.

use crate::settings::SaveMode;
use crate::thumbs::Thumb;
use crate::{targets, ScanUi, ACCENT};
use egui::{Color32, Rect, RichText, Stroke, Ui, Vec2};
use std::path::PathBuf;

/// Thumbnail size of the strip tiles (also what latent caches for its strip).
const STRIP_THUMB: u32 = 320;

/// What the strip lists: a plain folder, or the images of one row of an archive.
#[derive(Debug, Clone, PartialEq)]
pub enum StripKey {
    Folder(PathBuf),
    Row(String, u64),
}

#[derive(Default)]
pub struct StripState {
    /// Reload the row's image list on the next frame (after a save, a new target, ...).
    pub dirty: bool,
    /// What the list belongs to.
    key: Option<StripKey>,
    pub(crate) paths: Vec<String>,
    pub(crate) selected: Option<usize>,
    /// Index of the image open in the editor.
    pub(crate) large: Option<usize>,
}

impl ScanUi {
    /// The image editor window; opened by double-clicking a tile of the strip.
    pub(crate) fn editor_window(&mut self, ctx: &egui::Context) {
        use latent_image_edit_ui::EditorEvent;
        let Some(n) = self.strip.large.filter(|n| *n < self.strip.paths.len()) else {
            self.strip.large = None;
            self.editor.close();
            return;
        };
        let root = self.root();
        self.editor.sync(ctx, &root, &self.strip.paths, n);
        for event in self.editor.show(ctx, &mut self.presets) {
            match event {
                EditorEvent::Selected(i) => {
                    self.strip.selected = Some(i);
                    self.strip.large = Some(i);
                }
                EditorEvent::Closed => self.strip.large = None,
                EditorEvent::FileChanged { path, image } => {
                    // The file changed: forget its thumbnails and decode it again.
                    if let Some(t) = &mut self.thumbs {
                        t.forget(&path);
                    }
                    // Best effort: the strip shows the new thumbnail without decoding the big file.
                    if let Some(img) = image {
                        let cache = self.thumb_cache_dir();
                        crate::thumbs::write_cache(&cache, &path, crate::worker::STRIP_THUMB, &img);
                    }
                    self.strip.dirty = true;
                }
            }
        }
    }

    /// The strip: header with the row and count, then a sideways scrolling row of tiles.
    pub(crate) fn strip_ui(&mut self, ui: &mut Ui) {
        let key = match self.mode {
            SaveMode::Folder => Some(StripKey::Folder(self.folder.clone())),
            SaveMode::Archive => self
                .targets
                .get(self.table)
                .zip(self.row)
                .map(|(t, r)| StripKey::Row(t.name.clone(), r)),
        };
        self.reload_strip(&key);
        let root = self.root();

        ui.horizontal(|ui| {
            ui.label(RichText::new("Images").strong());
            let count = self.strip.paths.len();
            match &key {
                Some(StripKey::Folder(dir)) => {
                    ui.weak(format!("{} · {count} image(s)", dir.display()));
                }
                Some(StripKey::Row(table, row)) => {
                    ui.weak(format!("{table} {row} · {count} image(s)"));
                }
                None => {
                    ui.weak("choose an archive and a film under Save to");
                }
            }
            if ui.button("⟳").on_hover_text("Reload the list").clicked() {
                self.strip.dirty = true;
            }
        });

        let size = (ui.available_height() - ui.spacing().scroll.allocated_width() - 4.0)
            .clamp(48.0, 400.0);
        let paths = self.strip.paths.clone();
        let (mut select, mut open_large) = (None, None);
        egui::ScrollArea::horizontal()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // The wheel scrolls sideways over the strip.
                if ui.rect_contains_pointer(ui.clip_rect()) {
                    let d = ui.ctx().input(|i| i.smooth_scroll_delta);
                    ui.scroll_with_delta(egui::vec2(d.y, 0.0));
                }
                ui.horizontal_top(|ui| {
                    for (n, rel) in paths.iter().enumerate() {
                        let full = root.join(rel);
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(size * 1.3, size),
                            egui::Sense::click(),
                        );
                        if ui.is_rect_visible(rect) {
                            if let Some(thumbs) = &mut self.thumbs {
                                paint_tile(ui, rect, thumbs.get(&full, STRIP_THUMB));
                            }
                            label(ui, rect, rel);
                        }
                        if self.strip.selected == Some(n) {
                            ui.painter().rect_stroke(
                                rect,
                                2.0,
                                Stroke::new(2.0_f32, ACCENT),
                                egui::StrokeKind::Inside,
                            );
                        } else if resp.hovered() {
                            ui.painter().rect_stroke(
                                rect,
                                2.0,
                                Stroke::new(1.0_f32, Color32::from_white_alpha(120)),
                                egui::StrokeKind::Inside,
                            );
                        }
                        if resp.clicked() {
                            select = Some(n);
                        }
                        if resp.double_clicked() {
                            open_large = Some(n);
                        }
                        resp.on_hover_text(rel).context_menu(|ui| {
                            if ui.button("Open large").clicked() {
                                open_large = Some(n);
                                ui.close_menu();
                            }
                            if ui.button("Copy path").clicked() {
                                ui.ctx().copy_text(full.display().to_string());
                                ui.close_menu();
                            }
                        });
                    }
                });
            });
        if select.is_some() {
            self.strip.selected = select;
        }
        if open_large.is_some() {
            self.strip.selected = open_large;
            self.strip.large = open_large;
        }
    }

    /// Reads the row's image list again when the target changed or a scan was saved.
    fn reload_strip(&mut self, key: &Option<StripKey>) {
        if !self.strip.dirty && &self.strip.key == key {
            return;
        }
        if &self.strip.key != key {
            self.strip.selected = None;
            self.strip.large = None;
        }
        self.strip.paths = match key {
            Some(StripKey::Folder(dir)) => targets::folder_images(dir),
            Some(StripKey::Row(table, row)) => targets::row_images(&self.root(), table, *row),
            None => vec![],
        };
        self.strip.key = key.clone();
        self.strip.dirty = false;
    }
}

fn paint_tile(ui: &mut Ui, rect: Rect, thumb: &Thumb) {
    match thumb {
        Thumb::Ready(tex) => {
            let uv = fill_uv(tex.size_vec2(), rect.size());
            ui.painter().image(tex.id(), rect, uv, Color32::WHITE);
        }
        Thumb::Loading => {
            ui.painter().rect_filled(rect, 2.0, Color32::from_gray(40));
            ui.put(
                Rect::from_center_size(rect.center(), egui::vec2(24.0, 24.0)),
                egui::Spinner::new(),
            );
        }
        Thumb::Missing => {
            ui.painter().rect_filled(rect, 2.0, Color32::from_gray(40));
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "missing",
                egui::FontId::proportional(12.0),
                Color32::GRAY,
            );
        }
    }
}

/// File name (without extension) in the tile's bottom-left corner.
fn label(ui: &Ui, rect: Rect, rel: &str) {
    let name = std::path::Path::new(rel)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let galley = ui.painter().layout_no_wrap(
        name,
        egui::FontId::proportional(12.0),
        Color32::from_gray(230),
    );
    let pos = rect.left_bottom() + egui::vec2(4.0, -galley.size().y - 4.0);
    let pill = Rect::from_min_size(pos, galley.size()).expand2(egui::vec2(3.0, 1.0));
    ui.painter()
        .rect_filled(pill, 3.0, Color32::from_black_alpha(160));
    ui.painter().galley(pos, galley, Color32::WHITE);
}

/// The part of a texture (as UV) that fills a rectangle without distortion: a centred crop.
pub fn fill_uv(image: Vec2, target: Vec2) -> Rect {
    let (ia, ta) = (image.x / image.y.max(1.0), target.x / target.y.max(1.0));
    let (w, h) = if ia > ta {
        (ta / ia, 1.0)
    } else {
        (1.0, ia / ta)
    };
    Rect::from_min_max(
        egui::pos2(0.5 - w / 2.0, 0.5 - h / 2.0),
        egui::pos2(0.5 + w / 2.0, 0.5 + h / 2.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_aspect_uses_the_whole_texture() {
        let uv = fill_uv(egui::vec2(300.0, 200.0), egui::vec2(150.0, 100.0));
        assert_eq!((uv.width(), uv.height()), (1.0, 1.0));
    }

    #[test]
    fn a_wide_texture_is_cropped_at_the_sides_and_a_tall_one_top_and_bottom() {
        let wide = fill_uv(egui::vec2(400.0, 100.0), egui::vec2(100.0, 100.0));
        assert!((wide.width() - 0.25).abs() < 1e-6 && wide.height() == 1.0);
        assert!((wide.center().x - 0.5).abs() < 1e-6, "centred");
        let tall = fill_uv(egui::vec2(100.0, 400.0), egui::vec2(100.0, 100.0));
        assert!((tall.height() - 0.25).abs() < 1e-6 && tall.width() == 1.0);
    }

    #[test]
    fn the_list_is_reloaded_when_the_target_changes_or_after_a_save() {
        let dir = std::env::temp_dir().join(format!("scan-strip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(latent_scan_archive::DATA_FILE),
            r#"{"tables":{"roll_film":{"rows":[{"id":1,"images":["a.png"]},{"id":2}]}}}"#,
        )
        .unwrap();
        let mut ui = ScanUi::new(dir.clone());
        let one = Some(StripKey::Row("roll_film".to_string(), 1));
        ui.reload_strip(&one);
        assert_eq!(ui.strip.paths, ["a.png"]);
        ui.strip.selected = Some(0);

        // Same target, nothing dirty: no reload, selection kept.
        ui.reload_strip(&one);
        assert_eq!(ui.strip.selected, Some(0));

        // A save marks the list dirty; the new file shows up and the selection stays.
        std::fs::write(
            dir.join(latent_scan_archive::DATA_FILE),
            r#"{"tables":{"roll_film":{"rows":[{"id":1,"images":["a.png","b.png"]},{"id":2}]}}}"#,
        )
        .unwrap();
        ui.strip.dirty = true;
        ui.reload_strip(&one);
        assert_eq!(ui.strip.paths, ["a.png", "b.png"]);
        assert_eq!(ui.strip.selected, Some(0));

        // Another row: the selection is dropped and the list follows.
        ui.reload_strip(&Some(StripKey::Row("roll_film".to_string(), 2)));
        assert!(ui.strip.paths.is_empty() && ui.strip.selected.is_none());
        ui.reload_strip(&None);
        assert!(ui.strip.paths.is_empty());
        std::fs::remove_dir_all(dir).ok();
    }
}
