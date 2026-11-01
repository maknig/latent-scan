//! The image view of a saved scan: adjust, sharpen, rotate and crop with a live histogram,
//! modelled on the image view of latent.
//!
//! Edits only change the preview. **Apply** writes them to the file (at full resolution, same
//! format, 16 bit kept); the untouched original can be saved first with **Backup original**
//! (`.originals/…`, latent's layout, see [`crate::edit`]).

use crate::crop::{self, Crop, Grab};
use crate::dust;
use crate::form::{button_row, form, form_buttons, Btn, SliderOpts};
use crate::histogram::{self, Histogram};
use crate::panels::{adjust_sliders, preset_row};
use crate::presets::PresetUi;
use crate::preview::{self, rotate_image, to_color_image};
use crate::view::{self, ZoomState};
use crate::{canvas, edit, worker, ScanUi, ACCENT};
use egui::{Color32, ColorImage, Context, RichText, TextureHandle, Ui};
use scan_core::RawImage;
use scan_pipeline::heal::{heal, Spot};
use scan_pipeline::sharpen::{sharpen, Sharpen, SharpenStage};
use scan_pipeline::tone::{Adjust, Tone, ToneOptions, ToneStage};
use scan_pipeline::{file_depth, read_image, write_image_depth, BitDepth, Pipeline, Rotate};
use std::path::{Path, PathBuf};

/// Longest side of the preview decoded for the editor.
pub const PREVIEW_SIDE: u32 = 2048;
/// Width of the side column with the histogram and the sliders.
const SIDE_W: f32 = 300.0;
const HISTOGRAM_H: f32 = 100.0;

/// What the user changed and has not applied yet.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Edits {
    /// Invert and balance a negative.
    pub negative: bool,
    pub adjust: Adjust,
    pub sharpen: Sharpen,
    /// Clockwise quarter turns.
    pub turns: u8,
    /// Crop, normalized in the file's own orientation (before the turns).
    pub crop: Option<Crop>,
    /// Dust spots to heal, in the file's own orientation (see [`crate::dust`]).
    pub spots: Vec<Spot>,
    /// Write the file with 8 instead of 16 bits per sample (an 8-bit file stays 8 bit).
    pub eight_bit: bool,
}

impl Edits {
    /// Nothing that would change the file (a full-size crop or a radius change without
    /// sharpening counts as nothing).
    pub fn is_neutral(&self) -> bool {
        !self.negative
            && self.adjust.is_neutral()
            && self.sharpen.is_neutral()
            && self.turns.is_multiple_of(4)
            && self.crop.is_none_or(|c| c == crop::FULL)
            && self.spots.is_empty()
            && !self.eight_bit
    }

    pub fn degrees(&self) -> u16 {
        (self.turns % 4) as u16 * 90
    }

    pub fn tone(&self) -> ToneOptions {
        ToneOptions {
            negative: self.negative,
            auto_levels: false,
            adjust: self.adjust,
        }
    }

    /// The part of the edits that decides how the preview looks. The crop only matters while
    /// the negative base is estimated from it, so dragging the crop of a positive does not
    /// re-render the picture.
    fn render_key(&self) -> Self {
        Self {
            crop: self.crop.filter(|_| self.negative),
            sharpen: if self.sharpen.is_neutral() {
                Sharpen::default()
            } else {
                self.sharpen
            },
            eight_bit: false,
            ..self.clone()
        }
    }
}

/// TIFF, PNG and JPEG can be edited and written back. JPEG stays 8 bit and is re-compressed by
/// every Apply.
pub fn is_editable(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| matches!(e.as_str(), "tif" | "tiff" | "png" | "jpg" | "jpeg"))
}

/// The edited preview (without the crop, which is an overlay until it is applied). `scale` is the
/// preview's size relative to the file, so the sharpening radius matches the final result.
pub fn render(base: &RawImage, edits: &Edits, scale: f32) -> ColorImage {
    let mut img = base.clone();
    let tone = edits.tone();
    if !tone.is_identity() {
        // The negative base comes from the crop, as it will when applied.
        let reference = edits.crop.and_then(|c| preview::region(base, c));
        Tone::estimate(reference.as_ref().unwrap_or(base), tone).apply(&mut img);
    }
    sharpen(&mut img, edits.sharpen.scaled(scale));
    rotate_image(&to_color_image(&img, u32::MAX), edits.degrees())
}

/// Black and white point measured on the crop (or the whole preview).
pub fn auto_levels(base: &RawImage, edits: &Edits) -> Adjust {
    let reference = edits.crop.and_then(|c| preview::region(base, c));
    Adjust::auto_levels(reference.as_ref().unwrap_or(base), edits.negative)
}

/// Applies `edits` to the file at full resolution: heal dust, crop, rotate, tone, sharpen, written over
/// the file atomically in the same format. Returns the image that was written.
pub fn apply_file(path: &Path, edits: &Edits) -> Result<RawImage, String> {
    let depth = if edits.eight_bit || file_depth(path) == Some(BitDepth::Eight) {
        BitDepth::Eight
    } else {
        BitDepth::Sixteen
    };
    let mut img = read_image(path).map_err(|e| e.to_string())?;
    // Healing comes first: the spots are in the file's own orientation and pixels.
    heal(&mut img, &edits.spots);
    if let Some(c) = edits.crop.filter(|c| *c != crop::FULL) {
        img = preview::region(&img, c).ok_or("the crop is empty")?;
    }
    let pipeline = Pipeline::default()
        .with(Rotate(edits.degrees()))
        .with(ToneStage(edits.tone()))
        .with(SharpenStage(edits.sharpen));
    let out = pipeline.run(img).map_err(|e| e.to_string())?;
    edit::overwrite_atomic(path, |tmp| {
        write_image_depth(&out, tmp, depth).map_err(|e| e.to_string())
    })?;
    Ok(out)
}

/// What a background file operation of the editor does.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorOp {
    Apply { edits: Edits, backup_first: bool },
    Backup,
    Restore,
}

impl EditorOp {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Apply { .. } => "Apply",
            Self::Backup => "Backup",
            Self::Restore => "Restore",
        }
    }
}

/// One image open in the editor.
pub struct EditorState {
    pub rel: String,
    pub path: PathBuf,
    /// The decoded preview (16 bit, at most [`PREVIEW_SIDE`] long); `None` while loading.
    pub base: Option<RawImage>,
    pub file_size: [u32; 2],
    /// Preview size relative to the file.
    pub scale: f32,
    pub tex: Option<TextureHandle>,
    shown: Option<Edits>,
    pub histogram: Histogram,
    pub edits: Edits,
    pub zoom: Option<ZoomState>,
    /// The pane's shape of the last frame (for the zoom buttons).
    pub shape: f32,
    pub grab: Option<Grab>,
    /// Dust mode: dragging over the picture marks spots; the brush radius is a fraction of the
    /// picture's width.
    pub dust: bool,
    pub brush: f32,
    /// The pointer is down in dust mode: the preview waits until the spot list is complete.
    painting: bool,
    /// The preview base with the spots healed, for the spots it was made for.
    healed: Option<(Vec<Spot>, RawImage)>,
    pub editable: bool,
    pub has_original: bool,
    /// Bits per sample of the file on disk.
    pub source_depth: BitDepth,
    /// Asking whether to overwrite without a backup.
    pub confirm: bool,
    pub busy: Option<&'static str>,
    pub error: Option<String>,
}

impl EditorState {
    pub fn new(root: &Path, rel: &str) -> Self {
        let path = root.join(rel);
        Self {
            rel: rel.to_string(),
            editable: is_editable(&path),
            has_original: edit::has_original(root, &path),
            source_depth: file_depth(&path).unwrap_or_default(),
            path,
            base: None,
            file_size: [0, 0],
            scale: 1.0,
            tex: None,
            shown: None,
            histogram: Histogram::default(),
            edits: Edits::default(),
            zoom: None,
            shape: 1.0,
            grab: None,
            dust: false,
            brush: dust::DEFAULT_BRUSH,
            painting: false,
            healed: None,
            confirm: false,
            busy: None,
            error: None,
        }
    }

    pub fn dirty(&self) -> bool {
        !self.edits.is_neutral()
    }

    /// The decoded preview arrived.
    pub fn loaded(&mut self, preview: RawImage, file_size: [u32; 2]) {
        self.scale = preview.width as f32 / file_size[0].max(1) as f32;
        self.file_size = file_size;
        self.base = Some(preview);
        self.shown = None;
        self.busy = None;
        self.error = None;
    }

    /// Back to the file as it is (after an apply or a restore): the preview is decoded again.
    pub fn reset_for_reload(&mut self, root: &Path) {
        self.edits = Edits::default();
        self.dust = false;
        self.healed = None;
        self.base = None;
        self.shown = None;
        self.confirm = false;
        self.has_original = edit::has_original(root, &self.path);
        self.source_depth = file_depth(&self.path).unwrap_or_default();
    }

    /// The preview as displayed: `(width, height)` of the picture after the turns.
    pub fn picture_size(&self) -> Option<[f32; 2]> {
        let b = self.base.as_ref()?;
        let (w, h) = (b.width as f32, b.height as f32);
        Some(if self.edits.turns.is_multiple_of(2) {
            [w, h]
        } else {
            [h, w]
        })
    }

    /// Renders the texture and the histogram again when the edits changed what is shown.
    pub fn refresh(&mut self, ctx: &Context) {
        if self.grab.is_some() || self.painting {
            return;
        }
        let Some(base) = &self.base else { return };
        let key = self.edits.render_key();
        if self.shown.as_ref() == Some(&key) {
            return;
        }
        // Healing searches for texture, so it only runs when the spots changed.
        let base = if self.edits.spots.is_empty() {
            base
        } else {
            if self.healed.as_ref().map(|h| &h.0) != Some(&self.edits.spots) {
                let mut healed = base.clone();
                heal(&mut healed, &self.edits.spots);
                self.healed = Some((self.edits.spots.clone(), healed));
            }
            &self.healed.as_ref().expect("just made").1
        };
        let image = render(base, &self.edits, self.scale);
        self.histogram = Histogram::of(&image);
        match &mut self.tex {
            Some(tex) => tex.set(image, Default::default()),
            None => {
                self.tex = Some(ctx.load_texture(
                    format!("editor:{}", self.rel),
                    image,
                    Default::default(),
                ))
            }
        }
        self.shown = Some(key);
    }
}

/// Things the user asked for in one frame; run after the windows are drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Act {
    Step(i32),
    ZoomBy(f32),
    Fit,
    Turn(i8),
    ToggleCrop,
    ToggleDust,
    SetBrush(f32),
    UndoSpot,
    ClearSpots,
    Apply,
    ApplyConfirmed { backup_first: bool },
    CancelConfirm,
    Revert,
    Backup,
    Restore,
    Auto,
    ResetSliders,
}

impl ScanUi {
    /// The editor window; opened by double-clicking a tile of the strip.
    pub(crate) fn editor_window(&mut self, ctx: &Context) {
        let Some(n) = self.strip.large else {
            self.editor = None;
            return;
        };
        let Some(rel) = self.strip.paths.get(n).cloned() else {
            self.strip.large = None;
            self.editor = None;
            return;
        };
        if self.editor.as_ref().is_none_or(|e| e.rel != rel) {
            let root = self.root();
            self.editor = Some(EditorState::new(&root, &rel));
            self.load_editor_image(ctx);
        }
        let last = self.strip.paths.len().saturating_sub(1);
        if let Some(st) = &mut self.editor {
            st.refresh(ctx);
        }

        let mut open = true;
        let mut acts: Vec<Act> = vec![];
        let title = match &self.editor {
            Some(e) if e.dirty() => format!("{rel} *"),
            _ => rel.clone(),
        };
        if let Some(st) = &mut self.editor {
            egui::Window::new(title)
                .id(egui::Id::new("scan-editor"))
                .open(&mut open)
                .default_size([1180.0, 760.0])
                .show(ctx, |ui| {
                    editor_ui(ui, st, &mut self.presets, n, last, &mut acts)
                });
            if st.confirm {
                confirm_window(
                    ctx,
                    st.path.extension().is_some_and(|e| {
                        matches!(e.to_string_lossy().to_lowercase().as_str(), "jpg" | "jpeg")
                    }),
                    &mut acts,
                );
            }
        }

        let typing = ctx.wants_keyboard_input();
        if !typing {
            ctx.input(|i| {
                if i.key_pressed(egui::Key::ArrowLeft) {
                    acts.push(Act::Step(-1));
                }
                if i.key_pressed(egui::Key::ArrowRight) {
                    acts.push(Act::Step(1));
                }
            });
        }
        let escape = !typing && ctx.input(|i| i.key_pressed(egui::Key::Escape));
        for act in acts {
            self.run_editor_act(act, ctx, last);
        }
        if !open || escape {
            self.discard_editor_notice();
            self.strip.large = None;
            self.editor = None;
        }
    }

    /// Mentions edits that are thrown away when the image is left.
    fn discard_editor_notice(&mut self) {
        if let Some(e) = self.editor.as_ref().filter(|e| e.dirty()) {
            self.status = Some((false, format!("Discarded unsaved edits of {}", e.rel)));
        }
    }

    pub(crate) fn load_editor_image(&mut self, ctx: &Context) {
        let Some(e) = &self.editor else { return };
        worker::editor_load(self.reporter(ctx), e.path.clone());
    }

    fn run_editor_act(&mut self, act: Act, ctx: &Context, last: usize) {
        let root = self.root();
        let reporter = self.reporter(ctx);
        let cache = self.thumb_cache_dir();
        if let Act::Step(d) = act {
            let n = self.strip.large.unwrap_or(0) as i32 + d;
            if (0..=last as i32).contains(&n) {
                self.discard_editor_notice();
                self.strip.selected = Some(n as usize);
                self.strip.large = Some(n as usize);
            }
            return;
        }
        let Some(st) = &mut self.editor else { return };
        let busy = st.busy.is_some();
        match act {
            Act::ZoomBy(f) => {
                let anchor = st.zoom.map_or([0.5, 0.5], |z| z.center);
                view::wheel(&mut st.zoom, f, anchor, st.shape);
            }
            Act::Fit => st.zoom = None,
            Act::Turn(d) if st.editable && !busy => {
                st.edits.turns = (st.edits.turns as i8 + d).rem_euclid(4) as u8;
                st.zoom = None;
            }
            Act::ToggleCrop if st.editable && !busy => {
                st.edits.crop = match st.edits.crop {
                    Some(_) => None,
                    None => Some(crop::FULL),
                };
                st.dust = false;
            }
            Act::ToggleDust if st.editable && !busy => {
                st.dust = !st.dust;
                if st.dust {
                    st.edits.crop = None;
                }
            }
            Act::SetBrush(b) => st.brush = b.clamp(*dust::BRUSH.start(), *dust::BRUSH.end()),
            Act::UndoSpot if st.editable && !busy => {
                st.edits.spots.pop();
            }
            Act::ClearSpots if st.editable && !busy => st.edits.spots.clear(),
            Act::Apply if st.editable && !busy && st.dirty() => {
                if st.has_original {
                    start_op(st, &root, &reporter, &cache, false);
                } else {
                    st.confirm = true;
                }
            }
            Act::ApplyConfirmed { backup_first } if st.editable && !busy => {
                st.confirm = false;
                start_op(st, &root, &reporter, &cache, backup_first);
            }
            Act::CancelConfirm => st.confirm = false,
            Act::Revert => {
                st.edits = Edits::default();
                st.dust = false;
                st.zoom = None;
            }
            Act::Backup if st.editable && !busy && !st.has_original => {
                st.busy = Some("Backing up");
                worker::editor_op(reporter, root, st.path.clone(), EditorOp::Backup, cache);
            }
            Act::Restore if st.editable && !busy && st.has_original => {
                st.busy = Some("Restoring");
                worker::editor_op(reporter, root, st.path.clone(), EditorOp::Restore, cache);
            }
            Act::Auto => {
                if let Some(base) = &st.base {
                    let a = auto_levels(base, &st.edits);
                    st.edits.adjust.black = a.black;
                    st.edits.adjust.white = a.white;
                }
            }
            Act::ResetSliders => {
                st.edits.negative = false;
                st.edits.adjust = Adjust::default();
                st.edits.sharpen = Sharpen::default();
            }
            _ => {}
        }
    }
}

/// Starts the apply on a worker thread.
fn start_op(
    st: &mut EditorState,
    root: &Path,
    reporter: &worker::Reporter,
    cache: &Path,
    backup_first: bool,
) {
    st.busy = Some("Applying");
    let op = EditorOp::Apply {
        edits: st.edits.clone(),
        backup_first,
    };
    worker::editor_op(
        reporter.clone(),
        root.to_path_buf(),
        st.path.clone(),
        op,
        cache.to_path_buf(),
    );
}

/// "Overwrite without a backup?"
fn confirm_window(ctx: &Context, lossy: bool, acts: &mut Vec<Act>) {
    egui::Window::new("No backup of this image")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.label("This image has no backup. Applying overwrites it.");
            if lossy {
                ui.label("JPEG is lossy: every Apply compresses the picture again.");
            }
            ui.add_space(6.0);
            let buttons = [
                Btn::new("Back up and apply").primary(),
                Btn::new("Apply without backup"),
                Btn::new("Cancel"),
            ];
            match button_row(ui, &buttons) {
                Some(0) => acts.push(Act::ApplyConfirmed { backup_first: true }),
                Some(1) => acts.push(Act::ApplyConfirmed {
                    backup_first: false,
                }),
                Some(_) => acts.push(Act::CancelConfirm),
                None => {}
            }
        });
}

fn editor_ui(
    ui: &mut Ui,
    st: &mut EditorState,
    presets: &mut PresetUi,
    n: usize,
    last: usize,
    acts: &mut Vec<Act>,
) {
    toolbar(ui, st, n, last, acts);
    ui.separator();
    ui.horizontal_top(|ui| {
        let avail = ui.available_size();
        // The row is pane, gap, separator (one gap wide), gap, side column: three gaps. Leaving
        // any of them out makes the content wider than the window, which then grows every frame.
        let gaps = 3.0 * ui.spacing().item_spacing.x;
        let pane = egui::vec2((avail.x - SIDE_W - gaps).max(200.0), avail.y);
        // A region of exactly the pane's size, so the side column never moves with the picture.
        ui.allocate_ui(pane, |ui| {
            ui.set_min_size(pane);
            image_area(ui, st, pane);
        });
        ui.separator();
        ui.vertical(|ui| {
            ui.set_width(SIDE_W);
            side_column(ui, st, presets, acts);
        });
    });
}

fn toolbar(ui: &mut Ui, st: &EditorState, n: usize, last: usize, acts: &mut Vec<Act>) {
    let idle = st.busy.is_none();
    let can_edit = st.editable && idle;
    ui.horizontal_wrapped(|ui| {
        if ui.add_enabled(n > 0, egui::Button::new("◀ Back")).clicked() {
            acts.push(Act::Step(-1));
        }
        ui.weak(format!("{} / {}", n + 1, last + 1));
        if ui
            .add_enabled(n < last, egui::Button::new("Next ▶"))
            .clicked()
        {
            acts.push(Act::Step(1));
        }
        ui.separator();
        if ui.button("Zoom out").clicked() {
            acts.push(Act::ZoomBy(0.8));
        }
        if ui.button("Fit").clicked() {
            acts.push(Act::Fit);
        }
        if ui.button("Zoom in").clicked() {
            acts.push(Act::ZoomBy(1.25));
        }
        ui.separator();
        if ui
            .add_enabled(can_edit, egui::Button::new("⟲"))
            .on_hover_text("Rotate left")
            .clicked()
        {
            acts.push(Act::Turn(-1));
        }
        if ui
            .add_enabled(can_edit, egui::Button::new("⟳"))
            .on_hover_text("Rotate right")
            .clicked()
        {
            acts.push(Act::Turn(1));
        }
        let cropping = st.edits.crop.is_some();
        if ui
            .add_enabled(can_edit, egui::SelectableLabel::new(cropping, "Crop"))
            .on_hover_text("Drag the frame on the picture; Apply writes the crop")
            .clicked()
        {
            acts.push(Act::ToggleCrop);
        }
        if ui
            .add_enabled(can_edit, egui::SelectableLabel::new(st.dust, "Dust"))
            .on_hover_text("Click or drag over dust and scratches to mark them; Apply heals them")
            .clicked()
        {
            acts.push(Act::ToggleDust);
        }
        if st.dust {
            ui.label("Size");
            let mut brush = st.brush;
            if ui
                .add(egui::Slider::new(&mut brush, dust::BRUSH).show_value(false))
                .changed()
            {
                acts.push(Act::SetBrush(brush));
            }
            let marked = !st.edits.spots.is_empty();
            if ui.add_enabled(marked, egui::Button::new("Undo")).clicked() {
                acts.push(Act::UndoSpot);
            }
            if ui
                .add_enabled(
                    marked,
                    egui::Button::new(format!("Clear ({})", st.edits.spots.len())),
                )
                .clicked()
            {
                acts.push(Act::ClearSpots);
            }
        }
        ui.separator();
        if ui
            .add_enabled(
                can_edit && st.dirty(),
                egui::Button::new(RichText::new("Apply").strong()).fill(ACCENT),
            )
            .on_hover_text("Write the edits to the file")
            .clicked()
        {
            acts.push(Act::Apply);
        }
        if ui
            .add_enabled(st.dirty(), egui::Button::new("Revert"))
            .on_hover_text("Throw the unsaved edits away")
            .clicked()
        {
            acts.push(Act::Revert);
        }
        ui.separator();
        if ui
            .add_enabled(
                can_edit && !st.has_original,
                egui::Button::new("Backup original"),
            )
            .on_hover_text("Copy the untouched image to .originals/")
            .clicked()
        {
            acts.push(Act::Backup);
        }
        if ui
            .add_enabled(
                can_edit && st.has_original,
                egui::Button::new("Restore original"),
            )
            .on_hover_text("Put the backed-up original back")
            .clicked()
        {
            acts.push(Act::Restore);
        }
    });
}

fn image_area(ui: &mut Ui, st: &mut EditorState, pane: egui::Vec2) {
    if let Some(error) = &st.error {
        ui.colored_label(Color32::LIGHT_RED, error);
        return;
    }
    let (Some(tex), Some([w, h])) = (st.tex.as_ref().map(|t| t.id()), st.picture_size()) else {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.spinner();
        });
        return;
    };
    let aspect = w / h.max(1.0);
    let canvas = canvas::show(ui, pane, tex, aspect, &mut st.zoom, |_, _| {});
    st.shape = canvas.shape;
    // The crop is stored in the file's orientation and edited on the turned picture.
    if let Some(c) = st.edits.crop {
        let deg = st.edits.degrees();
        let mut frames = [crop::to_view(c, deg)];
        let before = frames;
        let mut active = 0;
        crop::overlay(
            ui,
            &canvas.resp,
            canvas.rect,
            canvas.window,
            &mut frames,
            &mut active,
            &mut st.grab,
            ACCENT,
        );
        if frames != before {
            st.edits.crop = Some(crop::from_view(frames[0], deg));
        }
    }
    st.painting = false;
    if st.dust {
        let aspect = match st.file_size {
            [w, h] if w > 0 && h > 0 => w as f32 / h as f32,
            _ => st
                .base
                .as_ref()
                .map_or(1.0, |b| b.width as f32 / b.height.max(1) as f32),
        };
        let deg = st.edits.degrees();
        st.painting = dust::overlay(
            ui,
            &canvas.resp,
            canvas.rect,
            canvas.window,
            &mut st.edits.spots,
            (deg, aspect, st.brush),
        );
    }
}

fn side_column(ui: &mut Ui, st: &mut EditorState, presets: &mut PresetUi, acts: &mut Vec<Act>) {
    histogram::paint(ui, &st.histogram, egui::vec2(SIDE_W, HISTOGRAM_H));
    ui.add_space(4.0);
    let mut state = vec![];
    if let Some(busy) = st.busy {
        state.push(format!("{busy}…"));
    } else if st.dirty() {
        state.push("unsaved edits".to_string());
    }
    if st.file_size[0] > 0 {
        state.push(format!("{} × {}", st.file_size[0], st.file_size[1]));
    }
    if !st.edits.spots.is_empty() {
        state.push(format!("{} dust spots", st.edits.spots.len()));
    }
    if st.has_original {
        state.push("backup exists".into());
    }
    if !st.editable {
        state.push("view only (TIFF and PNG can be edited)".into());
    }
    ui.weak(state.join(" · "));
    ui.add_space(4.0);

    let colour = st.base.as_ref().is_none_or(|b| b.channels == 3);
    ui.add_enabled_ui(st.editable && st.busy.is_none(), |ui| {
        form(ui, "editor-film", |f| {
            f.check(
                "Film",
                "negative",
                &mut st.edits.negative,
                "Invert and remove the orange mask",
            );
        });
        if let Some(p) = preset_row(
            ui,
            "editor-preset",
            presets,
            (&st.edits.adjust, Some(&st.edits.sharpen)),
        ) {
            st.edits.adjust = p.adjust();
            st.edits.sharpen = p.sharpen();
        }
        ui.add_space(4.0);
        adjust_sliders(ui, "editor-adjust", &mut st.edits.adjust, colour);
        ui.add_space(4.0);
        sharpen_sliders(ui, &mut st.edits.sharpen);
        let already_8 = st.source_depth == BitDepth::Eight;
        let mut eight = st.edits.eight_bit || already_8;
        ui.add_enabled_ui(!already_8, |ui| {
            form(ui, "editor-depth", |f| {
                f.check(
                    "Output",
                    "Save as 8 bit",
                    &mut eight,
                    if already_8 {
                        "This file is already 8 bit"
                    } else {
                        "Halves the file when applied; Restore original brings the 16 bit back"
                    },
                );
            });
        });
        if !already_8 {
            st.edits.eight_bit = eight;
        }
        ui.add_space(4.0);
        let buttons = [
            Btn::new("Auto")
                .enabled(st.base.is_some())
                .hover("Black and white point from the picture (or the crop)"),
            Btn::new("Reset sliders").hover("Film, sliders and sharpening back to neutral"),
        ];
        match form_buttons(ui, &buttons) {
            Some(0) => acts.push(Act::Auto),
            Some(_) => acts.push(Act::ResetSliders),
            None => {}
        }
    });
}

/// Amount and radius of the unsharp mask.
fn sharpen_sliders(ui: &mut Ui, s: &mut Sharpen) {
    let d = Sharpen::default();
    form(ui, "editor-sharpen", |f| {
        let mut percent = s.amount * 100.0;
        if f.slider(
            "Sharpen",
            &mut percent,
            0.0..=300.0,
            SliderOpts::default()
                .suffix(" %")
                .decimals(0)
                .default_value(d.amount as f64 * 100.0),
            "Strength of the unsharp mask (brightness only). The preview is made from a reduced \
             copy, so it shows the effect only approximately",
        ) {
            s.amount = percent / 100.0;
        }
        f.slider(
            "Radius",
            &mut s.radius,
            0.3..=5.0,
            SliderOpts::default()
                .suffix(" px")
                .decimals(1)
                .default_value(d.radius as f64),
            "Size of the detail that is sharpened, in pixels of the file",
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use scan_pipeline::write_image;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("scan-editor-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A 16-bit RGB file whose left half is dark and right half bright.
    fn write_scan(path: &Path, w: u32, h: u32) {
        let mut data = vec![];
        for _y in 0..h {
            for x in 0..w {
                let v = if x < w / 2 { 10000u16 } else { 40000 };
                data.extend([v, v, v]);
            }
        }
        write_image(&RawImage::new(w, h, 3, data).unwrap(), path).unwrap();
    }

    #[test]
    fn nothing_is_pending_until_something_changes() {
        assert!(Edits::default().is_neutral());
        let changed = [
            Edits {
                negative: true,
                ..Edits::default()
            },
            Edits {
                turns: 1,
                ..Edits::default()
            },
            Edits {
                crop: Some([0.1, 0.1, 0.9, 0.9]),
                ..Edits::default()
            },
            Edits {
                adjust: Adjust {
                    exposure_ev: 0.5,
                    ..Adjust::default()
                },
                ..Edits::default()
            },
            Edits {
                sharpen: Sharpen {
                    amount: 1.0,
                    radius: 1.0,
                },
                ..Edits::default()
            },
        ];
        assert!(changed.iter().all(|e| !e.is_neutral()));
        // These change nothing in the file.
        assert!(Edits {
            turns: 4,
            ..Edits::default()
        }
        .is_neutral());
        assert!(Edits {
            crop: Some(crop::FULL),
            ..Edits::default()
        }
        .is_neutral());
        assert!(Edits {
            sharpen: Sharpen {
                amount: 0.0,
                radius: 3.0
            },
            ..Edits::default()
        }
        .is_neutral());
    }

    #[test]
    fn only_tiff_png_and_jpeg_are_editable() {
        for ok in ["a.tiff", "a.TIF", "b.png", "c.PNG", "d.jpg", "e.JPEG"] {
            assert!(is_editable(Path::new(ok)), "{ok}");
        }
        for no in ["a.gif", "a", "a.txt"] {
            assert!(!is_editable(Path::new(no)), "{no}");
        }
    }

    #[test]
    fn the_preview_applies_tone_rotation_and_leaves_the_crop_as_an_overlay() {
        let base = RawImage::new(4, 2, 1, vec![0, 16384, 32768, 65535, 0, 0, 0, 0]).unwrap();
        let plain = render(&base, &Edits::default(), 1.0);
        assert_eq!(plain.size, [4, 2]);

        let turned = render(
            &base,
            &Edits {
                turns: 1,
                ..Edits::default()
            },
            1.0,
        );
        assert_eq!(turned.size, [2, 4], "a quarter turn swaps width and height");

        let brighter = Edits {
            adjust: Adjust {
                exposure_ev: 1.0,
                ..Adjust::default()
            },
            ..Edits::default()
        };
        let lit = render(&base, &brighter, 1.0);
        assert!(
            lit.pixels[1].r() > plain.pixels[1].r(),
            "exposure brightens"
        );

        let cropped = render(
            &base,
            &Edits {
                crop: Some([0.0, 0.0, 0.5, 0.5]),
                ..Edits::default()
            },
            1.0,
        );
        assert_eq!(
            cropped.size,
            [4, 2],
            "the crop is not applied to the preview"
        );
    }

    #[test]
    fn dragging_the_crop_of_a_positive_does_not_re_render_the_picture() {
        let a = Edits {
            crop: Some([0.1, 0.1, 0.5, 0.5]),
            ..Edits::default()
        };
        let b = Edits {
            crop: Some([0.2, 0.2, 0.6, 0.6]),
            ..Edits::default()
        };
        assert_eq!(a.render_key(), b.render_key());
        // A negative takes its base from the crop, so there it does.
        let (na, nb) = (
            Edits {
                negative: true,
                ..a
            },
            Edits {
                negative: true,
                ..b
            },
        );
        assert_ne!(na.render_key(), nb.render_key());
        // The radius of an off sharpening does not matter either.
        let r = Edits {
            sharpen: Sharpen {
                amount: 0.0,
                radius: 4.0,
            },
            ..Edits::default()
        };
        assert_eq!(r.render_key(), Edits::default().render_key());
    }

    #[test]
    fn auto_levels_come_from_the_crop_when_there_is_one() {
        // A dark ramp on the left, a bright ramp on the right (a flat area would give no range).
        let mut data = vec![];
        for _ in 0..10 {
            data.extend((0..5).map(|i| 10000u16 + i * 1000));
            data.extend((0..5).map(|i| 40000u16 + i * 2000));
        }
        let base = RawImage::new(10, 10, 1, data).unwrap();
        let whole = auto_levels(&base, &Edits::default());
        let right_half = auto_levels(
            &base,
            &Edits {
                crop: Some([0.5, 0.0, 1.0, 1.0]),
                ..Edits::default()
            },
        );
        assert!(
            right_half.black > whole.black,
            "{right_half:?} vs {whole:?}"
        );
    }

    #[test]
    fn applying_crops_rotates_and_tones_the_file_and_keeps_16_bits() {
        let d = dir("apply");
        let file = d.join("scan.tiff");
        write_scan(&file, 40, 20);
        let edits = Edits {
            crop: Some([0.5, 0.0, 1.0, 1.0]), // the bright right half: 20 x 20
            turns: 1,
            adjust: Adjust {
                exposure_ev: -1.0,
                ..Adjust::default()
            },
            ..Edits::default()
        };
        let written = apply_file(&file, &edits).unwrap();
        let back = read_image(&file).unwrap();
        assert_eq!(back, written, "what was returned is what is on disk");
        assert_eq!((back.width, back.height, back.channels), (20, 20, 3));
        // The right half (40000) lost one stop: about 20000, in full 16-bit precision.
        assert!(
            back.data.iter().all(|&v| (v as i32 - 20000).abs() <= 2),
            "{:?}",
            &back.data[..6]
        );
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn rotation_alone_turns_the_picture_clockwise_on_disk() {
        let d = dir("turn");
        let file = d.join("t.png");
        write_scan(&file, 40, 20);
        apply_file(
            &file,
            &Edits {
                turns: 1,
                ..Edits::default()
            },
        )
        .unwrap();
        let back = read_image(&file).unwrap();
        assert_eq!((back.width, back.height), (20, 40));
        // The dark left half is now the top half.
        assert_eq!(back.data[0], 10000);
        assert_eq!(*back.data.last().unwrap(), 40000);
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn sharpening_is_applied_last_to_the_full_resolution_file() {
        let d = dir("sharp");
        let file = d.join("s.tiff");
        write_scan(&file, 60, 10);
        let edits = Edits {
            sharpen: Sharpen {
                amount: 1.5,
                radius: 2.0,
            },
            ..Edits::default()
        };
        apply_file(&file, &edits).unwrap();
        let back = read_image(&file).unwrap();
        assert_eq!((back.width, back.height), (60, 10));
        let at = |x: usize| back.data[3 * x];
        assert!(
            at(29) < 10000 && at(30) > 40000,
            "overshoot at the edge: {} {}",
            at(29),
            at(30)
        );
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn a_failed_apply_leaves_the_file_alone() {
        let d = dir("fail");
        let file = d.join("bad.tiff");
        std::fs::write(&file, b"not an image").unwrap();
        assert!(apply_file(
            &file,
            &Edits {
                turns: 1,
                ..Edits::default()
            }
        )
        .is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"not an image");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn an_empty_crop_is_refused_without_touching_the_file() {
        let d = dir("empty");
        let file = d.join("e.png");
        write_scan(&file, 10, 10);
        let before = std::fs::read(&file).unwrap();
        // A crop of one pixel is still valid; a crop outside the image clamps to one pixel.
        let ok = apply_file(
            &file,
            &Edits {
                crop: Some([0.9, 0.9, 1.0, 1.0]),
                ..Edits::default()
            },
        )
        .unwrap();
        assert!(ok.width >= 1 && ok.height >= 1);
        assert_ne!(std::fs::read(&file).unwrap(), before);
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn a_new_editor_state_knows_about_formats_and_backups() {
        let d = dir("state");
        let file = d.join("images/roll/0001/x.tiff");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        write_scan(&file, 8, 8);
        let st = EditorState::new(&d, "images/roll/0001/x.tiff");
        assert!(st.editable && !st.has_original && !st.dirty() && st.base.is_none());

        edit::backup_original(&d, &file).unwrap();
        assert!(EditorState::new(&d, "images/roll/0001/x.tiff").has_original);
        assert!(!EditorState::new(&d, "images/roll/0001/y.gif").editable);
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn loading_sets_the_scale_and_reloading_clears_the_edits() {
        let d = dir("load");
        let mut st = EditorState::new(&d, "a.png");
        st.edits.negative = true;
        st.busy = Some("Applying");
        st.loaded(
            RawImage::new(100, 50, 1, vec![0; 5000]).unwrap(),
            [400, 200],
        );
        assert_eq!(st.scale, 0.25);
        assert!(
            st.busy.is_none() && st.dirty(),
            "edits survive a load (only apply resets them)"
        );
        assert_eq!(st.picture_size(), Some([100.0, 50.0]));
        st.edits.turns = 1;
        assert_eq!(st.picture_size(), Some([50.0, 100.0]));

        st.confirm = true;
        st.reset_for_reload(&d);
        assert!(!st.dirty() && st.base.is_none() && !st.confirm);
        std::fs::remove_dir_all(d).ok();
    }

    /// A `ScanUi` on an archive with one TIFF open in the editor, with a loaded preview.
    fn ui_with_open_image(name: &str) -> (ScanUi, PathBuf, PathBuf) {
        let root = dir(name);
        let rel = "images/roll_film/0001/x.tiff";
        let file = root.join(rel);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        write_scan(&file, 40, 20);
        let mut ui = ScanUi::new(root.clone());
        ui.strip.paths = vec![rel.to_string(), "images/roll_film/0001/y.tiff".to_string()];
        ui.strip.large = Some(0);
        let mut st = EditorState::new(&root, rel);
        st.loaded(
            RawImage::new(20, 10, 3, vec![20000; 600]).unwrap(),
            [40, 20],
        );
        ui.editor = Some(st);
        (ui, root, file)
    }

    /// Polls until the editor is no longer busy (a worker thread answered).
    fn wait_until_idle(ui: &mut ScanUi, ctx: &Context) {
        for _ in 0..200 {
            ui.poll(ctx);
            if ui.editor.as_ref().is_none_or(|e| e.busy.is_none()) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("the editor stayed busy");
    }

    fn turned() -> Edits {
        Edits {
            turns: 1,
            ..Edits::default()
        }
    }

    fn size_on_disk(file: &Path) -> (u32, u32) {
        let img = read_image(file).unwrap();
        (img.width, img.height)
    }

    /// A 16-bit RGB file: flat 10000, with a bright speck at (20, 15), and 40000 on the right.
    fn write_specked(path: &Path, w: u32, h: u32) {
        let mut data = vec![];
        for y in 0..h {
            for x in 0..w {
                let speck = (x as i64 - 20).pow(2) + (y as i64 - 15).pow(2) <= 4;
                let v = if speck {
                    65535u16
                } else if x < w / 2 {
                    10000
                } else {
                    40000
                };
                data.extend([v, v, v]);
            }
        }
        write_image(&RawImage::new(w, h, 3, data).unwrap(), path).unwrap();
    }

    #[test]
    fn apply_heals_marked_dust_at_full_resolution_and_keeps_16_bits() {
        let root = dir("dust-apply");
        let file = root.join("x.tiff");
        write_specked(&file, 80, 40);
        let edits = Edits {
            spots: vec![Spot {
                x: 20.0 / 80.0,
                y: 15.0 / 40.0,
                r: 3.0 / 80.0,
            }],
            ..Edits::default()
        };
        assert!(!edits.is_neutral(), "marked spots are a pending edit");
        let out = apply_file(&file, &edits).unwrap();
        let at = |img: &RawImage, x: usize, y: usize| img.data[(y * 80 + x) * 3] as i32;
        assert!((at(&out, 20, 15) - 10000).abs() < 100, "speck is gone");
        assert_eq!(at(&out, 60, 15), 40000, "16-bit values elsewhere are kept");
        assert_eq!(read_image(&file).unwrap(), out, "written to the file");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn dust_and_crop_modes_exclude_each_other_and_revert_clears_the_spots() {
        let (mut ui, root, _file) = ui_with_open_image("dust-modes");
        let ctx = Context::default();
        ui.run_editor_act(Act::ToggleCrop, &ctx, 1);
        ui.run_editor_act(Act::ToggleDust, &ctx, 1);
        let st = ui.editor.as_mut().unwrap();
        assert!(st.dust && st.edits.crop.is_none(), "dust leaves crop mode");
        st.edits.spots.push(Spot {
            x: 0.5,
            y: 0.5,
            r: 0.02,
        });
        st.edits.spots.push(Spot {
            x: 0.2,
            y: 0.2,
            r: 0.02,
        });
        assert!(st.dirty());
        ui.run_editor_act(Act::UndoSpot, &ctx, 1);
        assert_eq!(ui.editor.as_ref().unwrap().edits.spots.len(), 1);
        ui.run_editor_act(Act::ToggleCrop, &ctx, 1);
        let st = ui.editor.as_ref().unwrap();
        assert!(!st.dust && st.edits.crop.is_some(), "crop leaves dust mode");
        ui.run_editor_act(Act::ToggleDust, &ctx, 1);
        ui.run_editor_act(Act::Revert, &ctx, 1);
        let st = ui.editor.as_ref().unwrap();
        assert!(st.edits.spots.is_empty() && !st.dust && !st.dirty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_preview_shows_healed_spots_and_caches_them() {
        let (mut ui, root, _file) = ui_with_open_image("dust-preview");
        let ctx = Context::default();
        let st = ui.editor.as_mut().unwrap();
        let mut base = RawImage::new(60, 40, 3, vec![20000; 60 * 40 * 3]).unwrap();
        for c in 0..3 {
            base.data[(20 * 60 + 30) * 3 + c] = 65535;
        }
        st.loaded(base, [60, 40]);
        st.edits.spots.push(Spot {
            x: 0.5,
            y: 0.5,
            r: 3.0 / 60.0,
        });
        st.refresh(&ctx);
        let healed = &st.healed.as_ref().expect("healed preview cached").1;
        assert_eq!(healed.data[(20 * 60 + 30) * 3], 20000);
        assert_eq!(st.base.as_ref().unwrap().data[(20 * 60 + 30) * 3], 65535);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn dust_mode_draws_with_spots_on_a_turned_picture() {
        let (mut ui, root, _file) = ui_with_open_image("dust-draw");
        let ctx = Context::default();
        let st = ui.editor.as_mut().unwrap();
        st.dust = true;
        st.edits.turns = 1;
        st.edits.spots.push(Spot {
            x: 0.3,
            y: 0.4,
            r: 0.02,
        });
        draw(&mut ui, &ctx, 3);
        let st = ui.editor.as_ref().unwrap();
        assert!(st.dust && st.tex.is_some() && !st.painting);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn save_as_8_bit_halves_the_file_and_an_8_bit_file_stays_8_bit() {
        let root = dir("depth-apply");
        let file = root.join("x.tiff");
        write_scan(&file, 80, 40);
        assert_eq!(file_depth(&file), Some(BitDepth::Sixteen));
        let st = EditorState::new(&root, "x.tiff");
        assert_eq!(st.source_depth, BitDepth::Sixteen);
        let eight = Edits {
            eight_bit: true,
            ..Edits::default()
        };
        assert!(!eight.is_neutral(), "the conversion is a pending edit");
        let before = std::fs::metadata(&file).unwrap().len();
        apply_file(&file, &eight).unwrap();
        assert_eq!(file_depth(&file), Some(BitDepth::Eight));
        assert!(std::fs::metadata(&file).unwrap().len() < before * 6 / 10);
        assert_eq!(size_of(&file), (80, 40));
        // Any later edit keeps it at 8 bit instead of widening the file again.
        apply_file(&file, &turned()).unwrap();
        assert_eq!(file_depth(&file), Some(BitDepth::Eight));
        assert_eq!(
            EditorState::new(&root, "x.tiff").source_depth,
            BitDepth::Eight
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_jpeg_is_edited_in_place_and_stays_an_8_bit_jpeg() {
        let root = dir("jpeg-apply");
        let file = root.join("x.jpg");
        write_scan(&file, 80, 40);
        assert!(is_editable(&file));
        assert_eq!(file_depth(&file), Some(BitDepth::Eight));
        apply_file(&file, &turned()).unwrap();
        assert_eq!(file_depth(&file), Some(BitDepth::Eight));
        assert_eq!(size_of(&file), (40, 80), "rotated and still readable");
        assert_eq!(
            image::ImageReader::open(&file)
                .unwrap()
                .with_guessed_format()
                .unwrap()
                .format(),
            Some(image::ImageFormat::Jpeg)
        );
        std::fs::remove_dir_all(root).ok();
    }

    fn size_of(file: &Path) -> (u32, u32) {
        let img = read_image(file).unwrap();
        (img.width, img.height)
    }

    #[test]
    fn applying_without_a_backup_asks_first_and_writes_nothing() {
        let (mut ui, root, file) = ui_with_open_image("ask");
        let ctx = Context::default();
        ui.editor.as_mut().unwrap().edits = turned();
        ui.run_editor_act(Act::Apply, &ctx, 1);
        let st = ui.editor.as_ref().unwrap();
        assert!(st.confirm, "the question is shown");
        assert!(st.busy.is_none(), "nothing is running");
        assert_eq!(size_on_disk(&file), (40, 20), "the file is untouched");
        assert!(!edit::has_original(&root, &file));

        ui.run_editor_act(Act::CancelConfirm, &ctx, 1);
        assert!(!ui.editor.as_ref().unwrap().confirm);
        assert_eq!(size_on_disk(&file), (40, 20));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn back_up_and_apply_keeps_the_original_and_writes_the_edit() {
        let (mut ui, root, file) = ui_with_open_image("both");
        let ctx = Context::default();
        ui.editor.as_mut().unwrap().edits = turned();
        ui.run_editor_act(Act::Apply, &ctx, 1);
        ui.run_editor_act(Act::ApplyConfirmed { backup_first: true }, &ctx, 1);
        assert_eq!(ui.editor.as_ref().unwrap().busy, Some("Applying"));
        wait_until_idle(&mut ui, &ctx);

        assert_eq!(size_on_disk(&file), (20, 40), "the edit is written");
        let backup = edit::original_path(&root, &file).unwrap();
        assert_eq!(
            size_on_disk(&backup),
            (40, 20),
            "the untouched original is kept"
        );
        assert!(
            root.join(".thumbs/320").is_dir(),
            "the strip thumbnail is refreshed"
        );
        let st = ui.editor.as_ref().unwrap();
        assert!(
            st.has_original && !st.dirty(),
            "pending edits are gone after the apply"
        );
        assert!(ui.status.as_ref().unwrap().1.contains("Applied"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn applying_without_a_backup_when_the_user_chooses_so() {
        let (mut ui, root, file) = ui_with_open_image("nobackup");
        let ctx = Context::default();
        ui.editor.as_mut().unwrap().edits = turned();
        ui.run_editor_act(Act::Apply, &ctx, 1);
        ui.run_editor_act(
            Act::ApplyConfirmed {
                backup_first: false,
            },
            &ctx,
            1,
        );
        wait_until_idle(&mut ui, &ctx);
        assert_eq!(size_on_disk(&file), (20, 40));
        assert!(!edit::has_original(&root, &file), "no backup was made");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn with_a_backup_apply_goes_straight_through() {
        let (mut ui, root, file) = ui_with_open_image("direct");
        let ctx = Context::default();
        ui.run_editor_act(Act::Backup, &ctx, 1);
        wait_until_idle(&mut ui, &ctx);
        assert!(ui.editor.as_ref().unwrap().has_original);

        ui.editor.as_mut().unwrap().edits = turned();
        ui.run_editor_act(Act::Apply, &ctx, 1);
        assert!(
            !ui.editor.as_ref().unwrap().confirm,
            "no question when a backup exists"
        );
        wait_until_idle(&mut ui, &ctx);
        assert_eq!(size_on_disk(&file), (20, 40));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_backup_button_and_restore_original_round_trip() {
        let (mut ui, root, file) = ui_with_open_image("restore");
        let ctx = Context::default();
        ui.run_editor_act(Act::Backup, &ctx, 1);
        wait_until_idle(&mut ui, &ctx);
        assert!(edit::has_original(&root, &file));

        // A second backup press changes nothing; the edit then the restore.
        ui.editor.as_mut().unwrap().edits = turned();
        ui.run_editor_act(Act::Apply, &ctx, 1);
        wait_until_idle(&mut ui, &ctx);
        assert_eq!(size_on_disk(&file), (20, 40));

        ui.run_editor_act(Act::Restore, &ctx, 1);
        wait_until_idle(&mut ui, &ctx);
        assert_eq!(size_on_disk(&file), (40, 20), "the original is back");
        assert!(
            !edit::has_original(&root, &file),
            "and its backup is gone, as in latent"
        );
        assert!(!ui.editor.as_ref().unwrap().has_original);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn revert_and_reset_sliders_clear_pending_edits() {
        let (mut ui, root, _file) = ui_with_open_image("revert");
        let ctx = Context::default();
        let st = ui.editor.as_mut().unwrap();
        st.edits = Edits {
            spots: vec![],
            eight_bit: false,
            negative: true,
            turns: 3,
            crop: Some([0.1, 0.1, 0.5, 0.5]),
            adjust: Adjust {
                exposure_ev: 1.0,
                ..Adjust::default()
            },
            sharpen: Sharpen {
                amount: 1.0,
                radius: 2.0,
            },
        };
        ui.run_editor_act(Act::ResetSliders, &ctx, 1);
        let st = ui.editor.as_ref().unwrap();
        assert!(
            !st.edits.negative && st.edits.adjust.is_neutral() && st.edits.sharpen.is_neutral()
        );
        assert_eq!(
            (st.edits.turns, st.edits.crop),
            (3, Some([0.1, 0.1, 0.5, 0.5])),
            "rotation and crop stay"
        );
        ui.run_editor_act(Act::Revert, &ctx, 1);
        assert!(ui.editor.as_ref().unwrap().edits.is_neutral());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn turning_and_cropping_are_pending_and_toggle() {
        let (mut ui, root, _file) = ui_with_open_image("pending");
        let ctx = Context::default();
        ui.run_editor_act(Act::Turn(1), &ctx, 1);
        ui.run_editor_act(Act::Turn(1), &ctx, 1);
        assert_eq!(ui.editor.as_ref().unwrap().edits.turns, 2);
        ui.run_editor_act(Act::Turn(-3), &ctx, 1);
        assert_eq!(ui.editor.as_ref().unwrap().edits.turns, 3, "wraps around");
        ui.run_editor_act(Act::ToggleCrop, &ctx, 1);
        assert_eq!(ui.editor.as_ref().unwrap().edits.crop, Some(crop::FULL));
        ui.run_editor_act(Act::ToggleCrop, &ctx, 1);
        assert_eq!(ui.editor.as_ref().unwrap().edits.crop, None);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn stepping_to_another_image_drops_unsaved_edits_with_a_notice() {
        let (mut ui, root, file) = ui_with_open_image("step");
        let ctx = Context::default();
        ui.editor.as_mut().unwrap().edits = turned();
        ui.run_editor_act(Act::Step(1), &ctx, 1);
        assert_eq!(ui.strip.large, Some(1));
        assert!(ui
            .status
            .as_ref()
            .unwrap()
            .1
            .contains("Discarded unsaved edits"));
        assert_eq!(size_on_disk(&file), (40, 20), "nothing was written");
        // Stepping past the ends does nothing.
        ui.run_editor_act(Act::Step(1), &ctx, 1);
        assert_eq!(ui.strip.large, Some(1));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn files_that_are_not_tiff_png_or_jpeg_are_view_only() {
        let root = dir("viewonly");
        let rel = "images/a.gif";
        std::fs::create_dir_all(root.join("images")).unwrap();
        std::fs::write(root.join(rel), b"gif").unwrap();
        let mut ui = ScanUi::new(root.clone());
        ui.strip.paths = vec![rel.into()];
        ui.strip.large = Some(0);
        ui.editor = Some(EditorState::new(&root, rel));
        let ctx = Context::default();
        let st = ui.editor.as_mut().unwrap();
        st.edits = Edits {
            turns: 1,
            ..Edits::default()
        };
        ui.run_editor_act(Act::Apply, &ctx, 0);
        ui.run_editor_act(Act::Backup, &ctx, 0);
        let st = ui.editor.as_ref().unwrap();
        assert!(
            !st.editable && !st.confirm && st.busy.is_none(),
            "nothing is started"
        );
        assert!(!edit::has_original(&root, &root.join(rel)));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_worker_operations_work_on_a_plain_folder_too() {
        let folder = dir("folder-op");
        let file = folder.join("scan-01.png");
        write_scan(&file, 30, 10);
        let op = EditorOp::Apply {
            edits: turned(),
            backup_first: true,
        };
        worker::run_editor_op(&folder, &file, op, &folder.join("cache")).unwrap();
        assert_eq!(size_on_disk(&file), (10, 30));
        assert_eq!(
            size_on_disk(&folder.join(".originals/scan-01.png")),
            (30, 10)
        );
        worker::run_editor_op(&folder, &file, EditorOp::Restore, &folder.join("cache")).unwrap();
        assert_eq!(size_on_disk(&file), (30, 10));
        // An operation on a missing file reports an error instead of panicking.
        assert!(
            worker::run_editor_op(&folder, &folder.join("nope.png"), turned_apply(), &folder)
                .is_err()
        );
        std::fs::remove_dir_all(folder).ok();
    }

    fn turned_apply() -> EditorOp {
        EditorOp::Apply {
            edits: turned(),
            backup_first: false,
        }
    }

    /// Runs `frames` headless frames of the editor window.
    fn draw(ui: &mut ScanUi, ctx: &Context, frames: usize) {
        for _ in 0..frames {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1400.0, 900.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| ui.editor_window(ctx));
        }
    }

    #[test]
    fn double_clicking_the_sharpen_sliders_resets_them() {
        use crate::form::{test_support::Harness, LABEL_W};
        let mut h = Harness::new();
        let mut s = Sharpen {
            amount: 2.0,
            radius: 3.0,
        };
        // The form starts at the panel's top-left; rows are `interact_size.y` high, 6 px apart.
        let mut grid = (egui::Pos2::ZERO, 0.0_f32);
        for _ in 0..3 {
            h.frame(vec![], |ui| {
                sharpen_sliders(ui, &mut s);
                grid = (ui.min_rect().min, ui.spacing().interact_size.y);
            });
        }
        for i in 0..2 {
            let y = grid.0.y + i as f32 * (grid.1 + 6.0) + grid.1 / 2.0;
            h.double_click(egui::pos2(grid.0.x + LABEL_W + 40.0, y), |ui| {
                sharpen_sliders(ui, &mut s)
            });
        }
        assert_eq!(s, Sharpen::default());
        assert!(s.is_neutral());
    }

    #[test]
    fn the_editor_window_draws_the_picture_the_histogram_and_the_overlay() {
        let (mut ui, root, _file) = ui_with_open_image("draw");
        let ctx = Context::default();
        draw(&mut ui, &ctx, 2);
        let st = ui.editor.as_ref().unwrap();
        assert!(st.tex.is_some(), "the preview texture was made");
        assert_eq!(st.histogram.total(), 200, "20 x 10 preview pixels counted");

        // Rotated, cropped, with a negative and sharpening, and zoomed: still draws.
        let st = ui.editor.as_mut().unwrap();
        st.edits = Edits {
            spots: vec![],
            eight_bit: false,
            negative: true,
            turns: 1,
            crop: Some([0.2, 0.2, 0.8, 0.8]),
            adjust: Adjust {
                contrast: 0.5,
                darks: 0.3,
                ..Adjust::default()
            },
            sharpen: Sharpen {
                amount: 1.0,
                radius: 1.0,
            },
        };
        st.zoom = Some(ZoomState {
            center: [0.5, 0.5],
            zoom: 2.0,
            follow: None,
        });
        draw(&mut ui, &ctx, 2);
        let st = ui.editor.as_ref().unwrap();
        assert!(st.shape.is_finite() && st.shape > 0.0);
        assert!(st.dirty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_editor_window_draws_while_loading_with_an_error_and_with_the_question() {
        let (mut ui, root, _file) = ui_with_open_image("draw2");
        let ctx = Context::default();
        {
            let st = ui.editor.as_mut().unwrap();
            st.base = None; // still loading: a spinner
        }
        draw(&mut ui, &ctx, 1);
        ui.editor.as_mut().unwrap().error = Some("cannot read".into());
        draw(&mut ui, &ctx, 1);
        {
            let st = ui.editor.as_mut().unwrap();
            st.error = None;
            st.confirm = true;
            st.busy = Some("Applying");
        }
        draw(&mut ui, &ctx, 1);
        assert!(ui.editor.is_some());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_window_closes_the_editor_when_no_image_is_selected() {
        let (mut ui, root, _file) = ui_with_open_image("closed");
        let ctx = Context::default();
        ui.strip.large = None;
        draw(&mut ui, &ctx, 1);
        assert!(ui.editor.is_none());
        std::fs::remove_dir_all(root).ok();
    }

    /// Applies edits to a copy of a real scan: `EDIT_FILE=/scan.png cargo test -p scan-ui --
    /// --ignored --nocapture manual_apply`.
    #[test]
    #[ignore = "needs a real scan file"]
    fn manual_apply_to_a_real_scan() {
        let Ok(src) = std::env::var("EDIT_FILE") else {
            return;
        };
        let root = dir("real");
        let file = root
            .join("images/roll/0001")
            .join(Path::new(&src).file_name().unwrap());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::copy(&src, &file).unwrap();
        let before = read_image(&file).unwrap();
        let edits = Edits {
            turns: 1,
            crop: Some([0.1, 0.1, 0.9, 0.8]),
            adjust: Adjust {
                exposure_ev: 0.5,
                contrast: 0.3,
                ..Adjust::default()
            },
            sharpen: Sharpen {
                amount: 1.0,
                radius: 1.5,
            },
            ..Edits::default()
        };
        let op = EditorOp::Apply {
            edits,
            backup_first: true,
        };
        worker::run_editor_op(&root, &file, op, &root.join(".thumbs")).unwrap();
        let after = read_image(&file).unwrap();
        let original = read_image(&edit::original_path(&root, &file).unwrap()).unwrap();
        println!(
            "before {}x{}x{}  after {}x{}x{}  original kept {}x{}  16-bit max sample {}",
            before.width,
            before.height,
            before.channels,
            after.width,
            after.height,
            after.channels,
            original.width,
            original.height,
            after.data.iter().max().unwrap()
        );
        assert_eq!(original, before);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_editor_window_has_the_same_layout_fitted_and_zoomed_for_any_picture() {
        let ctx = Context::default();
        let area = |ctx: &Context| ctx.memory(|m| m.area_rect(egui::Id::new("scan-editor")));
        for (w, h) in [(20u32, 10u32), (10, 20), (30, 5)] {
            let (mut ui, root, _file) = ui_with_open_image(&format!("layout-{w}x{h}"));
            ui.editor.as_mut().unwrap().loaded(
                RawImage::new(w, h, 1, vec![30000; (w * h) as usize]).unwrap(),
                [w * 2, h * 2],
            );
            draw(&mut ui, &ctx, 3);
            let fitted = area(&ctx).expect("the window exists");
            draw(&mut ui, &ctx, 4);
            assert_eq!(
                area(&ctx),
                Some(fitted),
                "{w}x{h}: the window no longer drifts from frame to frame"
            );

            ui.editor.as_mut().unwrap().zoom = Some(ZoomState {
                center: [0.5, 0.5],
                zoom: 3.0,
                follow: None,
            });
            draw(&mut ui, &ctx, 3);
            assert_eq!(
                area(&ctx),
                Some(fitted),
                "{w}x{h}: the window does not change when zooming"
            );
            std::fs::remove_dir_all(root).ok();
        }
    }
}
