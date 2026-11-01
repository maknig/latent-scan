//! Thumbnails for the image strip: decoded on worker threads, kept as textures, and cached on
//! disk in `<archive>/.thumbs/<size>/<hash>.png` using the same file names as latent's
//! `images.rs`, so each program can reuse the other's thumbnails (a hash mismatch only costs a
//! re-decode). Extract a shared helper when merging into latent.

use egui::{ColorImage, Context, TextureHandle};
use scan_core::RawImage;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Sizes up to this are cached on disk (as in latent); larger ones are decoded every time.
const DISK_CACHE_MAX: u32 = 512;
const WORKERS: usize = 2;

pub enum Thumb {
    Loading,
    Ready(TextureHandle),
    Missing,
}

type Key = (PathBuf, u32);

struct Job {
    key: Key,
    cache_dir: PathBuf,
}

pub struct Thumbs {
    /// Disk cache folder: `<archive>/.thumbs` for a latent archive, a user cache folder otherwise.
    cache_dir: PathBuf,
    jobs: Sender<Job>,
    results: Receiver<(Key, Option<ColorImage>)>,
    cache: HashMap<Key, Thumb>,
}

impl Thumbs {
    pub fn new(ctx: &Context, cache_dir: PathBuf) -> Self {
        let (jobs, job_rx) = channel::<Job>();
        let (res_tx, results) = channel();
        let job_rx = Arc::new(Mutex::new(job_rx));
        for _ in 0..WORKERS {
            let (job_rx, res_tx, ctx) = (job_rx.clone(), res_tx.clone(), ctx.clone());
            std::thread::spawn(move || loop {
                // Hold the lock only while waiting for the next job.
                let job = match job_rx.lock().map(|r| r.recv()) {
                    Ok(Ok(job)) => job,
                    _ => return,
                };
                let image = load(&job.key.0, job.key.1, Some(&job.cache_dir));
                if res_tx.send((job.key, image)).is_err() {
                    return;
                }
                ctx.request_repaint();
            });
        }
        Self {
            cache_dir,
            jobs,
            results,
            cache: HashMap::new(),
        }
    }

    /// Turns finished decodes into textures; call once per frame.
    pub fn poll(&mut self, ctx: &Context) {
        while let Ok((key, image)) = self.results.try_recv() {
            let name = format!("thumb:{}:{}", key.0.display(), key.1);
            let thumb = match image {
                Some(im) => Thumb::Ready(ctx.load_texture(name, im, Default::default())),
                None => Thumb::Missing,
            };
            self.cache.insert(key, thumb);
        }
    }

    /// The thumbnail of `path` fitting `size` px, queued for decoding on first use.
    pub fn get(&mut self, path: &Path, size: u32) -> &Thumb {
        let key = (path.to_path_buf(), size);
        if !self.cache.contains_key(&key) {
            let job = Job {
                key: key.clone(),
                cache_dir: self.cache_dir.clone(),
            };
            let queued = self.jobs.send(job).is_ok();
            let state = if queued {
                Thumb::Loading
            } else {
                Thumb::Missing
            };
            self.cache.insert(key.clone(), state);
        }
        &self.cache[&key]
    }

    /// Switches to another cache folder (another destination); cached textures are dropped.
    pub fn set_cache_dir(&mut self, dir: PathBuf) {
        if self.cache_dir != dir {
            self.cache_dir = dir;
            self.cache.clear();
        }
    }

    /// Drops every cached size of `path` (after the file was edited).
    pub fn forget(&mut self, path: &Path) {
        self.cache.retain(|(p, _), _| p != path);
    }
}

/// Cache file for `path` at `size`; changes when the file's length or mtime change. Same key as
/// latent's `cache_file`.
pub fn cache_file(dir: &Path, path: &Path, size: u32) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (path, meta.len(), mtime.as_nanos()).hash(&mut h);
    Some(
        dir.join(size.to_string())
            .join(format!("{:016x}.png", h.finish())),
    )
}

fn to_color_image(im: &image::RgbaImage) -> ColorImage {
    ColorImage::from_rgba_unmultiplied([im.width() as usize, im.height() as usize], im.as_raw())
}

/// Decodes `path` (any size, any bit depth) to a thumbnail fitting `size`, using and filling the
/// disk cache for small sizes.
pub fn load(path: &Path, size: u32, cache_dir: Option<&Path>) -> Option<ColorImage> {
    let cached = (size <= DISK_CACHE_MAX)
        .then(|| cache_dir.and_then(|d| cache_file(d, path, size)))
        .flatten();
    if let Some(c) = &cached {
        if let Ok(im) = image::open(c) {
            return Some(to_color_image(&im.to_rgba8()));
        }
    }
    let mut reader = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?;
    // Full-resolution 16-bit scans exceed the decoder's default allocation limit.
    reader.no_limits();
    let im = reader.decode().ok()?;
    let im = if im.width() > size || im.height() > size {
        im.thumbnail(size, size)
    } else {
        im
    }
    .to_rgba8();
    if let Some(c) = cached {
        store(&c, &im);
    }
    Some(to_color_image(&im))
}

/// Best effort: a failed cache write only costs a re-decode later.
fn store(file: &Path, im: &image::RgbaImage) {
    let Some(dir) = file.parent() else { return };
    let tmp = file.with_extension("tmp");
    if std::fs::create_dir_all(dir).is_ok()
        && im.save_with_format(&tmp, image::ImageFormat::Png).is_ok()
    {
        let _ = std::fs::rename(&tmp, file);
    }
}

/// Writes the `size` px cache thumbnail of a just-saved scan from the in-memory image, so the
/// strip shows it at once without decoding the large file again. `saved` must exist already
/// (its length and mtime are part of the key).
pub fn write_cache(cache_dir: &Path, saved: &Path, size: u32, img: &RawImage) {
    let Some(file) = cache_file(cache_dir, saved, size) else {
        return;
    };
    // Cheap pre-shrink, then a proper box-filtered reduction.
    let small = crate::preview::downscale(img, size.saturating_mul(4));
    let (w, h) = (small.width, small.height);
    let dynamic = match small.channels {
        1 => image::ImageBuffer::<image::Luma<u16>, _>::from_raw(w, h, small.data)
            .map(image::DynamicImage::ImageLuma16),
        _ => image::ImageBuffer::<image::Rgb<u16>, _>::from_raw(w, h, small.data)
            .map(image::DynamicImage::ImageRgb16),
    };
    if let Some(d) = dynamic {
        store(&file, &d.thumbnail(size, size).to_rgba8());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("scan-thumbs-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_rgb16(path: &Path, w: u32, h: u32) {
        let data: Vec<u16> = (0..w * h * 3).map(|i| (i % 65535) as u16).collect();
        let img = RawImage::new(w, h, 3, data).unwrap();
        scan_pipeline::write_image(&img, path).unwrap();
    }

    #[test]
    fn cache_files_live_in_size_folders_and_follow_the_file() {
        let d = dir("key");
        let f = d.join("a.png");
        write_rgb16(&f, 4, 4);
        let thumbs = d.join(".thumbs");
        let a = cache_file(&thumbs, &f, 320).unwrap();
        assert_eq!(a.parent().unwrap(), thumbs.join("320"));
        assert_eq!(a.extension().unwrap(), "png");
        assert_eq!(a.file_stem().unwrap().len(), 16, "16 hex digits");
        assert_ne!(a, cache_file(&thumbs, &f, 160).unwrap());
        assert_eq!(a, cache_file(&thumbs, &f, 320).unwrap(), "stable");

        write_rgb16(&f, 5, 5); // different length (and mtime)
        assert_ne!(a, cache_file(&thumbs, &f, 320).unwrap());
        assert!(cache_file(&thumbs, &d.join("missing.png"), 320).is_none());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn loads_and_shrinks_a_16_bit_image_and_fills_the_cache() {
        let d = dir("load");
        let f = d.join("big.png");
        write_rgb16(&f, 400, 200);
        let thumbs = d.join(".thumbs");
        let im = load(&f, 100, Some(&thumbs)).unwrap();
        assert_eq!(im.size, [100, 50]);
        let cached = cache_file(&thumbs, &f, 100).unwrap();
        assert!(cached.exists(), "first load writes the cache");

        // The cached file is what a second load returns, even if the original vanishes.
        std::fs::rename(&f, d.join("moved.png")).unwrap();
        assert!(
            load(&f, 100, Some(&thumbs)).is_none(),
            "cache key needs the file"
        );
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn small_images_are_not_enlarged_and_large_sizes_skip_the_disk_cache() {
        let d = dir("small");
        let f = d.join("s.png");
        write_rgb16(&f, 30, 20);
        let thumbs = d.join(".thumbs");
        assert_eq!(load(&f, 600, Some(&thumbs)).unwrap().size, [30, 20]);
        assert!(
            !thumbs.join("600").exists(),
            "sizes above the limit are not cached"
        );
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn unreadable_files_give_none() {
        let d = dir("bad");
        let f = d.join("x.png");
        std::fs::write(&f, b"not an image").unwrap();
        assert!(load(&f, 100, None).is_none());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn a_saved_scan_gets_its_cache_thumbnail_without_decoding_it() {
        let d = dir("seed");
        let f = d.join("scan.png");
        let data: Vec<u16> = (0..600 * 300).map(|i| (i % 65535) as u16).collect();
        let img = RawImage::new(600, 300, 1, data).unwrap();
        scan_pipeline::write_image(&img, &f).unwrap();
        write_cache(&d.join(".thumbs"), &f, 320, &img);
        let cached = cache_file(&d.join(".thumbs"), &f, 320).unwrap();
        let im = image::open(cached).unwrap();
        assert_eq!((im.width(), im.height()), (320, 160));
        std::fs::remove_dir_all(d).ok();
    }
}

#[cfg(test)]
mod manual {
    /// Times the loader on a real large scan: `SCAN_THUMB_FILE=/path/to/scan.tiff cargo test
    /// -p scan-ui -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a real scan file"]
    fn time_a_large_scan() {
        let Ok(file) = std::env::var("SCAN_THUMB_FILE") else {
            return;
        };
        let path = std::path::Path::new(&file);
        for size in [320, 2048] {
            let start = std::time::Instant::now();
            let im = super::load(path, size, None).expect("decodes");
            println!("{size}px: {:?} in {:?}", im.size, start.elapsed());
        }
    }
}
