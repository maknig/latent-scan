//! egui UI for scanning film into a latent archive.
//!
//! Embedding contract (same shape as latent's panels): own a `ScanUi`, call `open` from a menu
//! item and `show` every frame (a floating window); react to the returned `ScanEvent`s. The
//! standalone app calls `show_main` instead, which fills the whole window: resizable controls
//! on the left, the preview on the right and an image strip of the row's scans at the bottom.

mod editor_window;
mod frames;
mod offset;
mod panels;
mod rendering;
mod settings;
mod strip;
mod targets;
mod thumbs;
mod worker;

use crop::{Crop, Grab};
use egui::{Color32, Context, TextureHandle};
use latent_image_edit::tone::{Adjust, ToneOptions};
use latent_image_edit::{BitDepth, OutputFormat};
use latent_image_edit_ui::{canvas, crop, presets, preview, view};
use latent_scan_core::{
    CancelToken, Capabilities, ColorMode, RawImage, ScanArea, ScannerInfo, Source, SourceCaps,
};
use latent_scan_profiles::{Profile, Profiles};
use preview::Look;
use settings::{SaveMode, Settings};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use targets::TargetTable;
use thumbs::Thumbs;
use worker::{Msg, Reporter, ScanRequest};

pub use latent_image_edit_ui::ACCENT;
const PREVIEW_DPI: u32 = 150;
/// Highest overview resolution offered (the whole window is 360 MB at 1200 dpi).
const PREVIEW_MAX_DPI: u32 = 1200;

mod text {
    pub const TITLE: &str = "Scan";
    pub const NO_SCANNER: &str = "No scanner found";
    pub const PREVIEW_HINT: &str = "Press Preview to see the scan bed.";
    pub const NO_TARGET: &str = "No film or enlargement rows found in the archive.";
}

/// Where a scan should be attached.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub table: String,
    pub row_id: u64,
}

pub enum ScanEvent {
    /// A scan was written into an archive; `rel` is archive-relative with forward slashes.
    Saved {
        table: String,
        row_id: u64,
        rel: String,
    },
    /// A scan was written as a plain file into the save folder.
    SavedFile { path: PathBuf },
}

struct Busy {
    cancel: CancelToken,
    progress: f32,
    what: String,
}

struct PreviewState {
    /// The overview as scanned (display-sized, 16-bit); `tex` is rendered from it.
    raw: Arc<RawImage>,
    tex: TextureHandle,
    /// Counts overviews (see [`rendering::Job`]).
    serial: u64,
    /// What the render thread is working on.
    requested: Option<rendering::Key>,
    /// What `tex` currently shows: the look and the frame the tone was estimated from.
    shown: Option<(Look, usize, Crop)>,
    /// Window (mm) the preview covers; crop fractions refer to it.
    area: ScanArea,
    source: Source,
    dpi: u32,
}

pub struct ScanUi {
    pub open: bool,
    /// Where scans go (standalone app: chosen in the UI; embedded: always the host's archive).
    mode: SaveMode,
    /// Folder for plain files.
    folder: PathBuf,
    /// Root of the latent archive, once chosen.
    archive: Option<PathBuf>,
    /// The standalone app may switch between folder and archive and browse for both.
    standalone: bool,
    write_data: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,

    devices: Vec<(ScannerInfo, Capabilities)>,
    device: usize,
    profiles: Profiles,
    profile: usize,
    /// Name under which "Save profile" stores the current frames.
    profile_name: String,
    dpi: u32,
    rotate: u16,
    color: ColorMode,
    negative: bool,
    /// Manual image adjustments (sliders) of each frame, same length as `frames`; neutral
    /// leaves a scan untouched.
    adjusts: Vec<Adjust>,
    format: OutputFormat,
    /// Resolution of the overview scan; `None` = [`PREVIEW_DPI`] or the next one the source offers.
    preview_dpi: Option<u32>,
    /// Bits per sample of the saved files (the scan itself is always taken at 16 bit).
    depth: BitDepth,
    /// Re-fit the frames on a pre-scan of each frame right before scanning it.
    adjust_before_scan: bool,
    /// Settings remembered between sessions (auto-adjust values).
    settings: Settings,
    /// Slider presets and the preset bar's name field (shared by the scan panel and the editor).
    presets: presets::PresetUi,
    settings_dirty: bool,
    /// Batch naming: `<prefix>-<number>`, numbering from `start`.
    prefix: String,
    start: u32,

    targets: Vec<TargetTable>,
    table: usize,
    row: Option<u64>,

    busy: Option<Busy>,
    preview: Option<PreviewState>,
    renderer: Option<rendering::PreviewRenderer>,
    preview_serial: u64,
    /// Frames of the holder on the preview, in scan order; `active` is the one being edited.
    frames: Vec<Crop>,
    active: usize,
    grab: Option<Grab>,
    /// Zoom of the preview; `None` shows the whole picture fitted to the pane.
    zoom: Option<view::ZoomState>,
    status: Option<(bool, String)>,

    /// Thumbnails for the image strip (main-window mode only), created on first use.
    thumbs: Option<Thumbs>,
    strip: strip::StripState,
    /// The image open in the editor (double-click on a tile of the strip).
    editor: editor_window::Editor,
}

impl ScanUi {
    /// For a host: scans always go into this latent archive.
    pub fn new(archive: PathBuf) -> Self {
        Self::build(Some(archive), false)
    }

    /// For the standalone app: starts with the destination used last time (a plain folder by
    /// default); `archive` (e.g. from the command line) selects that latent archive instead.
    pub fn standalone(archive: Option<PathBuf>) -> Self {
        Self::build(archive, true)
    }

    fn build(archive: Option<PathBuf>, standalone: bool) -> Self {
        let (settings, warning) = Settings::load();
        let mut ui = Self::with_settings(archive, standalone, settings);
        ui.status = ui.status.take().or(warning.map(|w| (true, w)));
        ui
    }

    fn with_settings(archive: Option<PathBuf>, standalone: bool, settings: Settings) -> Self {
        let (tx, rx) = channel();
        let (profiles, profile_warning) = Profiles::load();
        let (presets, presets_warning) = match latent_scan_profiles::config_dir() {
            Some(dir) => presets::Presets::load_from(&dir.join("presets.toml")),
            None => (presets::Presets::default(), None),
        };
        let dpi = profiles.all().first().map_or(300, |p| p.dpi);
        let profile_name = profiles
            .all()
            .first()
            .map_or(String::new(), |p| p.name.clone());
        let (mode, archive) = if standalone && archive.is_none() {
            (settings.save.mode, settings.save.archive.clone())
        } else {
            (SaveMode::Archive, archive)
        };
        Self {
            open: false,
            mode,
            folder: settings.save.folder_or_default(),
            archive,
            standalone,
            write_data: true,
            tx,
            rx,
            devices: vec![],
            device: 0,
            profiles,
            profile: 0,
            profile_name,
            dpi,
            rotate: 0,
            color: ColorMode::Color,
            negative: false,
            adjusts: vec![Adjust::default()],
            format: OutputFormat::Tiff,
            preview_dpi: settings.preview_dpi,
            depth: if settings.save.eight_bit {
                BitDepth::Eight
            } else {
                BitDepth::Sixteen
            },
            adjust_before_scan: false,
            settings,
            presets: presets::PresetUi::new(presets),
            settings_dirty: false,
            prefix: "frame".into(),
            start: 1,
            targets: vec![],
            table: 0,
            row: None,
            busy: None,
            preview: None,
            frames: vec![crop::FULL],
            active: 0,
            grab: None,
            zoom: None,
            status: profile_warning.or(presets_warning).map(|w| (true, w)),
            thumbs: None,
            strip: strip::StripState::default(),
            editor: editor_window::Editor::new(),
            renderer: None,
            preview_serial: 0,
        }
    }

    /// For hosts that keep the archive data in memory and add the image themselves from
    /// `ScanEvent::Saved`: only write the image file, leave `latent.data.json` alone.
    pub fn write_data(mut self, write: bool) -> Self {
        self.write_data = write;
        self
    }

    /// Shows the UI, looks for scanners and reloads the archive's rows.
    pub fn open(&mut self, ctx: &Context, target: Option<Target>) {
        self.open = true;
        self.reload_targets(target);
        if self.busy.is_none() {
            self.start_discover(ctx);
        }
    }

    fn reload_targets(&mut self, want: Option<Target>) {
        let loaded = match (self.mode, &self.archive) {
            (SaveMode::Archive, Some(root)) => targets::load(root),
            _ => Ok(vec![]),
        };
        match loaded {
            Ok(t) => self.targets = t,
            Err(e) => {
                self.targets.clear();
                self.status = Some((true, e));
            }
        }
        self.table = 0;
        self.row = self
            .targets
            .first()
            .and_then(|t| t.rows.first())
            .map(|r| r.0);
        if let Some(w) = want {
            if let Some(i) = self.targets.iter().position(|t| t.name == w.table) {
                self.table = i;
                self.row = Some(w.row_id);
            }
        }
        self.strip.dirty = true;
    }

    /// Floating window, for embedding in a host. Returns the scans saved since the last call.
    pub fn show(&mut self, ctx: &Context) -> Vec<ScanEvent> {
        let events = self.poll(ctx);
        if self.open {
            self.refresh_preview(ctx);
            let mut open = true;
            egui::Window::new(text::TITLE)
                .open(&mut open)
                .default_size([920.0, 620.0])
                .show(ctx, |ui| self.window_ui(ui));
            if !open {
                self.close();
            }
        }
        events
    }

    /// The whole window of the standalone app: resizable controls (left) and preview (right),
    /// image strip (bottom). Returns the scans saved since the last call.
    pub fn show_main(&mut self, ctx: &Context) -> Vec<ScanEvent> {
        latent_image_edit_ui::trace::timed("scan window frame", || self.show_main_frame(ctx))
    }

    fn show_main_frame(&mut self, ctx: &Context) -> Vec<ScanEvent> {
        self.open = true;
        let events = self.poll(ctx);
        if self.thumbs.is_none() {
            self.thumbs = Some(Thumbs::new(ctx, self.thumb_cache_dir()));
        }
        let cache = self.thumb_cache_dir();
        if let Some(t) = &mut self.thumbs {
            t.set_cache_dir(cache);
            t.poll(ctx);
        }
        self.refresh_preview(ctx);

        egui::TopBottomPanel::bottom("scan-strip")
            .resizable(true)
            .default_height(150.0)
            .min_height(60.0)
            .show(ctx, |ui| self.strip_ui(ui));
        egui::SidePanel::left("scan-controls")
            .resizable(true)
            .default_width(320.0)
            .width_range(240.0..=560.0)
            .show(ctx, |ui| self.controls_panel(ui));
        egui::CentralPanel::default().show(ctx, |ui| self.preview_pane(ui));
        self.editor_window(ctx);
        self.save_settings_when_idle(ctx);
        events
    }

    fn close(&mut self) {
        self.open = false;
        if let Some(b) = &self.busy {
            b.cancel.cancel();
        }
    }

    fn poll(&mut self, ctx: &Context) -> Vec<ScanEvent> {
        let mut events = vec![];
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Progress(p) => {
                    if let Some(b) = &mut self.busy {
                        b.progress = p;
                    }
                }
                Msg::Devices(d) => {
                    self.busy = None;
                    self.device = 0;
                    self.status = d.is_empty().then(|| (true, text::NO_SCANNER.to_string()));
                    self.devices = d;
                    self.keep_profile_for_scanner();
                    self.select_profile(self.profile);
                }
                Msg::Preview {
                    raw,
                    area,
                    source,
                    dpi,
                } => {
                    self.busy = None;
                    let tex = ctx.load_texture(
                        "scan-preview",
                        preview::to_color_image(&raw, u32::MAX),
                        Default::default(),
                    );
                    self.preview_serial += 1;
                    self.preview = Some(PreviewState {
                        raw: Arc::new(raw),
                        tex,
                        serial: self.preview_serial,
                        requested: None,
                        shown: None,
                        area,
                        source,
                        dpi,
                    });
                    self.place_frames_from_profile();
                    self.status = None;
                }
                Msg::Fitted(fits) => {
                    self.busy = None;
                    self.apply_fits(fits);
                }
                Msg::Picked { kind, path } => {
                    if let Some(path) = path {
                        self.picked(kind, path);
                    }
                }
                Msg::Saved(saved) => {
                    self.start += 1;
                    self.strip.dirty = true;
                    match saved {
                        worker::SavedTo::Archive { table, row_id, rel } => {
                            self.status = Some((false, format!("Saved {rel}")));
                            events.push(ScanEvent::Saved { table, row_id, rel });
                        }
                        worker::SavedTo::File { path } => {
                            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                            self.status =
                                Some((false, format!("Saved {}", name.unwrap_or_default())));
                            events.push(ScanEvent::SavedFile { path });
                        }
                    }
                }
                Msg::Checked {
                    text,
                    warn,
                    error,
                    correlation,
                } => {
                    self.status = Some((warn, text));
                    if let (Some(e), Some(key)) = (error, self.offset_key()) {
                        if self.settings.offset.learn(key, e, correlation) {
                            self.settings_dirty = true;
                        }
                    }
                }
                Msg::Adjusted {
                    index,
                    shift_mm,
                    confidence,
                    moved,
                } => {
                    self.status = Some((
                        false,
                        if moved {
                            format!(
                                "Frame {}: adjusted by {:+.1}, {:+.1} mm (confidence {confidence:.2})",
                                index + 1,
                                shift_mm.0,
                                shift_mm.1
                            )
                        } else {
                            format!(
                                "Frame {}: kept as saved (confidence {confidence:.2})",
                                index + 1
                            )
                        },
                    ));
                }
                Msg::Frame { index, total } => {
                    if let Some(b) = &mut self.busy {
                        b.what = format!("Scanning {} of {total}", index + 1);
                    }
                }
                Msg::BatchDone => {
                    self.busy = None;
                }
                Msg::Failed(e) => {
                    self.busy = None;
                    self.status = Some((true, e));
                }
                Msg::Cancelled => {
                    self.busy = None;
                    self.status = Some((false, "Cancelled".into()));
                }
            }
        }
        events
    }

    fn reporter(&self, ctx: &Context) -> Reporter {
        Reporter {
            tx: self.tx.clone(),
            ctx: ctx.clone(),
        }
    }

    fn start_discover(&mut self, ctx: &Context) {
        self.busy = Some(Busy {
            cancel: CancelToken::default(),
            progress: 0.0,
            what: "Looking for scanners".into(),
        });
        worker::discover(self.reporter(ctx));
    }

    /// Moves the selection to the first profile of the current scanner when the selected one
    /// belongs to another scanner.
    fn keep_profile_for_scanner(&mut self) {
        let Some((device, _)) = self.devices.get(self.device) else {
            return;
        };
        let fits = self
            .profiles
            .all()
            .get(self.profile)
            .is_some_and(|p| p.fits(device));
        if fits {
            return;
        }
        let first = self.profiles.for_scanner(device).next().map(|(i, _)| i);
        if let Some(i) = first {
            self.select_profile(i);
        }
    }

    fn select_profile(&mut self, idx: usize) {
        self.profile = idx.min(self.profiles.all().len().saturating_sub(1));
        let Some(p) = self.profiles.all().get(self.profile) else {
            return;
        };
        self.profile_name = p.name.clone();
        self.color = p.color;
        self.negative = p.negative;
        self.rotate = p.rotate;
        // The profile's dpi may not exist on this scanner or source.
        self.dpi = self
            .source_caps(p.source)
            .and_then(|c| c.resolution_at_least(p.dpi))
            .unwrap_or(p.dpi);
        // A preview of the other source shows a different window.
        if self.preview.as_ref().is_some_and(|v| v.source != p.source) {
            self.preview = None;
        }
        self.place_frames_from_profile();
    }

    /// Moves the frames to the selected profile's areas within the current preview.
    fn place_frames_from_profile(&mut self) {
        self.active = 0;
        // Another set of frames (or a new preview) starts with the whole picture in view.
        self.zoom = None;
        match (&self.preview, self.profiles.all().get(self.profile)) {
            (Some(v), Some(p)) => {
                self.frames = p
                    .frame_areas()
                    .into_iter()
                    .map(|a| frames::mm_to_crop(v.area, a))
                    .collect();
            }
            _ => self.frames = vec![crop::FULL],
        }
        // Adjustments belong to the frames of one preview; new frames start neutral.
        self.adjusts = vec![Adjust::default(); self.frames.len()];
    }

    /// The image adjustments of the active frame.
    fn active_adjust(&self) -> Adjust {
        self.adjusts.get(self.active).copied().unwrap_or_default()
    }

    fn active_adjust_mut(&mut self) -> &mut Adjust {
        self.adjusts
            .resize(self.frames.len().max(1), Adjust::default());
        let i = self.active.min(self.adjusts.len() - 1);
        &mut self.adjusts[i]
    }

    /// Measures black and white point of frame `idx` on the overview and writes them into that
    /// frame's sliders (nothing else changes). Returns the values.
    fn auto_levels_for(&mut self, idx: usize) -> Option<(f32, f32)> {
        let v = self.preview.as_ref()?;
        let reference = preview::region(&v.raw, *self.frames.get(idx)?)?;
        let a = Adjust::auto_levels(&reference, self.negative);
        self.adjusts.resize(self.frames.len(), Adjust::default());
        let slot = self.adjusts.get_mut(idx)?;
        slot.black = a.black;
        slot.white = a.white;
        Some((a.black, a.white))
    }

    fn auto_active(&mut self) {
        let idx = self.active;
        self.status = Some(match self.auto_levels_for(idx) {
            Some((b, w)) => (
                false,
                format!(
                    "Auto frame {}: black {:.1} %, white {:.1} %",
                    idx + 1,
                    b * 100.0,
                    w * 100.0
                ),
            ),
            None => (true, "Auto needs a preview".into()),
        });
    }

    fn auto_all(&mut self) {
        let n = self.frames.len();
        let done = (0..n)
            .filter(|&i| self.auto_levels_for(i).is_some())
            .count();
        self.status = Some(if done == 0 {
            (true, "Auto needs a preview".into())
        } else {
            (
                false,
                format!("Auto: black and white point of {done} frame(s)"),
            )
        });
    }

    fn reset_adjust(&mut self) {
        *self.active_adjust_mut() = Adjust::default();
    }

    /// Gives every frame the active frame's adjustments.
    fn copy_adjust_to_all(&mut self) {
        let a = self.active_adjust();
        self.adjusts = vec![a; self.frames.len()];
    }

    fn source_caps(&self, source: Source) -> Option<&SourceCaps> {
        self.devices.get(self.device)?.1.get(source)
    }

    /// Resolutions offered for the overview scan: the source's own between 150 and 1200 dpi.
    fn preview_resolutions(&self) -> Vec<u32> {
        let mut r: Vec<u32> = self
            .resolutions()
            .into_iter()
            .filter(|d| (PREVIEW_DPI..=PREVIEW_MAX_DPI).contains(d))
            .collect();
        r.sort_unstable();
        r
    }

    /// The resolution the overview is scanned at for `source`: the chosen one, else the lowest of
    /// at least 150 dpi, in both cases one the source really offers.
    fn preview_dpi_for(&self, source: Source) -> u32 {
        let wanted = self.preview_dpi.unwrap_or(PREVIEW_DPI);
        self.source_caps(source)
            .and_then(|c| c.resolution_at_least(wanted))
            .unwrap_or(wanted)
    }

    fn resolutions(&self) -> Vec<u32> {
        let source = self.profiles.all().get(self.profile).map(|p| p.source);
        source
            .and_then(|s| self.source_caps(s))
            .map(|c| c.resolutions.clone())
            .unwrap_or_default()
    }

    /// Areas (mm) of the frames to scan: the ones on the preview, else the profile's own.
    fn frame_areas(&self) -> Vec<ScanArea> {
        match &self.preview {
            Some(v) => self
                .frames
                .iter()
                .map(|c| frames::crop_to_mm(v.area, *c))
                .collect(),
            None => self
                .profiles
                .all()
                .get(self.profile)
                .map(Profile::frame_areas)
                .unwrap_or_default(),
        }
    }

    fn detect_options(&self) -> latent_scan_detect::Options {
        self.settings.detect.options()
    }

    /// Starts fitting the frames to the film on the current overview (the work runs on a thread;
    /// [`ScanUi::apply_fits`] moves the frames when it is done).
    fn auto_adjust(&mut self, ctx: &Context) {
        let Some(v) = &self.preview else {
            return;
        };
        let presets: Vec<ScanArea> = self
            .frames
            .iter()
            .map(|c| frames::crop_to_mm(v.area, *c))
            .collect();
        self.busy = Some(Busy {
            cancel: CancelToken::default(),
            progress: 0.0,
            what: "Adjusting frames".into(),
        });
        worker::fit_frames(
            self.reporter(ctx),
            v.raw.clone(),
            v.area,
            presets,
            self.detect_options(),
        );
    }

    /// Moves the frames to where the fit put them and reports what changed.
    fn apply_fits(&mut self, fits: Vec<latent_scan_detect::Fit>) {
        let Some(v) = &self.preview else {
            return;
        };
        let opts = self.detect_options();
        self.frames = fits
            .iter()
            .map(|f| frames::mm_to_crop(v.area, f.area))
            .collect();

        let moved: Vec<_> = fits.iter().filter(|f| f.moved).collect();
        let kept: Vec<String> = fits
            .iter()
            .enumerate()
            .filter(|(_, f)| f.confidence < opts.min_confidence)
            .map(|(i, _)| (i + 1).to_string())
            .collect();
        let n = moved.len().max(1) as f64;
        let (dx, dy) = moved
            .iter()
            .fold((0.0, 0.0), |a, f| (a.0 + f.shift_mm.0, a.1 + f.shift_mm.1));
        let mut msg = format!(
            "Adjusted {} of {} frames (mean shift {:+.1}, {:+.1} mm)",
            moved.len(),
            fits.len(),
            dx / n,
            dy / n
        );
        if !kept.is_empty() {
            msg.push_str(&format!(
                "; weak edges, frame {} follows the strip or stays",
                kept.join(", ")
            ));
        }
        self.status = Some((false, msg));
    }

    /// What the learned offset belongs to: the source and the preview and scan resolutions.
    fn offset_key(&self) -> Option<offset::Key> {
        let v = self.preview.as_ref()?;
        Some(offset::Key {
            source: v.source,
            preview_dpi: v.dpi,
            scan_dpi: self.dpi,
        })
    }

    /// Learned plus manual offset (mm) added to every full-resolution scan.
    fn scan_offset(&self) -> (f64, f64) {
        self.settings.offset.total(self.offset_key())
    }

    fn tone(&self) -> ToneOptions {
        ToneOptions {
            negative: self.negative,
            auto_levels: false,
            adjust: self.active_adjust(),
        }
    }

    /// Asks the render thread for a new preview texture when the look or the reference frame
    /// changed (the old texture stays until it is ready) and shows the ones that are done.
    /// Dragging a frame is skipped so the toning is not redone on every pointer move.
    fn refresh_preview(&mut self, ctx: &Context) {
        if let (Some(r), Some(v)) = (&self.renderer, &mut self.preview) {
            for done in r.finished() {
                if done.serial == v.serial && v.requested == Some(done.key) {
                    v.tex.set(done.image, Default::default());
                    v.shown = Some(done.key);
                    v.requested = None;
                }
            }
        }
        if self.grab.is_some() {
            return;
        }
        let look = Look {
            tone: self.tone(),
            gray: self.color == ColorMode::Gray,
            rotate: self.rotate,
        };
        let Some(&frame) = self.frames.get(self.active) else {
            return;
        };
        let Some(v) = &mut self.preview else {
            return;
        };
        let key = (look, self.active, frame);
        if v.shown == Some(key) || v.requested == Some(key) {
            return;
        }
        let renderer = self
            .renderer
            .get_or_insert_with(|| rendering::PreviewRenderer::new(ctx));
        let job = rendering::Job {
            serial: v.serial,
            key,
            raw: v.raw.clone(),
        };
        if renderer.request(job) {
            v.requested = Some(key);
        }
    }

    /// Writes the remembered settings once no slider is being dragged any more.
    fn save_settings_when_idle(&mut self, ctx: &Context) {
        if !self.settings_dirty || ctx.input(|i| i.pointer.any_down()) {
            return;
        }
        self.settings_dirty = false;
        if let Err(e) = self.settings.save() {
            self.status = Some((true, format!("cannot save settings: {e}")));
        }
    }

    /// "Zoom to frame": zooms onto the active frame and follows it when another one is
    /// selected; pressed again it shows the whole picture.
    fn toggle_zoom_to_frame(&mut self) {
        self.zoom = if self.zoom.is_some() {
            None
        } else {
            Some(view::start_following())
        };
    }

    /// Adds a frame next to the active one and selects it.
    fn add_frame(&mut self) {
        let n = crop::next_frame(&self.frames, self.active);
        // Neighbouring frames of a roll usually look alike: start from the active values.
        let a = self.active_adjust();
        self.frames.push(n);
        self.adjusts
            .resize(self.frames.len() - 1, Adjust::default());
        self.adjusts.push(a);
        self.active = self.frames.len() - 1;
    }

    fn remove_frame(&mut self) {
        if self.frames.len() > 1 {
            self.adjusts.resize(self.frames.len(), Adjust::default());
            self.frames.remove(self.active);
            self.adjusts.remove(self.active);
            self.active = self.active.min(self.frames.len() - 1);
        }
    }

    /// Stores the frames as the profile named `profile_name` in the user's profile file.
    fn save_profile(&mut self) {
        let (Some(v), Some(base)) = (&self.preview, self.profiles.all().get(self.profile)) else {
            return;
        };
        let name = self.profile_name.trim().to_string();
        if name.is_empty() {
            return;
        }
        let frames: Vec<ScanArea> = self
            .frames
            .iter()
            .map(|c| frames::crop_to_mm(v.area, *c))
            .collect();
        let profile = Profile {
            name: name.clone(),
            // A profile is calibrated on the scanner in use and stays with it.
            scanner: base
                .scanner
                .clone()
                .or_else(|| self.devices.get(self.device).map(|(d, _)| d.model.clone())),
            area: frames[0],
            frames,
            dpi: self.dpi,
            color: self.color,
            negative: self.negative,
            rotate: self.rotate,
            ..base.clone()
        };
        self.status = Some(match self.profiles.save_user(profile) {
            Ok(()) => {
                if let Some(i) = self.profiles.all().iter().position(|p| p.name == name) {
                    self.profile = i;
                }
                (false, format!("Saved profile {name}"))
            }
            Err(e) => (true, e.to_string()),
        });
    }

    /// The folder or archive root scans currently go to (empty when no archive is chosen yet).
    fn root(&self) -> PathBuf {
        match self.mode {
            SaveMode::Folder => self.folder.clone(),
            SaveMode::Archive => self.archive.clone().unwrap_or_default(),
        }
    }

    /// Thumbnail cache for the strip: an archive keeps it inside (shared with latent), a plain
    /// folder gets one in the user's cache directory so the folder stays clean.
    fn thumb_cache_dir(&self) -> PathBuf {
        match (self.mode, &self.archive) {
            (SaveMode::Archive, Some(root)) => root.join(".thumbs"),
            _ => user_cache_dir().join("latent-scan/thumbs"),
        }
    }

    /// The destination of the next scan, or `None` while something is still to be chosen (an
    /// archive, a film).
    fn dest(&self) -> Option<worker::Dest> {
        match self.mode {
            SaveMode::Folder => Some(worker::Dest::Folder {
                dir: self.folder.clone(),
            }),
            SaveMode::Archive => {
                let table = self.targets.get(self.table)?;
                Some(worker::Dest::Archive {
                    root: self.archive.clone()?,
                    table: table.name.clone(),
                    row_id: self.row?,
                    write_data: self.write_data,
                })
            }
        }
    }

    /// Remembers the destination for the next session.
    fn remember_destination(&mut self) {
        self.settings.save = settings::SaveSettings {
            mode: self.mode,
            folder: Some(self.folder.clone()),
            archive: self.archive.clone(),
            eight_bit: self.depth == BitDepth::Eight,
        };
        self.settings_dirty = true;
    }

    /// Switches between plain folder and latent archive (standalone app). Choosing the archive
    /// without one chosen yet opens the browser right away.
    fn set_mode(&mut self, mode: SaveMode, ctx: &Context) {
        if !self.standalone || self.mode == mode {
            return;
        }
        self.mode = mode;
        self.strip.dirty = true;
        if mode == SaveMode::Archive && self.archive.is_none() {
            self.browse(worker::PickKind::ArchiveRoot, ctx);
        }
        self.reload_targets(None);
        self.remember_destination();
    }

    /// Opens the native folder dialog on a thread; the answer arrives as `Msg::Picked`.
    fn browse(&mut self, kind: worker::PickKind, ctx: &Context) {
        let start = match kind {
            worker::PickKind::SaveFolder => Some(self.folder.clone()),
            worker::PickKind::ArchiveRoot => self.archive.clone(),
        };
        worker::pick_folder(self.reporter(ctx), kind, start);
    }

    fn picked(&mut self, kind: worker::PickKind, path: PathBuf) {
        match kind {
            worker::PickKind::SaveFolder => {
                self.folder = path;
                self.mode = SaveMode::Folder;
            }
            worker::PickKind::ArchiveRoot => {
                if !latent_scan_archive::is_archive(&path) {
                    self.status = Some((
                        true,
                        format!(
                            "{} is not a latent archive (no {} inside)",
                            path.display(),
                            latent_scan_archive::DATA_FILE
                        ),
                    ));
                    return;
                }
                self.archive = Some(path);
                self.mode = SaveMode::Archive;
                self.reload_targets(None);
            }
        }
        self.strip.dirty = true;
        self.remember_destination();
    }

    fn start_preview(&mut self, ctx: &Context) {
        let Some((device, _)) = self.devices.get(self.device).cloned() else {
            return;
        };
        // Overview of the source's whole window, so the frame can be placed (and calibrated)
        // anywhere. The dpi must be one the source offers: the transparency unit starts at 300.
        let mut job = self.profiles.all()[self.profile].job();
        let caps = self.source_caps(job.source);
        job.dpi = self.preview_dpi_for(job.source);
        if let Some(window) = caps.and_then(|c| c.max_area) {
            job.area = window;
        }
        job.color = ColorMode::Color;
        job.bit_depth = 8;
        job.preview = true;
        let cancel = CancelToken::default();
        self.busy = Some(Busy {
            cancel: cancel.clone(),
            progress: 0.0,
            what: "Preview".into(),
        });
        worker::preview(self.reporter(ctx), device, job, cancel);
    }

    /// Scans every frame, or with `only_active` just the selected one (named with the next number).
    fn start_scan(&mut self, ctx: &Context, only_active: bool) {
        let Some((device, _)) = self.devices.get(self.device).cloned() else {
            return;
        };
        let Some(dest) = self.dest() else {
            return;
        };
        let mut job = self.profiles.all()[self.profile].job();
        job.dpi = self.dpi;
        job.color = self.color;
        let mut areas: Vec<(usize, ScanArea)> =
            self.frame_areas().into_iter().enumerate().collect();
        if only_active {
            let keep = self.active.min(areas.len().saturating_sub(1));
            areas.retain(|(i, _)| *i == keep);
        }
        // Each frame is scanned with its own adjustments. Without a preview the frames come
        // from the profile and share the first set.
        let first = self.active_adjust();
        let frames = areas
            .into_iter()
            .zip(self.start..)
            .map(|((i, area), n)| worker::Frame {
                area,
                stem: latent_scan_archive::naming::frame_stem(&self.prefix, n),
                adjust: self.adjusts.get(i).copied().unwrap_or(first),
            })
            .collect();
        let job_source = job.source;
        let req = ScanRequest {
            device,
            job,
            frames,
            rotate: self.rotate,
            tone: self.tone(),
            format: self.format,
            depth: self.depth,
            dest,
            thumb_cache: self.thumb_cache_dir(),
            adjust: self.adjust_before_scan.then(|| self.detect_options()),
            source_caps: self.source_caps(job_source).cloned(),
            offset: self.scan_offset(),
            overview: self.preview.as_ref().map(|v| worker::Overview {
                raw: v.raw.clone(),
                area: v.area,
            }),
        };
        let cancel = CancelToken::default();
        self.busy = Some(Busy {
            cancel: cancel.clone(),
            progress: 0.0,
            what: "Scanning".into(),
        });
        worker::scan(self.reporter(ctx), req, cancel);
    }
}

/// `$XDG_CACHE_HOME`, else `~/.cache`.
fn user_cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
}

/// Dark theme matching latent's (`apply_theme` in latent-ui), for the standalone app.
pub fn apply_theme(ctx: &Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::from_gray(30);
    v.window_fill = Color32::from_gray(36);
    v.extreme_bg_color = Color32::from_gray(22);
    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 5.0);
        s.spacing.button_padding = egui::vec2(8.0, 3.0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_moves_to_a_profile_of_the_chosen_scanner() {
        let mut ui = caps_ui();
        let info = |model: &str| ScannerInfo {
            id: model.into(),
            vendor: "V".into(),
            model: model.into(),
        };
        let caps = ui.devices[0].1.clone();
        ui.devices = vec![
            (
                info("Canoscan 9000F multi-function peripheral"),
                caps.clone(),
            ),
            (info("GT-X980"), caps),
        ];
        ui.device = 1;
        ui.profile = ui
            .profiles
            .all()
            .iter()
            .position(|p| p.name == "35mm-strip")
            .unwrap();
        ui.keep_profile_for_scanner();
        let epson = ui.devices[1].0.clone();
        let p = &ui.profiles.all()[ui.profile];
        assert!(p.fits(&epson) && p.name != "35mm-strip", "{}", p.name);
        // A profile that already fits is kept.
        ui.device = 0;
        ui.profile = ui
            .profiles
            .all()
            .iter()
            .position(|p| p.name == "35mm-strip")
            .unwrap();
        ui.keep_profile_for_scanner();
        assert_eq!(ui.profiles.all()[ui.profile].name, "35mm-strip");
    }

    fn caps_ui() -> ScanUi {
        let caps = |source, resolutions: &[u32]| latent_scan_core::SourceCaps {
            source,
            resolutions: resolutions.to_vec(),
            modes: vec![],
            depths: vec![],
            quick_preview: false,
            max_area: None,
        };
        let mut ui = ScanUi::new(std::env::temp_dir());
        // `new` reads the user's settings.toml; the tests must not depend on it.
        ui.preview_dpi = None;
        ui.devices = vec![(
            ScannerInfo {
                id: "d".into(),
                vendor: "v".into(),
                model: "m".into(),
            },
            Capabilities {
                sources: vec![
                    caps(Source::Flatbed, &[75, 150, 300, 600, 1200, 2400, 4800]),
                    caps(Source::Transparency, &[300, 600, 1200, 2400, 4800, 9600]),
                ],
            },
        )];
        ui
    }

    #[test]
    fn the_preview_resolution_defaults_to_the_lowest_offered_from_150_dpi() {
        let ui = caps_ui();
        assert_eq!(ui.preview_dpi_for(Source::Flatbed), 150);
        assert_eq!(ui.preview_dpi_for(Source::Transparency), 300);
    }

    #[test]
    fn a_chosen_preview_resolution_is_rounded_up_to_one_the_source_offers() {
        let mut ui = caps_ui();
        ui.preview_dpi = Some(600);
        assert_eq!(ui.preview_dpi_for(Source::Transparency), 600);
        ui.preview_dpi = Some(500);
        assert_eq!(ui.preview_dpi_for(Source::Flatbed), 600);
        assert_eq!(ui.preview_dpi_for(Source::Transparency), 600);
        // Nothing offered above: the highest one.
        ui.preview_dpi = Some(20000);
        assert_eq!(ui.preview_dpi_for(Source::Transparency), 9600);
    }

    #[test]
    fn only_moderate_resolutions_are_offered_for_the_preview() {
        let mut ui = caps_ui();
        ui.profile = ui
            .profiles
            .all()
            .iter()
            .position(|p| p.source == Source::Transparency)
            .unwrap();
        assert_eq!(ui.preview_resolutions(), vec![300, 600, 1200]);
    }

    #[test]
    fn the_stored_overview_grows_only_for_higher_preview_resolutions() {
        use worker::preview_max_side;
        assert_eq!(preview_max_side(150), 2048);
        assert_eq!(preview_max_side(300), 2048);
        assert_eq!(preview_max_side(600), 4096);
        assert_eq!(preview_max_side(1200), 4096);
    }

    fn ui() -> ScanUi {
        ScanUi::new(std::env::temp_dir())
    }

    fn exposure(ev: f32) -> Adjust {
        Adjust {
            exposure_ev: ev,
            ..Adjust::default()
        }
    }

    /// A ui with three frames carrying exposure 0, 1 and 2.
    fn three_frames() -> ScanUi {
        let mut ui = ui();
        ui.frames = vec![crop::FULL, [0.0, 0.0, 0.5, 0.5], [0.5, 0.5, 1.0, 1.0]];
        ui.adjusts = vec![exposure(0.0), exposure(1.0), exposure(2.0)];
        ui
    }

    fn evs(ui: &ScanUi) -> Vec<f32> {
        ui.adjusts.iter().map(|a| a.exposure_ev).collect()
    }

    /// A ui whose preview shows a left half with a dark range and a right half with a bright
    /// range, each covered by one frame.
    fn ui_with_preview() -> ScanUi {
        let mut data = Vec::new();
        for _y in 0..100 {
            for x in 0..200u32 {
                let v = if x < 100 {
                    10_000 + x * 200
                } else {
                    30_000 + (x - 100) * 300
                } as u16;
                data.extend([v, v, v]);
            }
        }
        let raw = RawImage::new(200, 100, 3, data).unwrap();
        let tex = egui::Context::default().load_texture(
            "test",
            egui::ColorImage::from_rgb([1, 1], &[0, 0, 0]),
            Default::default(),
        );
        let mut ui = ui();
        ui.preview = Some(PreviewState {
            raw: Arc::new(raw),
            tex,
            serial: 0,
            requested: None,
            shown: None,
            area: ScanArea {
                x: 0.0,
                y: 0.0,
                width: 200.0,
                height: 100.0,
            },
            source: Source::Transparency,
            dpi: 300,
        });
        ui.frames = vec![[0.0, 0.0, 0.5, 1.0], [0.5, 0.0, 1.0, 1.0]];
        ui.adjusts = vec![Adjust::default(); 2];
        ui
    }

    #[test]
    fn the_preview_texture_is_rendered_on_a_thread_and_the_newest_look_wins() {
        let mut ui = ui_with_preview();
        let ctx = egui::Context::default();
        for step in 1..=6 {
            ui.adjusts[0].exposure_ev = step as f32 * 0.1;
            ui.refresh_preview(&ctx);
        }
        for _ in 0..400 {
            ui.refresh_preview(&ctx);
            if ui.preview.as_ref().unwrap().requested.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let v = ui.preview.as_ref().unwrap();
        assert!(v.requested.is_none(), "the render finished");
        let (look, active, frame) = v.shown.expect("a preview was shown");
        assert_eq!((active, frame), (0, ui.frames[0]));
        assert_eq!(
            look.tone.adjust.exposure_ev, 0.6,
            "the newest look is on screen"
        );
    }

    #[test]
    fn fitting_the_frames_runs_on_a_thread_and_reports_back() {
        let mut ui = ui_with_preview();
        let ctx = egui::Context::default();
        ui.auto_adjust(&ctx);
        assert!(ui.busy.is_some(), "the buttons wait for the result");
        let mut waited = 0;
        while ui.busy.is_some() && waited < 400 {
            ui.poll(&ctx);
            std::thread::sleep(std::time::Duration::from_millis(5));
            waited += 1;
        }
        assert!(ui.busy.is_none(), "the fit came back");
        assert!(ui
            .status
            .as_ref()
            .is_some_and(|s| s.1.starts_with("Adjusted")));
    }

    #[test]
    fn checks_teach_the_offset_for_the_current_resolutions_and_weak_ones_do_not() {
        let mut ui = ui_with_preview();
        ui.dpi = 2400;
        ui.settings.offset = Default::default();
        let ctx = egui::Context::default();
        let send = |ui: &ScanUi, error: (f64, f64), correlation: f32| {
            ui.tx
                .send(Msg::Checked {
                    text: "checked".into(),
                    warn: false,
                    error: Some(error),
                    correlation,
                })
                .unwrap();
        };
        send(&ui, (0.08, -0.72), 0.9);
        send(&ui, (0.0, -3.5), 0.99);
        send(&ui, (0.0, -0.4), 0.3);
        ui.poll(&ctx);
        let (x, y) = ui.scan_offset();
        assert!(
            (x + 0.08).abs() < 1e-9 && (y - 0.72).abs() < 1e-9,
            "{x} {y}"
        );
        assert!(ui.settings_dirty);
        // Another scan resolution starts from zero again.
        ui.dpi = 4800;
        assert_eq!(ui.scan_offset(), (0.0, 0.0));
        ui.settings.offset.manual = [0.0, 0.1];
        assert_eq!(ui.scan_offset(), (0.0, 0.1));
    }

    #[test]
    fn a_new_frame_starts_with_the_active_frames_values() {
        let mut ui = three_frames();
        ui.active = 1;
        ui.add_frame();
        assert_eq!(ui.frames.len(), 4);
        assert_eq!(ui.active, 3);
        assert_eq!(evs(&ui), [0.0, 1.0, 2.0, 1.0]);
    }

    #[test]
    fn deleting_a_frame_removes_its_values_and_keeps_the_others_aligned() {
        let mut ui = three_frames();
        ui.active = 1;
        ui.remove_frame();
        assert_eq!(evs(&ui), [0.0, 2.0]);
        assert_eq!(ui.frames.len(), 2);
        assert_eq!(ui.active, 1);
        // The last frame cannot be deleted.
        ui.remove_frame();
        ui.active = 0;
        ui.remove_frame();
        assert_eq!(ui.frames.len(), 1);
        ui.remove_frame();
        assert_eq!((ui.frames.len(), ui.adjusts.len()), (1, 1));
    }

    #[test]
    fn the_preview_and_sliders_follow_the_active_frame() {
        let mut ui = three_frames();
        ui.active = 2;
        assert_eq!(ui.tone().adjust.exposure_ev, 2.0);
        ui.active_adjust_mut().exposure_ev = -1.0;
        assert_eq!(evs(&ui), [0.0, 1.0, -1.0], "only the active frame changes");
        ui.active = 0;
        assert_eq!(ui.tone().adjust.exposure_ev, 0.0);
    }

    #[test]
    fn reset_touches_only_the_active_frame_and_copy_to_all_copies_it() {
        let mut ui = three_frames();
        ui.active = 1;
        ui.reset_adjust();
        assert_eq!(evs(&ui), [0.0, 0.0, 2.0]);
        ui.active = 2;
        ui.copy_adjust_to_all();
        assert_eq!(evs(&ui), [2.0, 2.0, 2.0]);
    }

    #[test]
    fn choosing_a_profile_or_getting_a_new_preview_resets_the_values() {
        let mut ui = three_frames();
        ui.place_frames_from_profile();
        assert_eq!(ui.adjusts.len(), ui.frames.len());
        assert!(ui.adjusts.iter().all(Adjust::is_neutral));
    }

    #[test]
    fn auto_fills_the_sliders_of_the_active_frame_only() {
        let mut ui = ui_with_preview();
        ui.active = 1;
        ui.auto_active();
        assert!(ui.adjusts[0].is_neutral());
        let a = ui.adjusts[1];
        assert!(a.black > 0.0 && a.white > a.black, "{a:?}");
        assert_eq!(
            (a.exposure_ev, a.gamma, a.warmth, a.tint),
            (0.0, 1.0, 0.0, 0.0)
        );
        assert!(!ui.status.as_ref().unwrap().0, "reported as a success");
        assert!(ui.status.as_ref().unwrap().1.contains("frame 2"));
    }

    #[test]
    fn auto_all_measures_every_frame_on_its_own_area() {
        let mut ui = ui_with_preview();
        ui.auto_all();
        let (dark, bright) = (ui.adjusts[0], ui.adjusts[1]);
        assert!(dark.black < bright.black, "{dark:?} vs {bright:?}");
        assert!(dark.white < bright.white, "{dark:?} vs {bright:?}");
        // The bright frame spans 30000..60000 of 65535, the dark one 10000..30000.
        assert!(
            (bright.white - 60_000.0 / 65_535.0).abs() < 0.02,
            "{bright:?}"
        );
        assert!((dark.black - 10_000.0 / 65_535.0).abs() < 0.02, "{dark:?}");
    }

    #[test]
    fn auto_follows_the_negative_setting() {
        let mut ui = ui_with_preview();
        ui.auto_all();
        let plain = ui.adjusts.clone();
        ui.negative = true;
        ui.auto_all();
        assert_ne!(plain, ui.adjusts);
    }

    #[test]
    fn auto_without_a_preview_is_an_error_and_changes_nothing() {
        let mut ui = ui();
        ui.auto_active();
        assert!(ui.status.as_ref().unwrap().0);
        ui.auto_all();
        assert!(ui.status.as_ref().unwrap().0);
        assert!(ui.adjusts.iter().all(Adjust::is_neutral));
    }
}

#[cfg(test)]
mod destination_tests {
    use super::*;
    use settings::SaveSettings;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("scan-dest-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn standalone(save: SaveSettings) -> ScanUi {
        ScanUi::with_settings(
            None,
            true,
            Settings {
                save,
                ..Settings::default()
            },
        )
    }

    /// An archive folder with one film (roll_film 1).
    fn archive(name: &str) -> PathBuf {
        let d = temp(name);
        std::fs::write(
            d.join(latent_scan_archive::DATA_FILE),
            r#"{"tables":{"roll_film":{"next_id":2,"rows":[{"_id":1,"film_name":"FP4"}]}}}"#,
        )
        .unwrap();
        d
    }

    #[test]
    fn the_standalone_app_saves_to_a_folder_by_default() {
        let ui = standalone(SaveSettings::default());
        assert_eq!(ui.mode, SaveMode::Folder);
        assert!(ui.root().ends_with("Pictures/latent-scan"));
        assert_eq!(
            ui.dest(),
            Some(worker::Dest::Folder {
                dir: ui.folder.clone()
            })
        );
    }

    #[test]
    fn the_remembered_destination_is_restored_and_an_argument_overrides_it() {
        let root = archive("restore");
        let saved = SaveSettings {
            mode: SaveMode::Archive,
            folder: Some(PathBuf::from("/data/scans")),
            archive: Some(root.clone()),
            eight_bit: false,
        };
        let ui = standalone(saved.clone());
        assert_eq!(
            (ui.mode, ui.archive.clone()),
            (SaveMode::Archive, Some(root.clone()))
        );
        assert_eq!(ui.folder, PathBuf::from("/data/scans"));

        let other = PathBuf::from("/elsewhere");
        let ui = ScanUi::with_settings(
            Some(other.clone()),
            true,
            Settings {
                save: saved,
                ..Settings::default()
            },
        );
        assert_eq!((ui.mode, ui.archive), (SaveMode::Archive, Some(other)));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn an_archive_needs_a_root_and_a_film_before_it_can_be_scanned_into() {
        let mut ui = standalone(SaveSettings {
            mode: SaveMode::Archive,
            ..SaveSettings::default()
        });
        assert_eq!(ui.dest(), None, "no archive chosen yet");

        let root = archive("film");
        ui.picked(worker::PickKind::ArchiveRoot, root.clone());
        assert_eq!(ui.archive, Some(root.clone()));
        assert_eq!(ui.targets.len(), 1, "the films of the archive are listed");
        ui.row = Some(1);
        assert_eq!(
            ui.dest(),
            Some(worker::Dest::Archive {
                root: root.clone(),
                table: "roll_film".into(),
                row_id: 1,
                write_data: true,
            })
        );
        ui.row = None;
        assert_eq!(ui.dest(), None, "no film chosen");
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn a_folder_without_a_data_file_is_refused_as_an_archive() {
        let mut ui = standalone(SaveSettings::default());
        let plain = temp("plain");
        ui.picked(worker::PickKind::ArchiveRoot, plain.clone());
        assert_eq!(ui.archive, None);
        assert_eq!(ui.mode, SaveMode::Folder, "stays where it was");
        let (is_error, msg) = ui.status.clone().unwrap();
        assert!(is_error && msg.contains("not a latent archive"), "{msg}");
        std::fs::remove_dir_all(plain).ok();
    }

    #[test]
    fn choosing_a_save_folder_switches_to_folder_mode_and_is_remembered() {
        let mut ui = standalone(SaveSettings {
            mode: SaveMode::Archive,
            ..SaveSettings::default()
        });
        ui.picked(worker::PickKind::SaveFolder, PathBuf::from("/data/scans"));
        assert_eq!(ui.mode, SaveMode::Folder);
        assert_eq!(ui.folder, PathBuf::from("/data/scans"));
        assert_eq!(ui.settings.save.folder, Some(PathBuf::from("/data/scans")));
        assert_eq!(ui.settings.save.mode, SaveMode::Folder);
        assert!(ui.settings_dirty, "written to settings.toml once idle");
    }

    #[test]
    fn a_host_embedding_the_ui_cannot_switch_away_from_its_archive() {
        let root = archive("host");
        let mut ui = ScanUi::new(root.clone());
        assert_eq!((ui.mode, ui.standalone), (SaveMode::Archive, false));
        ui.set_mode(SaveMode::Folder, &egui::Context::default());
        assert_eq!(ui.mode, SaveMode::Archive);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn thumbnails_of_an_archive_live_inside_it_and_those_of_a_folder_outside() {
        let root = archive("cache");
        let mut ui = ScanUi::new(root.clone());
        assert_eq!(ui.thumb_cache_dir(), root.join(".thumbs"));
        ui = standalone(SaveSettings::default());
        assert!(
            !ui.thumb_cache_dir().starts_with(&ui.folder),
            "the folder stays clean"
        );
        assert!(ui.thumb_cache_dir().ends_with("latent-scan/thumbs"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn switching_destination_keeps_frames_and_adjustments() {
        let root = archive("keep");
        let mut ui = standalone(SaveSettings::default());
        ui.frames = vec![crop::FULL, [0.1, 0.1, 0.5, 0.5]];
        ui.adjusts = vec![
            Adjust::default(),
            Adjust {
                exposure_ev: 1.0,
                ..Adjust::default()
            },
        ];
        ui.picked(worker::PickKind::ArchiveRoot, root.clone());
        ui.picked(worker::PickKind::SaveFolder, PathBuf::from("/data/scans"));
        assert_eq!(ui.frames.len(), 2);
        assert_eq!(ui.adjusts[1].exposure_ev, 1.0);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn rotation_comes_from_the_profile_and_is_saved_with_it() {
        let mut ui = standalone(SaveSettings::default());
        ui.rotate = 90;
        ui.profiles = latent_scan_profiles::Profiles::parse(
            "[[profile]]\nname='a'\nsource='flatbed'\ncolor='color'\nbit_depth=8\ndpi=300\nrotate=270\n[profile.area]\nx=0.0\ny=0.0\nwidth=1.0\nheight=1.0\n",
        )
        .unwrap();
        ui.select_profile(0);
        assert_eq!(ui.rotate, 270);
        assert_eq!(ui.profiles.all()[0].rotate, 270);
    }
}

#[cfg(test)]
mod zoom_tests {
    use super::*;

    fn ui() -> ScanUi {
        ScanUi::new(std::env::temp_dir())
    }

    #[test]
    fn zoom_to_frame_toggles_on_and_off_and_follows_the_active_frame() {
        let mut ui = ui();
        assert!(ui.zoom.is_none(), "starts with the whole picture");
        ui.toggle_zoom_to_frame();
        let z = ui.zoom.expect("zoomed");
        assert!(
            z.follow.is_some(),
            "follows the active frame until the user zooms by hand"
        );
        ui.toggle_zoom_to_frame();
        assert!(ui.zoom.is_none());
    }

    #[test]
    fn the_zoom_follows_frame_selection_and_rotation_but_not_edits() {
        let mut ui = ui();
        ui.frames = vec![[0.1, 0.1, 0.3, 0.3], [0.6, 0.6, 0.8, 0.8]];
        ui.toggle_zoom_to_frame();
        let shape = 1.0;
        view::follow_frame(&mut ui.zoom, &ui.frames, ui.active, ui.rotate, shape);
        let first = ui.zoom.unwrap();
        assert!((first.center[0] - 0.2).abs() < 1e-4);

        // Editing the frame's edges leaves the view where it is.
        ui.frames[0] = [0.1, 0.1, 0.4, 0.4];
        view::follow_frame(&mut ui.zoom, &ui.frames, ui.active, ui.rotate, shape);
        assert_eq!(ui.zoom.unwrap(), first);

        // Selecting another frame moves the view to it, and so does rotating.
        ui.active = 1;
        view::follow_frame(&mut ui.zoom, &ui.frames, ui.active, ui.rotate, shape);
        assert!((ui.zoom.unwrap().center[0] - 0.7).abs() < 1e-4);
        ui.rotate = 90;
        view::follow_frame(&mut ui.zoom, &ui.frames, ui.active, ui.rotate, shape);
        assert_eq!(ui.zoom.unwrap().follow, Some((1, 90)));
        // 90 degrees clockwise: a frame at the right/bottom ends up at the left/bottom.
        assert!(ui.zoom.unwrap().center[0] < 0.5 && ui.zoom.unwrap().center[1] > 0.5);
    }

    #[test]
    fn a_new_set_of_frames_or_a_new_preview_starts_with_the_whole_picture() {
        let mut ui = ui();
        ui.toggle_zoom_to_frame();
        ui.place_frames_from_profile();
        assert!(ui.zoom.is_none());
    }
}

#[cfg(test)]
mod tone_shaping_tests {
    use super::*;

    fn shaped() -> Adjust {
        Adjust {
            contrast: 0.4,
            darks: -0.3,
            whites: 0.6,
            ..Adjust::default()
        }
    }

    fn two_frames() -> ScanUi {
        let mut ui = ScanUi::new(std::env::temp_dir());
        ui.frames = vec![crop::FULL, [0.0, 0.0, 0.5, 0.5]];
        ui.adjusts = vec![shaped(), Adjust::default()];
        ui
    }

    #[test]
    fn reset_clears_contrast_darks_and_whites_of_the_active_frame_only() {
        let mut ui = two_frames();
        ui.adjusts[1] = shaped();
        ui.active = 0;
        ui.reset_adjust();
        assert!(ui.adjusts[0].is_neutral());
        assert_eq!(ui.adjusts[1], shaped());
    }

    #[test]
    fn copy_to_all_copies_the_new_values_too() {
        let mut ui = two_frames();
        ui.active = 0;
        ui.copy_adjust_to_all();
        assert_eq!(ui.adjusts, [shaped(), shaped()]);
    }

    #[test]
    fn they_reach_the_tone_used_for_the_preview_and_the_scan() {
        let mut ui = two_frames();
        ui.active = 0;
        let t = ui.tone().adjust;
        assert_eq!((t.contrast, t.darks, t.whites), (0.4, -0.3, 0.6));
        ui.active = 1;
        assert!(ui.tone().adjust.is_neutral());
    }
}
