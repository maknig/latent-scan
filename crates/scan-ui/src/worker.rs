//! Background jobs. Each runs on its own thread and reports through an mpsc channel, calling
//! `request_repaint` after every message (the pattern used by latent's `similar.rs`).

use crate::preview::downscale;
use crate::thumbs;
use egui::Context;
use scan_archive::Archive;
use scan_core::{
    CancelToken, Capabilities, RawImage, ScanArea, ScanError, ScanJob, Scanner, ScannerBackend,
    ScannerInfo, Source,
};
use scan_pipeline::tone::{Adjust, ToneOptions, ToneStage};
use scan_pipeline::{BitDepth, OutputFormat, Pipeline, Rotate};
use scan_sane::SaneBackend;
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
const STRIP_THUMB: u32 = 320;

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
    /// The image editor's preview was decoded (preview, size of the file in pixels).
    EditorLoaded {
        path: PathBuf,
        result: Result<(RawImage, [u32; 2]), String>,
    },
    /// A file operation of the editor finished (`what` is [`crate::editor::EditorOp::name`]).
    EditorDone {
        path: PathBuf,
        what: &'static str,
        result: Result<(), String>,
    },
    /// The answer of a folder dialog (`None` when it was cancelled).
    Picked {
        kind: PickKind,
        path: Option<PathBuf>,
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
    pub adjust: Option<scan_detect::Options>,
    /// Options of the job's source, for the pre-scan's window and resolution.
    pub source_caps: Option<scan_core::SourceCaps>,
}

#[derive(Clone)]
pub struct Reporter {
    pub tx: Sender<Msg>,
    pub ctx: Context,
}

impl Reporter {
    fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
        self.ctx.request_repaint();
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

/// Decodes an image for the editor on a thread: the preview (at most the editor's preview size)
/// and the size of the file.
pub fn editor_load(r: Reporter, path: PathBuf) {
    std::thread::spawn(move || {
        let result = scan_pipeline::read_image(&path)
            .map(|full| {
                let size = [full.width, full.height];
                (downscale(&full, crate::editor::PREVIEW_SIDE), size)
            })
            .map_err(|e| format!("cannot read {}: {e}", path.display()));
        r.send(Msg::EditorLoaded { path, result });
    });
}

/// Runs a file operation of the editor (apply, backup, restore) on a thread. A successful apply
/// also writes the strip thumbnail into `thumb_cache`.
pub fn editor_op(
    r: Reporter,
    root: PathBuf,
    path: PathBuf,
    op: crate::editor::EditorOp,
    thumb_cache: PathBuf,
) {
    std::thread::spawn(move || {
        let what = op.name();
        let result = run_editor_op(&root, &path, op, &thumb_cache);
        r.send(Msg::EditorDone { path, what, result });
    });
}

/// The work of [`editor_op`], on the calling thread.
pub fn run_editor_op(
    root: &std::path::Path,
    path: &std::path::Path,
    op: crate::editor::EditorOp,
    thumb_cache: &std::path::Path,
) -> Result<(), String> {
    use crate::editor::EditorOp;
    match op {
        EditorOp::Backup => crate::edit::backup_original(root, path),
        EditorOp::Restore => crate::edit::restore_original(root, path),
        EditorOp::Apply {
            edits,
            backup_first,
        } => {
            if backup_first {
                crate::edit::backup_original(root, path)?;
            }
            let img = crate::editor::apply_file(path, &edits)?;
            // Best effort: the strip shows the new thumbnail without decoding the big file.
            thumbs::write_cache(thumb_cache, path, STRIP_THUMB, &img);
            Ok(())
        }
    }
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
        if let Some(opts) = &req.adjust {
            let fit = scan_detect::adjust_before_scan(
                scanner,
                &job,
                req.source_caps.as_ref(),
                opts,
                cancel,
            )
            .map_err(with_frame)?;
            job.area = fit.area;
            emit(Msg::Adjusted {
                index: i,
                shift_mm: fit.shift_mm,
                confidence: fit.confidence,
                moved: fit.moved,
            });
        }
        let raw = scanner.scan(&job, &progress, cancel).map_err(with_frame)?;

        let pipeline = Pipeline::default()
            .with(Rotate(req.rotate))
            .with(ToneStage(ToneOptions {
                adjust: frame.adjust,
                ..req.tone
            }));
        let img = pipeline.run(raw).map_err(with_frame)?;

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
    }
    Ok(())
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
            let path = scan_archive::folder_target(dir, stem, format.ext())?;
            scan_pipeline::write_image_depth(img, &path, depth)?;
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
            scan_pipeline::write_image_depth(img, &target.abs, depth)?;
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
    use scan_core::{ColorMode, Progress, RawImage, Result};
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
            RawImage::new(2, 1, 1, vec![0, 65535])
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
                area: area(0.0),
            },
            frames: (0..n)
                .map(|i| Frame {
                    area: area(i as f64 * 20.0),
                    stem: scan_archive::naming::frame_stem("frame", 5 + i as u32),
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
    fn the_chosen_bit_depth_decides_what_is_written() {
        for (depth, bits) in [(BitDepth::Eight, 8), (BitDepth::Sixteen, 16)] {
            let dir = tempfile::tempdir().unwrap();
            let mut req = request(dir.path(), 1);
            req.depth = depth;
            let (res, saved) = run(&fake(), &req, &CancelToken::default());
            res.unwrap();
            let file = dir.path().join(&saved[0]);
            assert_eq!(
                scan_pipeline::file_depth(&file).map(BitDepth::bits),
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
        assert_eq!(scan_pipeline::file_depth(&file), Some(BitDepth::Eight));
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
    use scan_core::{ColorMode, Progress, RawImage, Result};
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
            RawImage::new(w, h, 1, data)
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
            adjust: adjust.then(scan_detect::Options::default),
            source_caps: None,
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
            RawImage::new(2, 1, 1, vec![32768, 32768])
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
    use scan_core::{Progress, Result};

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
            RawImage::new(2, 1, 1, vec![100, 60000])
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
