//! The image editor window of latent-scan over a list of saved scans: adjust, sharpen, rotate,
//! crop and remove dust with a live histogram, modelled on the image view of latent.
//!
//! The editing state, the render thread and the file operations are latent's
//! (`latent_image_edit_ui::editor`); this is the window around them. Edits only change the
//! preview. **Apply** writes them to the file (at full resolution, same format, 16 bit kept);
//! the untouched original can be saved first with **Backup original** (`.originals/…`).

use egui::{Color32, Context, RichText, Ui};
use latent_image_edit::clarity::Clarity;
use latent_image_edit::sharpen::Sharpen;
use latent_image_edit::tone::Adjust;
use latent_image_edit::BitDepth;
use latent_image_edit::RawImage;
use latent_image_edit_ui::crop::{self};
use latent_image_edit_ui::dust;
use latent_image_edit_ui::editor::{
    auto_levels, decode, run_op, EditorOp, EditorState, Edits, Loaded,
};
use latent_image_edit_ui::form::{button_row, form, form_buttons, Btn};
use latent_image_edit_ui::histogram;
use latent_image_edit_ui::presets::PresetUi;
use latent_image_edit_ui::sliders::{adjust_groups, preset_row};
use latent_image_edit_ui::view;
use latent_image_edit_ui::{canvas, edit, ACCENT};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

/// Width of the side column with the histogram and the sliders.
const SIDE_W: f32 = 300.0;
const HISTOGRAM_H: f32 = 100.0;

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

/// Something the host should react to.
#[derive(Debug)]
pub enum EditorEvent {
    /// The file was rewritten by an Apply (`image` is what was written) or brought back by
    /// Restore original (`None`): thumbnails of it are stale.
    FileChanged {
        path: PathBuf,
        image: Option<RawImage>,
    },
    /// The user stepped to the file with this index.
    Selected(usize),
    /// The window was closed.
    Closed,
}

/// Results of the worker threads.
enum Msg {
    Loaded {
        path: PathBuf,
        result: Result<Loaded, String>,
    },
    Done {
        path: PathBuf,
        what: &'static str,
        result: Result<Option<RawImage>, String>,
    },
}

#[derive(Clone)]
struct Reporter {
    tx: Sender<Msg>,
    ctx: Context,
}

impl Reporter {
    fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
        self.ctx.request_repaint();
    }
}

/// Decodes the file on a thread: the preview, the file's size and the levels for the sharp part.
fn load(r: Reporter, path: PathBuf) {
    std::thread::spawn(move || {
        let result = decode(&path);
        r.send(Msg::Loaded { path, result });
    });
}

/// Runs a file operation on a thread.
fn spawn_op(r: Reporter, root: PathBuf, path: PathBuf, op: EditorOp) {
    std::thread::spawn(move || {
        let what = op.name();
        let result = run_op(&root, &path, op);
        r.send(Msg::Done { path, what, result });
    });
}

/// The image editor window over a list of files. The host owns it, calls [`Editor::sync`] to say
/// which file is wanted and [`Editor::show`] every frame, and reacts to the [`EditorEvent`]s.
pub struct Editor {
    root: PathBuf,
    files: Vec<String>,
    index: usize,
    pub state: Option<EditorState>,
    /// Last message (an error or what was done), shown in the window.
    pub status: Option<(bool, String)>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            root: PathBuf::new(),
            files: vec![],
            index: 0,
            state: None,
            status: None,
            tx,
            rx,
        }
    }

    /// Path (relative to the root) of the open file.
    pub fn current(&self) -> Option<&str> {
        self.state.as_ref().map(|s| s.rel.as_str())
    }

    pub fn close(&mut self) {
        self.state = None;
    }

    /// Makes `files[index]` (paths relative to `root`, where backups go to `.originals/`) the
    /// open file. Nothing happens when it already is, so this can be called every frame.
    pub fn sync(&mut self, ctx: &Context, root: &Path, files: &[String], index: usize) {
        let Some(rel) = files.get(index) else {
            self.close();
            return;
        };
        if self.files != files {
            self.files = files.to_vec();
        }
        self.index = index;
        if self.current() != Some(rel.as_str()) || self.root != root {
            self.root = root.to_path_buf();
            self.go(ctx, index);
        }
    }

    fn go(&mut self, ctx: &Context, index: usize) {
        let Some(rel) = self.files.get(index).cloned() else {
            return;
        };
        self.index = index;
        self.state = Some(EditorState::new(&self.root, &rel));
        self.reload(ctx);
    }

    fn reporter(&self, ctx: &Context) -> Reporter {
        Reporter {
            tx: self.tx.clone(),
            ctx: ctx.clone(),
        }
    }

    fn reload(&self, ctx: &Context) {
        if let Some(e) = &self.state {
            load(self.reporter(ctx), e.path.clone());
        }
    }

    /// Worker results.
    fn poll(&mut self, ctx: &Context, events: &mut Vec<EditorEvent>) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Loaded { path, result } => {
                    if let Some(e) = self.state.as_mut().filter(|e| e.path == path) {
                        match result {
                            Ok(l) => e.loaded_from(l),
                            Err(msg) => {
                                e.busy = None;
                                e.error = Some(msg);
                            }
                        }
                        e.has_original = edit::has_original(&self.root, &path);
                    }
                }
                Msg::Done { path, what, result } => self.done(ctx, path, what, result, events),
            }
        }
    }

    /// A file operation finished.
    fn done(
        &mut self,
        ctx: &Context,
        path: PathBuf,
        what: &str,
        result: Result<Option<RawImage>, String>,
        events: &mut Vec<EditorEvent>,
    ) {
        let name = path
            .file_name()
            .map_or(String::new(), |n| n.to_string_lossy().into_owned());
        let mine = self.state.as_ref().is_some_and(|e| e.path == path);
        match result {
            Ok(image) => {
                self.status = Some((
                    false,
                    match what {
                        "Apply" => format!("Applied the edits to {name}"),
                        "Backup" => format!("Backed up {name}"),
                        _ => format!("Restored the original of {name}"),
                    },
                ));
                if what != "Backup" {
                    events.push(EditorEvent::FileChanged { path, image });
                    if mine {
                        if let Some(e) = &mut self.state {
                            e.reset_for_reload(&self.root);
                        }
                        self.reload(ctx);
                    }
                } else if let Some(e) = self.state.as_mut().filter(|_| mine) {
                    e.has_original = true;
                    e.busy = None;
                }
            }
            Err(msg) => {
                self.status = Some((true, format!("{what} failed: {msg}")));
                if let Some(e) = self.state.as_mut().filter(|_| mine) {
                    e.busy = None;
                    e.has_original = edit::has_original(&self.root, &path);
                }
            }
        }
    }

    /// Mentions edits that are thrown away when the image is left.
    fn discard_notice(&mut self) {
        if let Some(e) = self.state.as_ref().filter(|e| e.dirty()) {
            self.status = Some((false, format!("Discarded unsaved edits of {}", e.rel)));
        }
    }

    /// Draws the window and returns what happened. `presets` is shared with the host so the
    /// same presets are offered everywhere.
    pub fn show(&mut self, ctx: &Context, presets: &mut PresetUi) -> Vec<EditorEvent> {
        let mut events = vec![];
        self.poll(ctx, &mut events);
        let last = self.files.len().saturating_sub(1);
        let n = self.index;
        let status = self.status.clone();
        let Some(st) = &mut self.state else {
            return events;
        };
        st.refresh(ctx);

        let mut open = true;
        let mut acts: Vec<Act> = vec![];
        let title = if st.dirty() {
            format!("{} *", st.rel)
        } else {
            st.rel.clone()
        };
        egui::Window::new(title)
            .id(egui::Id::new("latent-image-editor"))
            .open(&mut open)
            .default_size([1180.0, 760.0])
            .show(ctx, |ui| {
                editor_ui(ui, st, presets, status.as_ref(), n, last, &mut acts)
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
            self.act(act, ctx, &mut events);
        }
        if !open || escape {
            self.close();
            events.push(EditorEvent::Closed);
        }
        events
    }

    fn act(&mut self, act: Act, ctx: &Context, events: &mut Vec<EditorEvent>) {
        let reporter = self.reporter(ctx);
        let root = self.root.clone();
        if let Act::Step(d) = act {
            let n = self.index as i32 + d;
            if (0..self.files.len() as i32).contains(&n) {
                self.discard_notice();
                self.go(ctx, n as usize);
                events.push(EditorEvent::Selected(n as usize));
            }
            return;
        }
        let Some(st) = &mut self.state else { return };
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
                    start_op(st, &root, &reporter, false);
                } else {
                    st.confirm = true;
                }
            }
            Act::ApplyConfirmed { backup_first } if st.editable && !busy => {
                st.confirm = false;
                start_op(st, &root, &reporter, backup_first);
            }
            Act::CancelConfirm => st.confirm = false,
            Act::Revert => {
                st.edits = Edits::default();
                st.dust = false;
                st.zoom = None;
            }
            Act::Backup if st.editable && !busy && !st.has_original => {
                st.busy = Some("Backing up");
                spawn_op(reporter, root, st.path.clone(), EditorOp::Backup);
            }
            Act::Restore if st.editable && !busy && st.has_original => {
                st.busy = Some("Restoring");
                spawn_op(reporter, root, st.path.clone(), EditorOp::Restore);
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
                st.edits.clarity = Clarity::default();
                st.edits.sharpen = Sharpen::default();
            }
            _ => {}
        }
    }
}

/// Starts the apply on a worker thread.
fn start_op(st: &mut EditorState, root: &Path, reporter: &Reporter, backup_first: bool) {
    st.busy = Some("Applying");
    let op = EditorOp::Apply {
        edits: st.edits.clone(),
        backup_first,
    };
    spawn_op(reporter.clone(), root.to_path_buf(), st.path.clone(), op);
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
    status: Option<&(bool, String)>,
    n: usize,
    last: usize,
    acts: &mut Vec<Act>,
) {
    toolbar(ui, st, n, last, acts);
    if let Some((error, msg)) = status {
        let text = RichText::new(msg);
        ui.label(if *error {
            text.color(Color32::LIGHT_RED)
        } else {
            text.weak()
        });
    }
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
            // A fixed width: the count growing a digit must not rewrap the toolbar.
            if ui
                .add_enabled(
                    marked,
                    egui::Button::new(format!("Clear ({})", st.edits.spots.len()))
                        .min_size(egui::vec2(88.0, 0.0)),
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
    // The crop frame and the dust brush own the primary drag; without them it grabs the picture.
    let tool = st.edits.crop.is_some() || st.dust;
    let canvas = canvas::show(ui, pane, tex, aspect, &mut st.zoom, !tool, |_, _| {});
    st.shape = canvas.shape;
    if !tool {
        if canvas.resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if canvas.resp.hovered() && st.zoom.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
    }
    st.show_detail(ui, canvas.rect, canvas.window);
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
    if st.rendering() {
        state.push("rendering…".to_string());
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
    // A block of two lines whatever the text: a click changes it ("rendering…", "n dust
    // spots"), and a line that wraps would grow the window and move the picture.
    let text = state.join(" · ");
    let line = ui.text_style_height(&egui::TextStyle::Body);
    let (block, _) = ui.allocate_exact_size(egui::vec2(SIDE_W, 2.0 * line), egui::Sense::hover());
    let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(block));
    inner.set_clip_rect(block.intersect(ui.clip_rect()));
    inner
        .add(egui::Label::new(RichText::new(&text).weak()).wrap())
        .on_hover_text(&text);
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
            (
                &st.edits.adjust,
                Some((&st.edits.clarity, &st.edits.sharpen)),
            ),
        ) {
            st.edits.adjust = p.adjust();
            st.edits.clarity = p.clarity();
            st.edits.sharpen = p.sharpen();
        }
        ui.add_space(4.0);
        let edits = &mut st.edits;
        adjust_groups(
            ui,
            "editor",
            &mut edits.adjust,
            Some((&mut edits.clarity, &mut edits.sharpen)),
            colour,
        );
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
#[cfg(test)]
mod tests {
    use super::*;
    use latent_image_edit::heal::Spot;
    use latent_image_edit::write_image;
    use latent_image_edit::{file_depth, read_image};
    use latent_image_edit_ui::editor::{apply_file, is_editable};
    use latent_image_edit_ui::view::ZoomState;

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
    /// A `Editor` on an archive with one TIFF open in the editor, with a loaded preview.
    fn ui_with_open_image(name: &str) -> (Editor, PathBuf, PathBuf) {
        let root = dir(name);
        let rel = "images/roll_film/0001/x.tiff";
        let file = root.join(rel);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        write_scan(&file, 40, 20);
        let mut ui = Editor::new();
        ui.root = root.clone();
        ui.files = vec![rel.to_string(), "images/roll_film/0001/y.tiff".to_string()];
        let mut st = EditorState::new(&root, rel);
        st.loaded(
            RawImage::new(20, 10, 3, vec![20000; 600]).unwrap(),
            [40, 20],
        );
        ui.state = Some(st);
        (ui, root, file)
    }

    /// Polls until the editor is no longer busy (a worker thread answered).
    fn wait_until_idle(ui: &mut Editor, ctx: &Context) -> Vec<EditorEvent> {
        let mut events = vec![];
        for _ in 0..200 {
            ui.poll(ctx, &mut events);
            if ui.state.as_ref().is_none_or(|e| e.busy.is_none()) {
                return events;
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

    #[test]
    fn clarity_and_saturation_are_pending_edits_and_change_the_file_when_applied() {
        let clarity_only = Edits {
            clarity: Clarity { amount: 0.5 },
            ..Edits::default()
        };
        assert!(!clarity_only.is_neutral());
        let saturated = Edits {
            adjust: Adjust {
                saturation: 0.5,
                ..Adjust::default()
            },
            ..Edits::default()
        };
        assert!(!saturated.is_neutral());

        let root = dir("clarity-apply");
        let file = root.join("x.tiff");
        // A stripe pattern with mid-sized structure and some colour.
        let data: Vec<u16> = (0..200 * 40)
            .flat_map(|i| {
                let v = if (i % 200) / 25 % 2 == 0 {
                    24000
                } else {
                    30000
                };
                [v + 6000, v, v - 6000]
            })
            .collect();
        write_image(&RawImage::new(200, 40, 3, data).unwrap(), &file).unwrap();
        let before = read_image(&file).unwrap();
        apply_file(&file, &clarity_only).unwrap();
        let after = read_image(&file).unwrap();
        assert_eq!((after.width, after.channels), (200, 3));
        assert_ne!(after, before, "clarity changed the pixels");
        let mid = |img: &RawImage| {
            let g: Vec<i32> = img
                .data
                .as_chunks::<3>()
                .0
                .iter()
                .map(|p| p[1] as i32)
                .collect();
            g[60..140].iter().max().unwrap() - g[60..140].iter().min().unwrap()
        };
        assert!(mid(&after) > mid(&before), "more local contrast");

        apply_file(&file, &saturated).unwrap();
        let sat = read_image(&file).unwrap();
        let chroma = |img: &RawImage| (img.data[0] as i32 - img.data[2] as i32).abs();
        assert!(chroma(&sat) > chroma(&after), "more colour");
        std::fs::remove_dir_all(root).ok();
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
        ui.act(Act::ToggleCrop, &ctx, &mut vec![]);
        ui.act(Act::ToggleDust, &ctx, &mut vec![]);
        let st = ui.state.as_mut().unwrap();
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
        ui.act(Act::UndoSpot, &ctx, &mut vec![]);
        assert_eq!(ui.state.as_ref().unwrap().edits.spots.len(), 1);
        ui.act(Act::ToggleCrop, &ctx, &mut vec![]);
        let st = ui.state.as_ref().unwrap();
        assert!(!st.dust && st.edits.crop.is_some(), "crop leaves dust mode");
        ui.act(Act::ToggleDust, &ctx, &mut vec![]);
        ui.act(Act::Revert, &ctx, &mut vec![]);
        let st = ui.state.as_ref().unwrap();
        assert!(st.edits.spots.is_empty() && !st.dust && !st.dirty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn dust_mode_draws_with_spots_on_a_turned_picture() {
        let (mut ui, root, _file) = ui_with_open_image("dust-draw");
        let ctx = Context::default();
        let st = ui.state.as_mut().unwrap();
        st.dust = true;
        st.edits.turns = 1;
        st.edits.spots.push(Spot {
            x: 0.3,
            y: 0.4,
            r: 0.02,
        });
        draw(&mut ui, &ctx, 3);
        let st = ui.state.as_ref().unwrap();
        assert!(st.dust && st.tex.is_some() && !st.painting);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn marking_dust_spots_does_not_move_the_picture() {
        for (sw, sh) in [(1400.0, 900.0), (1366.0, 768.0), (1024.0, 700.0)] {
            let (mut ui, root, _file) = ui_with_open_image("no-wiggle");
            let ctx = Context::default();
            ui.state.as_mut().unwrap().dust = true;
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(sw, sh));
            for _ in 0..5 {
                let input = egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                };
                let _ = ctx.run(input, |ctx| {
                    ui.show(ctx, &mut PresetUi::default());
                });
            }
            let area =
                |ctx: &Context| ctx.memory(|m| m.area_rect(egui::Id::new("latent-image-editor")));
            let fixed = |ui: &Editor, ctx: &Context| (area(ctx), ui.state.as_ref().unwrap().shape);
            let before = fixed(&ui, &ctx);
            assert!(before.0.is_some());
            // What a click changes: more spots (the "Clear (n)" button, the state line), unsaved
            // edits, a backup and a render in flight, and a longer state line.
            for n in 1..=12 {
                {
                    let st = ui.state.as_mut().unwrap();
                    st.edits.spots.push(Spot {
                        x: (n as f32 * 0.07) % 1.0,
                        y: 0.5,
                        r: 0.02,
                    });
                    st.has_original = n % 2 == 0;
                    st.file_size = if n % 3 == 0 { [42000, 30000] } else { [40, 20] };
                }
                let input = egui::RawInput {
                    screen_rect: Some(screen),
                    ..Default::default()
                };
                // One frame, without waiting for the render: "rendering…" is on.
                let _ = ctx.run(input, |ctx| {
                    ui.show(ctx, &mut PresetUi::default());
                });
                assert_eq!(fixed(&ui, &ctx), before, "{sw}x{sh}: after {n} spots");
            }
            std::fs::remove_dir_all(root).ok();
        }
    }

    /// Runs one frame of the editor window with the given pointer events.
    fn frame_with(ui: &mut Editor, ctx: &Context, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1400.0, 900.0),
            )),
            events,
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| {
            ui.show(ctx, &mut PresetUi::default());
        });
    }

    /// Drags the primary button from `from` by `by` over a few frames.
    fn drag(ui: &mut Editor, ctx: &Context, from: egui::Pos2, by: egui::Vec2) {
        use egui::{Event, PointerButton};
        let button = |pos, pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        frame_with(ui, ctx, vec![Event::PointerMoved(from)]);
        frame_with(ui, ctx, vec![button(from, true)]);
        for i in 1..=4 {
            let p = from + by * (i as f32 / 4.0);
            frame_with(ui, ctx, vec![Event::PointerMoved(p)]);
        }
        frame_with(ui, ctx, vec![button(from + by, false)]);
    }

    /// An editor with a zoomed-in picture and the middle of its pane under the pointer.
    fn zoomed_editor(name: &str) -> (Editor, Context, PathBuf, egui::Pos2) {
        let (mut ui, root, _file) = ui_with_open_image(name);
        let ctx = Context::default();
        ui.state.as_mut().unwrap().zoom = Some(ZoomState {
            center: [0.5, 0.5],
            zoom: 3.0,
            follow: None,
        });
        draw(&mut ui, &ctx, 4);
        let window = ctx
            .memory(|m| m.area_rect(egui::Id::new("latent-image-editor")))
            .expect("the window exists");
        // Left of the side column, below the toolbar: inside the pane.
        let pane_middle = egui::pos2(window.left() + 300.0, window.top() + 300.0);
        (ui, ctx, root, pane_middle)
    }

    #[test]
    fn dragging_the_picture_with_the_primary_button_pans_it() {
        let (mut ui, ctx, root, at) = zoomed_editor("drag-pan");
        let before = ui.state.as_ref().unwrap().zoom.unwrap().center;
        drag(&mut ui, &ctx, at, egui::vec2(-120.0, 0.0));
        let after = ui.state.as_ref().unwrap().zoom.unwrap().center;
        assert!(after[0] > before[0] + 0.01, "{before:?} -> {after:?}");
        assert!((after[1] - before[1]).abs() < 1e-3, "only sideways");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn with_the_dust_brush_or_the_crop_the_primary_drag_does_not_pan() {
        for crop_tool in [false, true] {
            let (mut ui, ctx, root, at) = zoomed_editor("drag-tool");
            {
                let st = ui.state.as_mut().unwrap();
                st.dust = !crop_tool;
                st.edits.crop = crop_tool.then_some([0.4, 0.4, 0.6, 0.6]);
            }
            let before = ui.state.as_ref().unwrap().zoom.unwrap().center;
            drag(&mut ui, &ctx, at, egui::vec2(-120.0, 0.0));
            let st = ui.state.as_ref().unwrap();
            assert_eq!(st.zoom.unwrap().center, before, "crop tool: {crop_tool}");
            std::fs::remove_dir_all(root).ok();
        }
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
        ui.state.as_mut().unwrap().edits = turned();
        ui.act(Act::Apply, &ctx, &mut vec![]);
        let st = ui.state.as_ref().unwrap();
        assert!(st.confirm, "the question is shown");
        assert!(st.busy.is_none(), "nothing is running");
        assert_eq!(size_on_disk(&file), (40, 20), "the file is untouched");
        assert!(!edit::has_original(&root, &file));

        ui.act(Act::CancelConfirm, &ctx, &mut vec![]);
        assert!(!ui.state.as_ref().unwrap().confirm);
        assert_eq!(size_on_disk(&file), (40, 20));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn back_up_and_apply_keeps_the_original_and_writes_the_edit() {
        let (mut ui, root, file) = ui_with_open_image("both");
        let ctx = Context::default();
        ui.state.as_mut().unwrap().edits = turned();
        ui.act(Act::Apply, &ctx, &mut vec![]);
        ui.act(
            Act::ApplyConfirmed { backup_first: true },
            &ctx,
            &mut vec![],
        );
        assert_eq!(ui.state.as_ref().unwrap().busy, Some("Applying"));
        let events = wait_until_idle(&mut ui, &ctx);

        assert_eq!(size_on_disk(&file), (20, 40), "the edit is written");
        let backup = edit::original_path(&root, &file).unwrap();
        assert_eq!(
            size_on_disk(&backup),
            (40, 20),
            "the untouched original is kept"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, EditorEvent::FileChanged { image: Some(_), .. })),
            "the host is told which image was written"
        );
        let st = ui.state.as_ref().unwrap();
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
        ui.state.as_mut().unwrap().edits = turned();
        ui.act(Act::Apply, &ctx, &mut vec![]);
        ui.act(
            Act::ApplyConfirmed {
                backup_first: false,
            },
            &ctx,
            &mut vec![],
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
        ui.act(Act::Backup, &ctx, &mut vec![]);
        wait_until_idle(&mut ui, &ctx);
        assert!(ui.state.as_ref().unwrap().has_original);

        ui.state.as_mut().unwrap().edits = turned();
        ui.act(Act::Apply, &ctx, &mut vec![]);
        assert!(
            !ui.state.as_ref().unwrap().confirm,
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
        ui.act(Act::Backup, &ctx, &mut vec![]);
        wait_until_idle(&mut ui, &ctx);
        assert!(edit::has_original(&root, &file));

        // A second backup press changes nothing; the edit then the restore.
        ui.state.as_mut().unwrap().edits = turned();
        ui.act(Act::Apply, &ctx, &mut vec![]);
        wait_until_idle(&mut ui, &ctx);
        assert_eq!(size_on_disk(&file), (20, 40));

        ui.act(Act::Restore, &ctx, &mut vec![]);
        wait_until_idle(&mut ui, &ctx);
        assert_eq!(size_on_disk(&file), (40, 20), "the original is back");
        assert!(
            !edit::has_original(&root, &file),
            "and its backup is gone, as in latent"
        );
        assert!(!ui.state.as_ref().unwrap().has_original);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn revert_and_reset_sliders_clear_pending_edits() {
        let (mut ui, root, _file) = ui_with_open_image("revert");
        let ctx = Context::default();
        let st = ui.state.as_mut().unwrap();
        st.edits = Edits {
            spots: vec![],
            eight_bit: false,
            clarity: Clarity { amount: 0.3 },
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
        ui.act(Act::ResetSliders, &ctx, &mut vec![]);
        let st = ui.state.as_ref().unwrap();
        assert!(
            !st.edits.negative && st.edits.adjust.is_neutral() && st.edits.sharpen.is_neutral()
        );
        assert_eq!(
            (st.edits.turns, st.edits.crop),
            (3, Some([0.1, 0.1, 0.5, 0.5])),
            "rotation and crop stay"
        );
        ui.act(Act::Revert, &ctx, &mut vec![]);
        assert!(ui.state.as_ref().unwrap().edits.is_neutral());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn turning_and_cropping_are_pending_and_toggle() {
        let (mut ui, root, _file) = ui_with_open_image("pending");
        let ctx = Context::default();
        ui.act(Act::Turn(1), &ctx, &mut vec![]);
        ui.act(Act::Turn(1), &ctx, &mut vec![]);
        assert_eq!(ui.state.as_ref().unwrap().edits.turns, 2);
        ui.act(Act::Turn(-3), &ctx, &mut vec![]);
        assert_eq!(ui.state.as_ref().unwrap().edits.turns, 3, "wraps around");
        ui.act(Act::ToggleCrop, &ctx, &mut vec![]);
        assert_eq!(ui.state.as_ref().unwrap().edits.crop, Some(crop::FULL));
        ui.act(Act::ToggleCrop, &ctx, &mut vec![]);
        assert_eq!(ui.state.as_ref().unwrap().edits.crop, None);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn stepping_to_another_image_drops_unsaved_edits_with_a_notice() {
        let (mut ui, root, file) = ui_with_open_image("step");
        let ctx = Context::default();
        ui.state.as_mut().unwrap().edits = turned();
        ui.act(Act::Step(1), &ctx, &mut vec![]);
        assert_eq!(ui.index, 1);
        assert!(ui
            .status
            .as_ref()
            .unwrap()
            .1
            .contains("Discarded unsaved edits"));
        assert_eq!(size_on_disk(&file), (40, 20), "nothing was written");
        // Stepping past the ends does nothing.
        ui.act(Act::Step(1), &ctx, &mut vec![]);
        assert_eq!(ui.index, 1);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn files_that_are_not_tiff_png_or_jpeg_are_view_only() {
        let root = dir("viewonly");
        let rel = "images/a.gif";
        std::fs::create_dir_all(root.join("images")).unwrap();
        std::fs::write(root.join(rel), b"gif").unwrap();
        let mut ui = Editor::new();
        ui.root = root.clone();
        ui.files = vec![rel.into()];
        ui.state = Some(EditorState::new(&root, rel));
        let ctx = Context::default();
        let st = ui.state.as_mut().unwrap();
        st.edits = Edits {
            turns: 1,
            ..Edits::default()
        };
        ui.act(Act::Apply, &ctx, &mut vec![]);
        ui.act(Act::Backup, &ctx, &mut vec![]);
        let st = ui.state.as_ref().unwrap();
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
        run_op(&folder, &file, op).unwrap();
        assert_eq!(size_on_disk(&file), (10, 30));
        assert_eq!(
            size_on_disk(&folder.join(".originals/scan-01.png")),
            (30, 10)
        );
        run_op(&folder, &file, EditorOp::Restore).unwrap();
        assert_eq!(size_on_disk(&file), (30, 10));
        // An operation on a missing file reports an error instead of panicking.
        assert!(run_op(&folder, &folder.join("nope.png"), turned_apply()).is_err());
        std::fs::remove_dir_all(folder).ok();
    }

    fn turned_apply() -> EditorOp {
        EditorOp::Apply {
            edits: turned(),
            backup_first: false,
        }
    }

    /// Runs `frames` headless frames of the editor window.
    fn draw(ui: &mut Editor, ctx: &Context, frames: usize) {
        for _ in 0..frames {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1400.0, 900.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                ui.show(ctx, &mut PresetUi::default());
            });
        }
        // The preview is made on another thread: wait for it like a user would.
        for _ in 0..400 {
            if ui.state.as_ref().is_none_or(|s| !s.rendering()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                ui.show(ctx, &mut PresetUi::default());
            });
        }
    }

    #[test]
    fn the_editor_window_draws_the_picture_the_histogram_and_the_overlay() {
        let (mut ui, root, _file) = ui_with_open_image("draw");
        let ctx = Context::default();
        draw(&mut ui, &ctx, 2);
        let st = ui.state.as_ref().unwrap();
        assert!(st.tex.is_some(), "the preview texture was made");
        assert_eq!(st.histogram.total(), 200, "20 x 10 preview pixels counted");

        // Rotated, cropped, with a negative and sharpening, and zoomed: still draws.
        let st = ui.state.as_mut().unwrap();
        st.edits = Edits {
            spots: vec![],
            eight_bit: false,
            clarity: Clarity { amount: 0.3 },
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
        let st = ui.state.as_ref().unwrap();
        assert!(st.shape.is_finite() && st.shape > 0.0);
        assert!(st.dirty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_editor_window_draws_while_loading_with_an_error_and_with_the_question() {
        let (mut ui, root, _file) = ui_with_open_image("draw2");
        let ctx = Context::default();
        {
            let st = ui.state.as_mut().unwrap();
            st.base = None; // still loading: a spinner
        }
        draw(&mut ui, &ctx, 1);
        ui.state.as_mut().unwrap().error = Some("cannot read".into());
        draw(&mut ui, &ctx, 1);
        {
            let st = ui.state.as_mut().unwrap();
            st.error = None;
            st.confirm = true;
            st.busy = Some("Applying");
        }
        draw(&mut ui, &ctx, 1);
        assert!(ui.state.is_some());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_window_closes_the_editor_when_no_image_is_selected() {
        let (mut ui, root, _file) = ui_with_open_image("closed");
        let ctx = Context::default();
        ui.sync(&ctx, &root, &[], 0);
        assert!(ui.state.is_none());
        std::fs::remove_dir_all(root).ok();
    }

    /// Applies edits to a copy of a real scan: `EDIT_FILE=/scan.png cargo test -p latent-scan-ui --
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
        run_op(&root, &file, op).unwrap();
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
        let area =
            |ctx: &Context| ctx.memory(|m| m.area_rect(egui::Id::new("latent-image-editor")));
        for (w, h) in [(20u32, 10u32), (10, 20), (30, 5)] {
            let (mut ui, root, _file) = ui_with_open_image(&format!("layout-{w}x{h}"));
            ui.state.as_mut().unwrap().loaded(
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

            ui.state.as_mut().unwrap().zoom = Some(ZoomState {
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
