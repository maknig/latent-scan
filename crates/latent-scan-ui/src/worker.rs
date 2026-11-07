//! Background jobs. Each runs on its own thread and reports through an mpsc channel, calling
//! `request_repaint` after every message (the pattern used by latent's `similar.rs`).

use crate::preview::downscale;
use crate::thumbs;
use egui::Context;
use latent_image_edit::tone::{Adjust, ToneOptions, ToneStage};
use latent_image_edit::{BitDepth, OutputFormat, Pipeline, Rotate};
use latent_scan_archive::Archive;
use latent_scan_core::{
    CancelToken, Capabilities, RawImage, ScanArea, ScanError, ScanJob, Scanner, ScannerBackend,
    ScannerInfo, Source,
};
use latent_scan_sane::SaneBackend;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

/// Longest side (px) of the stored overview for a preview scanned at `dpi`: the 16-bit overview is
/// re-rendered on every slider move, so low resolutions keep the small size and only a higher
/// preview resolution buys more pixels.
pub(crate) fn preview_max_side(dpi: u32) -> u32 {
    if dpi <= 300 {
        2048
    } else {
        4096
    }
}
/// Size of the strip thumbnail written into the archive's thumbnail cache after each scan.
pub(crate) const STRIP_THUMB: u32 = 320;

pub enum Msg {
    Devices(Vec<(ScannerInfo, Capabilities)>),
    Progress(f32),
    Preview {
        source: Source,
        dpi: u32,
        raw: RawImage,
        area: ScanArea,
    },
    Saved(SavedTo),
    /// The frames were fitted to the film on the overview.
    Fitted(Vec<latent_scan_detect::Fit>),
    /// The answer of a folder dialog (`None` when it was cancelled).
    Picked {
        kind: PickKind,
        path: Option<PathBuf>,
    },
    /// A saved scan was compared with the preview it was placed on (`warn`: off by more than
    /// the tolerance).
    Checked {
        text: String,
        warn: bool,
        /// Where the scan lay minus where it was asked (mm), for learning the offset, and the
        /// correlation of that match.
        error: Option<(f64, f64)>,
        correlation: f32,
    },
    /// A batch moved on to frame `index` (0-based) of `total`.
    Frame {
        index: usize,
        total: usize,
    },
    /// Result of the auto-adjust pre-scan of frame `index` (0-based).
    Adjusted {
        index: usize,
        shift_mm: (f64, f64),
        confidence: f32,
        moved: bool,
    },
    BatchDone,
    Failed(String),
    Cancelled,
}

pub struct Frame {
    pub area: ScanArea,
    /// File name without extension.
    pub stem: String,
    /// Image adjustments of this frame (applied after `ScanRequest::tone`).
    pub adjust: Adjust,
}

/// Where a batch saves its files.
#[derive(Debug, Clone, PartialEq)]
pub enum Dest {
    /// Plain files directly in `dir` (created when needed).
    Folder { dir: PathBuf },
    /// A latent archive: `images/<table>/<id>/...`, optionally listed in the row's `images`.
    Archive {
        root: PathBuf,
        table: String,
        row_id: u64,
        /// Also append the file to the row's `images` list in `latent.data.json`.
        write_data: bool,
    },
}

/// Which folder a dialog is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickKind {
    /// The folder plain scans are saved in.
    SaveFolder,
    /// The root folder of a latent archive.
    ArchiveRoot,
}

/// What a saved scan reports back.
#[derive(Debug, Clone, PartialEq)]
pub enum SavedTo {
    Archive {
        table: String,
        row_id: u64,
        /// Archive-relative path with forward slashes.
        rel: String,
    },
    File {
        path: PathBuf,
    },
}

pub struct ScanRequest {
    pub device: ScannerInfo,
    /// Settings shared by all frames; its `area` is ignored.
    pub job: ScanJob,
    pub frames: Vec<Frame>,
    pub rotate: u16,
    pub tone: ToneOptions,
    pub format: OutputFormat,
    pub depth: BitDepth,
    /// Where the files go.
    pub dest: Dest,
    /// Thumbnail cache the strip reads; a fresh thumbnail is written there after each scan.
    pub thumb_cache: PathBuf,
    /// Re-fit each frame on a pre-scan right before scanning it.
    pub adjust: Option<latent_scan_detect::Options>,
    /// Options of the job's source, for the pre-scan's window and resolution.
    pub source_caps: Option<latent_scan_core::SourceCaps>,
    /// The preview the frames were placed on: each scan is compared with it afterwards.
    pub overview: Option<Overview>,
    /// Added to every frame's area (mm): the scanner's systematic offset against the preview.
    pub offset: (f64, f64),
}

/// The preview a scan is compared with: its pixels and the bed area it covers.
#[derive(Clone)]
pub struct Overview {
    pub raw: std::sync::Arc<RawImage>,
    pub area: ScanArea,
}

#[derive(Clone)]
pub struct Reporter {
    pub tx: Sender<Msg>,
    pub ctx: Context,
}

/// When the window should be redrawn after a message: at once, except for progress, which
/// comes in dozens of small steps and is throttled (more so while the window is not focused:
/// nobody watches a window on another workspace, and it gets no frame callbacks there).
pub(crate) fn repaint_delay(is_progress: bool, focused: bool) -> std::time::Duration {
    use std::time::Duration;
    match (is_progress, focused) {
        (false, _) => Duration::ZERO,
        (true, true) => Duration::from_millis(100),
        (true, false) => Duration::from_secs(1),
    }
}

impl Reporter {
    fn send(&self, msg: Msg) {
        let is_progress = matches!(msg, Msg::Progress(_));
        let _ = self.tx.send(msg);
        let focused = self.ctx.input(|i| i.viewport().focused).unwrap_or(true);
        let delay = repaint_delay(is_progress, focused);
        if delay.is_zero() {
            self.ctx.request_repaint();
        } else {
            self.ctx.request_repaint_after(delay);
        }
    }

    fn finish(&self, result: Result<Msg, ScanError>) {
        self.send(match result {
            Ok(msg) => msg,
            Err(ScanError::Cancelled) => Msg::Cancelled,
            Err(e) => Msg::Failed(e.to_string()),
        });
    }
}

/// Opens the native folder dialog on its own thread (it blocks until closed) and reports the
/// choice as `Msg::Picked`.
pub fn pick_folder(r: Reporter, kind: PickKind, start: Option<PathBuf>) {
    std::thread::spawn(move || {
        let title = match kind {
            PickKind::SaveFolder => "Folder for the scans",
            PickKind::ArchiveRoot => "Root folder of the latent archive",
        };
        let mut dialog = rfd::FileDialog::new().set_title(title);
        if let Some(dir) = start.filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        r.send(Msg::Picked {
            kind,
            path: dialog.pick_folder(),
        });
    });
}

/// Fits the preset frames to the film on the overview, on a thread (it scans every pixel).
pub fn fit_frames(
    r: Reporter,
    raw: std::sync::Arc<latent_scan_core::RawImage>,
    area: ScanArea,
    presets: Vec<ScanArea>,
    opts: latent_scan_detect::Options,
) {
    std::thread::spawn(move || {
        let fits = latent_image_edit_ui::trace::timed("auto-adjust frames", || {
            latent_scan_detect::fit_frames(&raw, area, &presets, &opts)
        });
        r.send(Msg::Fitted(fits));
    });
}

pub fn discover(r: Reporter) {
    std::thread::spawn(move || {
        let backend = SaneBackend::default();
        let found = backend.discover().and_then(|infos| {
            infos
                .into_iter()
                .map(|i| {
                    let caps = backend.open(&i)?.capabilities()?;
                    Ok((i, caps))
                })
                .collect()
        });
        r.finish(found.map(Msg::Devices));
    });
}

pub fn preview(r: Reporter, device: ScannerInfo, job: ScanJob, cancel: CancelToken) {
    std::thread::spawn(move || {
        let result = SaneBackend::default().open(&device).and_then(|s| {
            let progress = |p: f32| r.send(Msg::Progress(p));
            let raw = s.scan(&job, &progress, &cancel)?;
            Ok(Msg::Preview {
                raw: downscale(&raw, preview_max_side(job.dpi)),
                area: delivered_area(job.area, raw.width, raw.height, job.dpi),
                source: job.source,
                dpi: job.dpi,
            })
        });
        r.finish(result);
    });
}

pub fn scan(r: Reporter, req: ScanRequest, cancel: CancelToken) {
    std::thread::spawn(move || {
        let result = SaneBackend::default()
            .open(&req.device)
            .and_then(|scanner| run_batch(scanner.as_ref(), &req, &cancel, &|m| r.send(m)));
        match result {
            Ok(()) => r.send(Msg::BatchDone),
            Err(e) => r.finish(Err(e)),
        }
    });
}

/// Scans the request's frames one after another; frames already saved stay saved when a later
/// one fails or the batch is cancelled.
fn run_batch(
    scanner: &dyn Scanner,
    req: &ScanRequest,
    cancel: &CancelToken,
    emit: &(dyn Fn(Msg) + Sync),
) -> Result<(), ScanError> {
    let total = req.frames.len();
    for (i, frame) in req.frames.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(ScanError::Cancelled);
        }
        emit(Msg::Frame { index: i, total });
        let progress = |p: f32| emit(Msg::Progress((i as f32 + p) / total as f32));
        let mut job = ScanJob {
            area: frame.area,
            ..req.job.clone()
        };
        let with_frame = |e: ScanError| match e {
            ScanError::Cancelled => e,
            e => ScanError::Backend(format!("frame {} ({}): {e}", i + 1, frame.stem)),
        };
        let mut fit_shift = None;
        if let Some(opts) = &req.adjust {
            let fit = latent_scan_detect::adjust_before_scan(
                scanner,
                &job,
                req.source_caps.as_ref(),
                opts,
                cancel,
            )
            .map_err(with_frame)?;
            job.area = fit.area;
            fit_shift = Some(fit.shift_mm);
            emit(Msg::Adjusted {
                index: i,
                shift_mm: fit.shift_mm,
                confidence: fit.confidence,
                moved: fit.moved,
            });
        }
        // The scanner starts a little away from where the preview did; correct for that.
        job.area.x = (job.area.x + req.offset.0).max(0.0);
        job.area.y = (job.area.y + req.offset.1).max(0.0);
        let raw = scanner.scan(&job, &progress, cancel).map_err(with_frame)?;
        let checked = req.overview.as_ref().map(|o| {
            let case = Case {
                stem: &frame.stem,
                view: frame.area,
                offset: req.offset,
                fit_shift,
            };
            check_text(&case, &job, &raw, o)
        });

        let pipeline = Pipeline::default()
            .with(Rotate(req.rotate))
            .with(ToneStage(ToneOptions {
                adjust: frame.adjust,
                ..req.tone
            }));
        let img = pipeline.run(raw).map_err(|e| with_frame(e.into()))?;

        let saved = save_frame(&req.dest, &frame.stem, (req.format, req.depth), &img)
            .map_err(with_frame)?;
        // Best effort: lets the image strip show the new scan without decoding the big file.
        let file = match (&saved, &req.dest) {
            (SavedTo::File { path }, _) => path.clone(),
            (SavedTo::Archive { rel, .. }, Dest::Archive { root, .. }) => root.join(rel),
            (SavedTo::Archive { rel, .. }, Dest::Folder { dir }) => dir.join(rel),
        };
        thumbs::write_cache(&req.thumb_cache, &file, STRIP_THUMB, &img);
        emit(Msg::Saved(saved));
        if let Some(c) = checked {
            emit(Msg::Checked {
                text: c.text,
                warn: c.warn,
                error: c.error,
                correlation: c.correlation,
            });
        }
    }
    Ok(())
}

/// Largest size or position difference (mm) between a scan and its preview frame that is not
/// reported as a warning.
const CHECK_TOLERANCE_MM: f64 = 0.15;

/// What one frame was meant to be, for [`check_text`].
struct Case<'a> {
    stem: &'a str,
    /// The frame as it is in the view.
    view: ScanArea,
    /// The offset that was added to the request.
    offset: (f64, f64),
    /// How far the pre-scan fit moved the frame, if it ran.
    fit_shift: Option<(f64, f64)>,
}

/// What [`check_text`] found.
struct Checked {
    text: String,
    warn: bool,
    error: Option<(f64, f64)>,
    correlation: f32,
}

/// Compares a scan (taken for `job.area`) with the preview it was placed on and describes the
/// result for the status line. The position is given against the frame in the view, so it reads
/// zero once the offset is right; `error` is the scan's position against what was requested.
fn check_text(case: &Case, job: &ScanJob, raw: &RawImage, overview: &Overview) -> Checked {
    let c = latent_image_edit_ui::trace::timed("compare scan with preview", || {
        latent_scan_detect::compare_with_overview(
            raw,
            job.area,
            job.dpi,
            &overview.raw,
            overview.area,
        )
    });
    let view = case.view;
    let mut text = format!(
        "Saved {}: asked {:.2} x {:.2} mm, got {:.2} x {:.2} mm",
        case.stem, view.width, view.height, c.got_mm.0, c.got_mm.1
    );
    let mut warn = (c.got_mm.0 - view.width).abs() > CHECK_TOLERANCE_MM
        || (c.got_mm.1 - view.height).abs() > CHECK_TOLERANCE_MM;
    match (c.shift_mm, c.scale) {
        (Some((ex, ey)), Some((sx, sy))) => {
            // Where the picture lies relative to the frame in the view.
            let (dx, dy) = (ex + job.area.x - view.x, ey + job.area.y - view.y);
            text.push_str(&format!(
                "; position vs preview {dx:+.2} / {dy:+.2} mm, length x{sx:.3} / x{sy:.3}"
            ));
            warn |= dx.abs() > CHECK_TOLERANCE_MM
                || dy.abs() > CHECK_TOLERANCE_MM
                || (sx - 1.0).abs() * view.width > CHECK_TOLERANCE_MM
                || (sy - 1.0).abs() * view.height > CHECK_TOLERANCE_MM;
        }
        _ => text.push_str("; too little structure to compare with the preview"),
    }
    if case.offset != (0.0, 0.0) {
        text.push_str(&format!(
            "; offset applied {:+.2} / {:+.2} mm",
            case.offset.0, case.offset.1
        ));
    }
    if let Some((fx, fy)) = case.fit_shift {
        text.push_str(&format!("; pre-scan fit moved it {fx:+.2} / {fy:+.2} mm"));
    }
    if std::env::var_os("LATENT_SCAN_TRACE").is_some_and(|v| !v.is_empty()) {
        eprintln!("[trace] {text}");
    }
    Checked {
        text,
        warn,
        error: c.shift_mm,
        correlation: c.correlation,
    }
}

/// Writes one finished frame to its destination.
fn save_frame(
    dest: &Dest,
    stem: &str,
    (format, depth): (OutputFormat, BitDepth),
    img: &RawImage,
) -> Result<SavedTo, ScanError> {
    match dest {
        Dest::Folder { dir } => {
            let path = latent_scan_archive::folder_target(dir, stem, format.ext())?;
            latent_image_edit::write_image_depth(img, &path, depth)?;
            Ok(SavedTo::File { path })
        }
        Dest::Archive {
            root,
            table,
            row_id,
            write_data,
        } => {
            let archive = Archive::new(root);
            let target = archive.target(table, *row_id, stem, format.ext())?;
            latent_image_edit::write_image_depth(img, &target.abs, depth)?;
            if *write_data {
                archive
                    .add_image(table, *row_id, &target.rel)
                    .map_err(|e| ScanError::Backend(format!("saved {} but {e}", target.rel)))?;
            }
            Ok(SavedTo::Archive {
                table: table.clone(),
                row_id: *row_id,
                rel: target.rel,
            })
        }
    }
}

/// The area a scan really covers: scanners clip requests to their physical window (the 9000F's
/// transparency window is about 250 mm tall, not the 297 mm it reports), so the size follows
/// from the pixels delivered. The origin stays where it was requested.
fn delivered_area(requested: ScanArea, width_px: u32, height_px: u32, dpi: u32) -> ScanArea {
    let mm = |px: u32| px as f64 / dpi.max(1) as f64 * 25.4;
    ScanArea {
        width: mm(width_px),
        height: mm(height_px),
        ..requested
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_progress_is_throttled_and_more_so_in_the_background() {
        use std::time::Duration;
        assert_eq!(repaint_delay(false, true), Duration::ZERO);
        assert_eq!(repaint_delay(false, false), Duration::ZERO);
        assert_eq!(repaint_delay(true, true), Duration::from_millis(100));
        assert_eq!(repaint_delay(true, false), Duration::from_secs(1));
    }

    #[test]
    fn a_progress_message_is_delivered_even_without_a_window() {
        let (tx, rx) = std::sync::mpsc::channel();
        let r = Reporter {
            tx,
            ctx: Context::default(),
        };
        r.send(Msg::Progress(0.5));
        assert!(matches!(rx.try_recv(), Ok(Msg::Progress(_))));
    }

    #[test]
    fn delivered_area_follows_the_pixels() {
        let requested = ScanArea {
            x: 0.0,
            y: 0.0,
            width: 216.069,
            height: 297.011,
        };
        // Measured on the 9000F transparency unit at 300 dpi.
        let a = delivered_area(requested, 2552, 2960, 300);
        assert!((a.width - 216.07).abs() < 0.1 && (a.height - 250.6).abs() < 0.1);
        assert_eq!((a.x, a.y), (0.0, 0.0));
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    use latent_scan_core::{ColorMode, Progress, RawImage, Result};
    use std::cell::RefCell;

    /// Returns a tiny image; fails on the call numbers in `fail_on` (1-based) and cancels the
    /// token after `cancel_after` successful scans.
    struct Fake {
        info: ScannerInfo,
        calls: RefCell<Vec<ScanArea>>,
        fail_on: Option<usize>,
        cancel_after: Option<(usize, CancelToken)>,
    }

    impl Scanner for Fake {
        fn info(&self) -> &ScannerInfo {
            &self.info
        }
        fn capabilities(&self) -> Result<Capabilities> {
            Ok(Capabilities::default())
        }
        fn scan(&self, job: &ScanJob, progress: Progress, _: &CancelToken) -> Result<RawImage> {
            let n = {
                let mut c = self.calls.borrow_mut();
                c.push(job.area);
                c.len()
            };
            progress(0.5);
            if self.fail_on == Some(n) {
                return Err(ScanError::Backend("jam".into()));
            }
            if let Some((after, token)) = &self.cancel_after {
                if n >= *after {
                    token.cancel();
                }
            }
            Ok(RawImage::new(2, 1, 1, vec![0, 65535])?)
        }
    }

    fn fake() -> Fake {
        Fake {
            info: ScannerInfo {
                id: "fake".into(),
                vendor: "v".into(),
                model: "m".into(),
            },
            calls: RefCell::new(vec![]),
            fail_on: None,
            cancel_after: None,
        }
    }

    pub(super) fn request(dir: &std::path::Path, n: usize) -> ScanRequest {
        let area = |x: f64| ScanArea {
            x,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        };
        ScanRequest {
            device: fake().info,
            job: ScanJob {
                source: Source::Flatbed,
                color: ColorMode::Gray,
                bit_depth: 16,
                dpi: 300,
                preview: false,
                area: area(0.0),
            },
            frames: (0..n)
                .map(|i| Frame {
                    area: area(i as f64 * 20.0),
                    stem: latent_scan_archive::naming::frame_stem("frame", 5 + i as u32),
                    adjust: Adjust::default(),
                })
                .collect(),
            rotate: 0,
            tone: ToneOptions::default(),
            format: OutputFormat::default(),
            depth: BitDepth::default(),
            dest: Dest::Archive {
                root: dir.to_path_buf(),
                table: "roll_film".into(),
                row_id: 1,
                write_data: false,
            },
            thumb_cache: dir.join(".thumbs"),
            adjust: None,
            source_caps: None,
            overview: None,
            offset: (0.0, 0.0),
        }
    }

    fn run(f: &Fake, req: &ScanRequest, cancel: &CancelToken) -> (Result<()>, Vec<String>) {
        let log = std::sync::Mutex::new(vec![]);
        let emit = |m: Msg| {
            if let Msg::Saved(SavedTo::Archive { rel, .. }) = m {
                log.lock().unwrap().push(rel);
            }
        };
        (run_batch(f, req, cancel, &emit), log.into_inner().unwrap())
    }

    #[test]
    fn the_offset_moves_the_request_but_not_the_frame_in_the_view() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = request(dir.path(), 2);
        req.offset = (-0.5, 0.7);
        let f = fake();
        run_batch(&f, &req, &CancelToken::default(), &|_| {}).unwrap();
        let areas = f.calls.borrow().clone();
        // Frame 1 starts at x 0 / y 0: the negative x is clamped, y is moved down.
        assert_eq!((areas[0].x, areas[0].y), (0.0, 0.7));
        assert_eq!((areas[1].x, areas[1].y), (19.5, 0.7));
        assert_eq!(
            (areas[1].width, areas[1].height),
            (10.0, 10.0),
            "size is untouched"
        );
    }

    #[test]
    fn the_check_line_mentions_the_applied_offset_and_the_fit() {
        let img = RawImage::new(2, 1, 1, vec![0, 65535]).unwrap();
        let overview = Overview {
            raw: std::sync::Arc::new(RawImage::new(10, 10, 1, vec![30000; 100]).unwrap()),
            area: ScanArea {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
        };
        let view = ScanArea {
            x: 20.0,
            y: 20.0,
            width: 10.0,
            height: 10.0,
        };
        let job = ScanJob {
            source: Source::Flatbed,
            color: ColorMode::Gray,
            bit_depth: 16,
            dpi: 300,
            preview: false,
            area: ScanArea { y: 20.7, ..view },
        };
        let case = Case {
            stem: "frame-07",
            view,
            offset: (0.0, 0.7),
            fit_shift: Some((0.1, -0.2)),
        };
        let c = check_text(&case, &job, &img, &overview);
        assert!(
            c.text.contains("offset applied +0.00 / +0.70 mm"),
            "{}",
            c.text
        );
        assert!(
            c.text.contains("pre-scan fit moved it +0.10 / -0.20 mm"),
            "{}",
            c.text
        );
    }

    #[test]
    fn every_saved_frame_is_compared_with_the_preview_it_was_placed_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = request(dir.path(), 2);
        req.overview = Some(Overview {
            raw: std::sync::Arc::new(RawImage::new(10, 10, 1, vec![30000; 100]).unwrap()),
            area: ScanArea {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
        });
        let checks = std::sync::Mutex::new(vec![]);
        let emit = |m: Msg| {
            if let Msg::Checked { text, warn, .. } = m {
                checks.lock().unwrap().push((text, warn));
            }
        };
        run_batch(&fake(), &req, &CancelToken::default(), &emit).unwrap();
        let checks = checks.into_inner().unwrap();
        assert_eq!(checks.len(), 2, "one report per frame");
        let (text, warn) = &checks[0];
        // The fake delivers 2 x 1 pixels instead of 10 x 10 mm: far off, and not matchable.
        assert!(text.starts_with("Saved frame-05: asked 10.00 x 10.00 mm, got 0.17 x 0.08 mm"));
        assert!(text.contains("too little structure"));
        assert!(*warn);
    }

    #[test]
    fn the_chosen_bit_depth_decides_what_is_written() {
        for (depth, bits) in [(BitDepth::Eight, 8), (BitDepth::Sixteen, 16)] {
            let dir = tempfile::tempdir().unwrap();
            let mut req = request(dir.path(), 1);
            req.depth = depth;
            let (res, saved) = run(&fake(), &req, &CancelToken::default());
            res.unwrap();
            let file = dir.path().join(&saved[0]);
            assert_eq!(
                latent_image_edit::file_depth(&file).map(BitDepth::bits),
                Some(bits)
            );
        }
    }

    #[test]
    fn a_jpeg_scan_is_saved_as_an_8_bit_jpg() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = request(dir.path(), 1);
        req.format = OutputFormat::Jpeg;
        req.depth = BitDepth::Sixteen;
        let (res, saved) = run(&fake(), &req, &CancelToken::default());
        res.unwrap();
        assert!(saved[0].ends_with(".jpg"), "{}", saved[0]);
        let file = dir.path().join(&saved[0]);
        assert_eq!(latent_image_edit::file_depth(&file), Some(BitDepth::Eight));
    }

    #[test]
    fn scans_every_frame_in_order_with_its_own_name_and_area() {
        let dir = tempfile::tempdir().unwrap();
        let f = fake();
        let (res, saved) = run(&f, &request(dir.path(), 3), &CancelToken::default());
        res.unwrap();
        assert_eq!(
            saved,
            [
                "images/roll_film/0001/frame-05.tiff",
                "images/roll_film/0001/frame-06.tiff",
                "images/roll_film/0001/frame-07.tiff"
            ]
        );
        let xs: Vec<f64> = f.calls.borrow().iter().map(|a| a.x).collect();
        assert_eq!(xs, vec![0.0, 20.0, 40.0]);
        assert!(dir.path().join(&saved[2]).exists());
    }

    #[test]
    fn a_failure_keeps_earlier_frames_and_names_the_frame() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = fake();
        f.fail_on = Some(2);
        let (res, saved) = run(&f, &request(dir.path(), 3), &CancelToken::default());
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("frame 2") && msg.contains("frame-06") && msg.contains("jam"));
        assert_eq!(saved.len(), 1);
        assert_eq!(f.calls.borrow().len(), 2);
    }

    #[test]
    fn cancelling_between_frames_stops_the_batch() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancelToken::default();
        let mut f = fake();
        f.cancel_after = Some((1, cancel.clone()));
        let (res, saved) = run(&f, &request(dir.path(), 3), &cancel);
        assert!(matches!(res, Err(ScanError::Cancelled)));
        assert_eq!(saved.len(), 1);
        assert_eq!(f.calls.borrow().len(), 1);
    }
}

#[cfg(test)]
mod adjust_tests {
    use super::batch_tests::request;
    use super::*;
    use latent_scan_core::{ColorMode, Progress, RawImage, Result};
    use std::sync::Mutex;

    /// A bed with one bright frame at a fixed true position; records the requested areas.
    struct Bed {
        info: ScannerInfo,
        truth: ScanArea,
        calls: Mutex<Vec<(ScanArea, u8)>>,
    }

    impl Scanner for Bed {
        fn info(&self) -> &ScannerInfo {
            &self.info
        }
        fn capabilities(&self) -> Result<Capabilities> {
            Ok(Capabilities::default())
        }
        fn scan(&self, job: &ScanJob, _: Progress, _: &CancelToken) -> Result<RawImage> {
            self.calls.lock().unwrap().push((job.area, job.bit_depth));
            // Keep the final scan small: the test only cares where it is requested.
            let ppm = job.dpi.min(600) as f64 / 25.4;
            let (w, h) = (
                (job.area.width * ppm).round() as u32,
                (job.area.height * ppm).round() as u32,
            );
            let t = &self.truth;
            let data = (0..h)
                .flat_map(|py| (0..w).map(move |px| (px, py)))
                .map(|(px, py)| {
                    let (x, y) = (job.area.x + px as f64 / ppm, job.area.y + py as f64 / ppm);
                    let inside = x >= t.x && x < t.x + t.width && y >= t.y && y < t.y + t.height;
                    if inside {
                        40000
                    } else {
                        5000
                    }
                })
                .collect();
            Ok(RawImage::new(w, h, 1, data)?)
        }
    }

    /// Requested (area, bit depth) of each scan, and (dx, dy, moved) of each adjustment.
    type Outcome = (Vec<(ScanArea, u8)>, Vec<(f64, f64, bool)>);

    fn run_with(truth: ScanArea, adjust: bool) -> Outcome {
        let dir = tempfile::tempdir().unwrap();
        let area = ScanArea {
            x: 60.0,
            y: 30.0,
            width: 36.0,
            height: 24.0,
        };
        let req = ScanRequest {
            device: ScannerInfo {
                id: "b".into(),
                vendor: "v".into(),
                model: "m".into(),
            },
            job: ScanJob {
                source: Source::Transparency,
                color: ColorMode::Gray,
                bit_depth: 16,
                dpi: 300,
                preview: false,
                area,
            },
            frames: vec![Frame {
                area,
                stem: "f-01".into(),
                adjust: Adjust::default(),
            }],
            rotate: 0,
            tone: ToneOptions::default(),
            format: OutputFormat::Tiff,
            depth: BitDepth::default(),
            dest: Dest::Archive {
                root: dir.path().to_path_buf(),
                table: "roll_film".into(),
                row_id: 1,
                write_data: false,
            },
            thumb_cache: dir.path().join(".thumbs"),
            adjust: adjust.then(latent_scan_detect::Options::default),
            source_caps: None,
            overview: None,
            offset: (0.0, 0.0),
        };
        let bed = Bed {
            info: req.device.clone(),
            truth,
            calls: Mutex::new(vec![]),
        };
        let adjusted = Mutex::new(vec![]);
        let emit = |m: Msg| {
            if let Msg::Adjusted {
                shift_mm: (x, y),
                moved,
                ..
            } = m
            {
                adjusted.lock().unwrap().push((x, y, moved));
            }
        };
        run_batch(&bed, &req, &CancelToken::default(), &emit).unwrap();
        (
            bed.calls.into_inner().unwrap(),
            adjusted.into_inner().unwrap(),
        )
    }

    #[test]
    fn adjust_scans_the_frame_where_the_film_really_is() {
        let truth = ScanArea {
            x: 61.8,
            y: 30.7,
            width: 36.0,
            height: 24.0,
        };
        let (calls, adjusted) = run_with(truth, true);
        assert_eq!(calls.len(), 2, "pre-scan, then the real scan");
        assert_eq!(calls[0].1, 8, "pre-scan is 8-bit");
        assert!(calls[0].0.width > 36.0, "pre-scan covers the surroundings");
        let (final_area, depth) = calls[1];
        assert_eq!(depth, 16);
        assert!((final_area.x - 61.8).abs() < 0.2 && (final_area.y - 30.7).abs() < 0.2);
        assert_eq!(final_area.width, 36.0);
        assert!(adjusted[0].2, "reported as moved");
    }

    #[test]
    fn without_adjust_the_preset_is_scanned_as_is() {
        let truth = ScanArea {
            x: 61.8,
            y: 30.7,
            width: 36.0,
            height: 24.0,
        };
        let (calls, adjusted) = run_with(truth, false);
        assert_eq!(calls.len(), 1);
        assert_eq!((calls[0].0.x, calls[0].0.y), (60.0, 30.0));
        assert!(adjusted.is_empty());
    }

    /// Returns a flat mid-grey image, so differing adjustments are visible in the output.
    struct Mid {
        info: ScannerInfo,
    }

    impl Scanner for Mid {
        fn info(&self) -> &ScannerInfo {
            &self.info
        }
        fn capabilities(&self) -> Result<Capabilities> {
            Ok(Capabilities::default())
        }
        fn scan(&self, _: &ScanJob, _: Progress, _: &CancelToken) -> Result<RawImage> {
            Ok(RawImage::new(2, 1, 1, vec![32768, 32768])?)
        }
    }

    #[test]
    fn each_frame_is_saved_with_its_own_adjustments() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = request(dir.path(), 2);
        req.frames[0].adjust = Adjust {
            exposure_ev: -1.0,
            ..Adjust::default()
        };
        req.frames[1].adjust = Adjust::default();
        let mid = Mid {
            info: req.device.clone(),
        };
        let (res, saved) = (
            run_batch(&mid, &req, &CancelToken::default(), &|_| {}),
            req.frames.len(),
        );
        res.unwrap();
        assert_eq!(saved, 2);

        let value = |name: &str| {
            let p = dir
                .path()
                .join(format!("images/roll_film/0001/{name}.tiff"));
            image::open(p).unwrap().into_luma16().into_raw()[0]
        };
        let (a, b) = (value("frame-05"), value("frame-06"));
        assert!(
            (a as i32 - 16384).abs() <= 2,
            "-1 EV halves the mid-grey: {a}"
        );
        assert!(
            (b as i32 - 32768).abs() <= 2,
            "neutral stays untouched: {b}"
        );
    }
}

#[cfg(test)]
mod folder_tests {
    use super::batch_tests::request;
    use super::*;
    use latent_scan_core::{Progress, Result};

    struct Pixels {
        info: ScannerInfo,
    }

    impl Scanner for Pixels {
        fn info(&self) -> &ScannerInfo {
            &self.info
        }
        fn capabilities(&self) -> Result<Capabilities> {
            Ok(Capabilities::default())
        }
        fn scan(&self, _: &ScanJob, _: Progress, _: &CancelToken) -> Result<RawImage> {
            Ok(RawImage::new(2, 1, 1, vec![100, 60000])?)
        }
    }

    #[test]
    fn a_folder_destination_gets_plain_files_and_no_data_file() {
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("my scans");
        let mut req = request(root.path(), 2);
        req.dest = Dest::Folder { dir: out.clone() };
        req.thumb_cache = root.path().join("cache");
        let scanner = Pixels {
            info: req.device.clone(),
        };
        let saved = std::sync::Mutex::new(vec![]);
        let emit = |m: Msg| {
            if let Msg::Saved(s) = m {
                saved.lock().unwrap().push(s);
            }
        };
        run_batch(&scanner, &req, &CancelToken::default(), &emit).unwrap();

        assert_eq!(
            saved.into_inner().unwrap(),
            [
                SavedTo::File {
                    path: out.join("frame-05.tiff")
                },
                SavedTo::File {
                    path: out.join("frame-06.tiff")
                },
            ]
        );
        assert!(out.join("frame-05.tiff").is_file() && out.join("frame-06.tiff").is_file());
        assert!(!out.join("latent.data.json").exists());
        assert!(
            !root.path().join("images").exists(),
            "nothing archive-shaped is created"
        );
        // The strip's thumbnails are written into the cache folder, not next to the scans.
        assert!(root.path().join("cache/320").is_dir());
        assert!(!out.join(".thumbs").exists());
    }

    #[test]
    fn a_second_batch_into_the_same_folder_does_not_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let mut req = request(root.path(), 1);
        req.dest = Dest::Folder {
            dir: root.path().to_path_buf(),
        };
        let scanner = Pixels {
            info: req.device.clone(),
        };
        for _ in 0..2 {
            run_batch(&scanner, &req, &CancelToken::default(), &|_| {}).unwrap();
        }
        assert!(root.path().join("frame-05.tiff").is_file());
        assert!(root.path().join("frame-05-1.tiff").is_file());
    }
}
