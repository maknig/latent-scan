//! Fits preset frame rectangles to the film actually lying in the holder.
//!
//! Holder and film shift a little between sessions and along a strip. Instead of detecting
//! lines (Hough), the known rectangle outline of each preset frame is moved within a small
//! window to where its four edges best coincide with strong brightness edges. The film moves as
//! a unit, so all frames of a strip first get one shared shift, then each is refined slightly.
//! Weak matches (dark scenes, blank holder) keep the preset instead of drifting.

use latent_scan_core::{
    CancelToken, ColorMode, RawImage, Result, ScanArea, ScanJob, Scanner, SourceCaps,
};

mod check;
pub use check::{compare_with_overview, FrameCheck};

/// Thickness (px) over which the gradient of one edge is averaged.
const EDGE_PX: i32 = 3;
/// Resolution used for the pre-scan of a single frame.
const PRESCAN_DPI: u32 = 600;
/// Pre-scan margin around the search window (mm), so every candidate outline fits inside.
const PRESCAN_MARGIN_MM: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    /// How far (mm) the whole strip may have moved in x and y.
    pub search_mm: f64,
    /// How far (mm) a single frame may deviate from the strip's shift.
    pub refine_mm: f64,
    /// Matches below this confidence (0..1) are ignored.
    pub min_confidence: f32,
    /// How far (mm) the preset lies inside the visible frame boundary; the outline that is
    /// matched is the preset grown by this, the result is shrunk again. 0 = on the boundary.
    pub inset_mm: f64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            search_mm: 3.0,
            refine_mm: 1.0,
            min_confidence: 0.25,
            inset_mm: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    /// The frame to scan (same size as the preset).
    pub area: ScanArea,
    /// Movement against the preset in mm.
    pub shift_mm: (f64, f64),
    pub confidence: f32,
    /// False when the match was too weak and the preset is kept.
    pub moved: bool,
}

/// Fits every preset frame. `img` covers `window` (mm on the bed).
pub fn fit_frames(
    img: &RawImage,
    window: ScanArea,
    presets: &[ScanArea],
    opts: &Options,
) -> Vec<Fit> {
    let keep = |p: &ScanArea| Fit {
        area: *p,
        shift_mm: (0.0, 0.0),
        confidence: 0.0,
        moved: false,
    };
    if presets.is_empty() || window.width <= 0.0 || window.height <= 0.0 || img.data.is_empty() {
        return presets.iter().map(keep).collect();
    }
    let (sx, sy) = (
        img.width as f64 / window.width,
        img.height as f64 / window.height,
    );
    let rects: Vec<PxRect> = presets
        .iter()
        .map(|p| {
            let i = opts.inset_mm;
            PxRect {
                x0: ((p.x - i - window.x) * sx).round() as i32,
                y0: ((p.y - i - window.y) * sy).round() as i32,
                x1: ((p.x + p.width + i - window.x) * sx).round() as i32,
                y1: ((p.y + p.height + i - window.y) * sy).round() as i32,
            }
        })
        .collect();
    let edges = Edges::new(img);
    let px = |mm: f64, s: f64| (mm * s).round() as i32;
    let (rx, ry) = (px(opts.search_mm, sx), px(opts.search_mm, sy));
    let (fx, fy) = (px(opts.refine_mm, sx), px(opts.refine_mm, sy));

    let group = edges.search(&rects, (0, 0), (rx, ry));
    let strip = group.filter(|g| g.conf >= opts.min_confidence);

    presets
        .iter()
        .zip(&rects)
        .map(|(preset, rect)| {
            let own = match strip {
                Some(g) => edges.search(std::slice::from_ref(rect), (g.dx, g.dy), (fx, fy)),
                None => edges.search(std::slice::from_ref(rect), (0, 0), (rx, ry)),
            };
            let chosen = match (own, strip) {
                (Some(o), _) if o.conf >= opts.min_confidence => Some(o),
                (_, Some(g)) => Some(g),
                _ => None,
            };
            match chosen {
                Some(m) => {
                    let shift = (m.dx as f64 / sx, m.dy as f64 / sy);
                    Fit {
                        area: ScanArea {
                            x: preset.x + shift.0,
                            y: preset.y + shift.1,
                            ..*preset
                        },
                        shift_mm: shift,
                        confidence: m.conf,
                        moved: m.dx != 0 || m.dy != 0,
                    }
                }
                None => Fit {
                    confidence: own.or(group).map_or(0.0, |m| m.conf),
                    ..keep(preset)
                },
            }
        })
        .collect()
}

/// [`fit_frames`] for one frame.
pub fn fit_frame(img: &RawImage, window: ScanArea, preset: ScanArea, opts: &Options) -> Fit {
    fit_frames(img, window, &[preset], opts)[0]
}

/// Pre-scans the surroundings of `job.area` at 600 dpi (8-bit) and fits the frame in it.
/// `caps` (of the job's source) limits the window to the scannable area and picks the dpi.
pub fn adjust_before_scan(
    scanner: &dyn Scanner,
    job: &ScanJob,
    caps: Option<&SourceCaps>,
    opts: &Options,
    cancel: &CancelToken,
) -> Result<Fit> {
    let m = opts.search_mm + PRESCAN_MARGIN_MM + opts.inset_mm;
    let mut window = ScanArea {
        x: (job.area.x - m).max(0.0),
        y: (job.area.y - m).max(0.0),
        width: 0.0,
        height: 0.0,
    };
    window.width = job.area.x + job.area.width + m - window.x;
    window.height = job.area.y + job.area.height + m - window.y;
    if let Some(max) = caps.and_then(|c| c.max_area) {
        window.width = window.width.min(max.width - window.x).max(1.0);
        window.height = window.height.min(max.height - window.y).max(1.0);
    }
    let dpi = caps
        .and_then(|c| c.resolution_at_least(PRESCAN_DPI))
        .unwrap_or(PRESCAN_DPI);
    let pre = ScanJob {
        area: window,
        dpi,
        preview: false,
        bit_depth: 8,
        color: ColorMode::Gray,
        ..job.clone()
    };
    let img = scanner.scan(&pre, &|_| {}, cancel)?;
    // The scanner may clip the window; the size follows from the pixels delivered.
    let delivered = ScanArea {
        width: img.width as f64 / dpi as f64 * 25.4,
        height: img.height as f64 / dpi as f64 * 25.4,
        ..window
    };
    Ok(fit_frame(&img, delivered, job.area, opts))
}

#[derive(Debug, Clone, Copy)]
struct PxRect {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

#[derive(Debug, Clone, Copy)]
struct Match {
    dx: i32,
    dy: i32,
    conf: f32,
}

/// Gradient magnitudes as prefix sums, so the mean along any straight edge is O(1).
struct Edges {
    w: i32,
    h: i32,
    /// `|gx|` accumulated along y, indexed `x * (h + 1) + y`.
    vert: Vec<f32>,
    /// `|gy|` accumulated along x, indexed `y * (w + 1) + x`.
    horiz: Vec<f32>,
    /// Mean clipped gradient of the whole image.
    mean: f32,
}

impl Edges {
    fn new(img: &RawImage) -> Self {
        let (w, h) = (img.width as usize, img.height as usize);
        let luma = blur(&luma(img), w, h);
        let at = |x: usize, y: usize| luma[y * w + x];
        let mut gx = vec![0f32; w * h];
        let mut gy = vec![0f32; w * h];
        for y in 1..h.saturating_sub(1) {
            for x in 1..w.saturating_sub(1) {
                gx[y * w + x] = (at(x + 1, y - 1) + 2.0 * at(x + 1, y) + at(x + 1, y + 1)
                    - at(x - 1, y - 1)
                    - 2.0 * at(x - 1, y)
                    - at(x - 1, y + 1))
                .abs();
                gy[y * w + x] = (at(x - 1, y + 1) + 2.0 * at(x, y + 1) + at(x + 1, y + 1)
                    - at(x - 1, y - 1)
                    - 2.0 * at(x, y - 1)
                    - at(x + 1, y - 1))
                .abs();
            }
        }
        // One very strong edge (the holder) must not dominate the score.
        let clip = clip_level(&gx, &gy).max(1e-6);
        let mut total = 0f64;
        for v in gx.iter_mut().chain(gy.iter_mut()) {
            *v = v.min(clip);
            total += *v as f64;
        }
        let mean = (total / (2 * w * h).max(1) as f64) as f32;

        let mut vert = vec![0f32; w * (h + 1)];
        for x in 0..w {
            for y in 0..h {
                vert[x * (h + 1) + y + 1] = vert[x * (h + 1) + y] + gx[y * w + x];
            }
        }
        let mut horiz = vec![0f32; h * (w + 1)];
        for y in 0..h {
            for x in 0..w {
                horiz[y * (w + 1) + x + 1] = horiz[y * (w + 1) + x] + gy[y * w + x];
            }
        }
        Self {
            w: w as i32,
            h: h as i32,
            vert,
            horiz,
            mean: mean.max(1e-9),
        }
    }

    /// Mean `|gx|` of a vertical edge at column `x` between rows `y0..y1`.
    fn v_edge(&self, x: i32, y0: i32, y1: i32) -> Option<f32> {
        let half = EDGE_PX / 2;
        if x - half < 0 || x + half >= self.w || y0 < 0 || y1 > self.h || y1 <= y0 {
            return None;
        }
        let sum: f32 = (x - half..=x + half)
            .map(|c| {
                let base = c as usize * (self.h as usize + 1);
                self.vert[base + y1 as usize] - self.vert[base + y0 as usize]
            })
            .sum();
        Some(sum / (EDGE_PX * (y1 - y0)) as f32)
    }

    /// Mean `|gy|` of a horizontal edge at row `y` between columns `x0..x1`.
    fn h_edge(&self, y: i32, x0: i32, x1: i32) -> Option<f32> {
        let half = EDGE_PX / 2;
        if y - half < 0 || y + half >= self.h || x0 < 0 || x1 > self.w || x1 <= x0 {
            return None;
        }
        let sum: f32 = (y - half..=y + half)
            .map(|r| {
                let base = r as usize * (self.w as usize + 1);
                self.horiz[base + x1 as usize] - self.horiz[base + x0 as usize]
            })
            .sum();
        Some(sum / (EDGE_PX * (x1 - x0)) as f32)
    }

    /// Mean strength of the four edges of `r` moved by `(dx, dy)`; `None` outside the image.
    fn outline(&self, r: &PxRect, dx: i32, dy: i32) -> Option<f32> {
        let (x0, y0, x1, y1) = (r.x0 + dx, r.y0 + dy, r.x1 + dx, r.y1 + dy);
        Some(
            (self.v_edge(x0, y0, y1)?
                + self.v_edge(x1, y0, y1)?
                + self.h_edge(y0, x0, x1)?
                + self.h_edge(y1, x0, x1)?)
                / 4.0,
        )
    }

    /// Best common shift of `rects` within `radius` of `centre`, with its confidence.
    fn search(&self, rects: &[PxRect], centre: (i32, i32), radius: (i32, i32)) -> Option<Match> {
        let mut best: Option<(f32, i32, i32)> = None;
        let (mut sum, mut n) = (0f64, 0u32);
        for dy in centre.1 - radius.1..=centre.1 + radius.1 {
            for dx in centre.0 - radius.0..=centre.0 + radius.0 {
                let Some(score) = rects
                    .iter()
                    .map(|r| self.outline(r, dx, dy))
                    .sum::<Option<f32>>()
                    .map(|s| s / rects.len() as f32)
                else {
                    continue;
                };
                sum += score as f64;
                n += 1;
                let dist = |a: i32, b: i32| (a - centre.0).abs() + (b - centre.1).abs();
                let better = match best {
                    None => true,
                    Some((s, bx, by)) => {
                        score > s + 1e-6 || (score >= s - 1e-6 && dist(dx, dy) < dist(bx, by))
                    }
                };
                if better {
                    best = Some((score, dx, dy));
                }
            }
        }
        let (score, dx, dy) = best?;
        let avg = (sum / n as f64) as f32;
        // How far the peak stands out of the search window, and how strong the edges are
        // compared with the image as a whole.
        let prominence = ((score - avg) / score.max(1e-9) / 0.4).clamp(0.0, 1.0);
        let strength = ((score / self.mean - 1.0) / 1.5).clamp(0.0, 1.0);
        Some(Match {
            dx,
            dy,
            conf: prominence * strength,
        })
    }
}

fn luma(img: &RawImage) -> Vec<f32> {
    let k = 1.0 / 65535.0;
    match img.channels {
        3 => img
            .data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|p| (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) * k)
            .collect(),
        n => img
            .data
            .iter()
            .step_by(n as usize)
            .map(|&v| v as f32 * k)
            .collect(),
    }
}

/// 3x3 box blur with clamped borders.
fn blur(src: &[f32], w: usize, h: usize) -> Vec<f32> {
    let pass = |src: &[f32], horizontal: bool| {
        let mut out = vec![0f32; src.len()];
        for y in 0..h {
            for x in 0..w {
                let (a, b) = if horizontal {
                    (x.saturating_sub(1), (x + 1).min(w - 1))
                } else {
                    (y.saturating_sub(1), (y + 1).min(h - 1))
                };
                let at = |i: usize| {
                    if horizontal {
                        src[y * w + i]
                    } else {
                        src[i * w + x]
                    }
                };
                let mid = if horizontal { x } else { y };
                out[y * w + x] = (at(a) + at(mid) + at(b)) / 3.0;
            }
        }
        out
    };
    pass(&pass(src, true), false)
}

/// Level above which gradients are clipped: the 90th percentile of the *strong* gradients
/// (those above 1% of the maximum). Taking it over all pixels would give 0 on a clean image
/// whose edges are a tiny fraction of the pixels.
fn clip_level(a: &[f32], b: &[f32]) -> f32 {
    let max = a.iter().chain(b).copied().fold(0.0, f32::max);
    let mut v: Vec<f32> = a
        .iter()
        .chain(b)
        .copied()
        .filter(|&g| g > 0.01 * max)
        .collect();
    if v.is_empty() {
        return max;
    }
    v.sort_by(|x, y| x.total_cmp(y));
    v[((v.len() - 1) as f64 * 0.9) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use latent_scan_core::{Capabilities, Progress, ScannerInfo, Source};

    /// Pixels per mm of all synthetic images.
    const PPM: f64 = 10.0;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> ScanArea {
        ScanArea {
            x,
            y,
            width: w,
            height: h,
        }
    }

    /// Renders `frames` (mm) as filled rectangles of value `fg` on `bg` into the image covering
    /// `window`, plus deterministic noise of the given amplitude.
    fn render(window: ScanArea, frames: &[ScanArea], bg: u16, fg: u16, noise: u32) -> RawImage {
        let (w, h) = (
            (window.width * PPM).round() as u32,
            (window.height * PPM).round() as u32,
        );
        let mut seed = 12345u32;
        let mut data = Vec::with_capacity((w * h) as usize);
        for py in 0..h {
            for px in 0..w {
                let (x, y) = (
                    window.x + px as f64 / PPM + 0.05,
                    window.y + py as f64 / PPM + 0.05,
                );
                let inside = frames
                    .iter()
                    .any(|f| x >= f.x && x < f.x + f.width && y >= f.y && y < f.y + f.height);
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let n = if noise == 0 { 0 } else { (seed >> 16) % noise };
                data.push(if inside { fg } else { bg }.saturating_add(n as u16));
            }
        }
        RawImage::new(w, h, 1, data).unwrap()
    }

    const WINDOW: ScanArea = ScanArea {
        x: 0.0,
        y: 0.0,
        width: 110.0,
        height: 40.0,
    };

    fn presets() -> Vec<ScanArea> {
        vec![
            rect(5.0, 10.0, 30.0, 20.0),
            rect(40.0, 10.0, 30.0, 20.0),
            rect(75.0, 10.0, 30.0, 20.0),
        ]
    }

    fn shifted(by: (f64, f64)) -> Vec<ScanArea> {
        presets()
            .iter()
            .map(|p| rect(p.x + by.0, p.y + by.1, p.width, p.height))
            .collect()
    }

    fn close(a: (f64, f64), b: (f64, f64), tol: f64) -> bool {
        (a.0 - b.0).abs() <= tol && (a.1 - b.1).abs() <= tol
    }

    #[test]
    fn finds_a_shifted_strip() {
        let img = render(WINDOW, &shifted((2.3, -1.1)), 5000, 40000, 800);
        let fits = fit_frames(&img, WINDOW, &presets(), &Options::default());
        for f in &fits {
            assert!(f.moved && f.confidence > 0.5, "{f:?}");
            assert!(close(f.shift_mm, (2.3, -1.1), 0.15), "{f:?}");
        }
        assert!((fits[1].area.x - 42.3).abs() < 0.15 && (fits[1].area.width - 30.0).abs() < 1e-9);
    }

    #[test]
    fn works_for_dark_frames_on_a_bright_base() {
        let img = render(WINDOW, &shifted((-1.7, 0.9)), 50000, 12000, 800);
        let fits = fit_frames(&img, WINDOW, &presets(), &Options::default());
        assert!(fits.iter().all(|f| close(f.shift_mm, (-1.7, 0.9), 0.15)));
    }

    #[test]
    fn an_unshifted_strip_stays_put() {
        let img = render(WINDOW, &presets(), 5000, 40000, 800);
        let fits = fit_frames(&img, WINDOW, &presets(), &Options::default());
        assert!(fits.iter().all(|f| !f.moved && f.confidence > 0.5));
    }

    #[test]
    fn a_featureless_frame_follows_the_strip() {
        // Only the first two frames exist in the image; the third is blank film.
        let mut frames = shifted((1.4, 0.6));
        frames.truncate(2);
        let img = render(WINDOW, &frames, 5000, 40000, 800);
        let fits = fit_frames(&img, WINDOW, &presets(), &Options::default());
        assert!(close(fits[2].shift_mm, (1.4, 0.6), 0.2), "{:?}", fits[2]);
    }

    #[test]
    fn one_frame_can_deviate_slightly_from_the_strip() {
        let mut frames = shifted((2.0, 0.0));
        frames[2].x += 0.6; // this frame sits 0.6 mm further right
        let img = render(WINDOW, &frames, 5000, 40000, 800);
        let fits = fit_frames(&img, WINDOW, &presets(), &Options::default());
        assert!(close(fits[0].shift_mm, (2.0, 0.0), 0.15), "{:?}", fits[0]);
        assert!(close(fits[2].shift_mm, (2.6, 0.0), 0.15), "{:?}", fits[2]);
    }

    #[test]
    fn noise_keeps_the_presets() {
        let img = render(WINDOW, &[], 20000, 20000, 6000);
        let fits = fit_frames(&img, WINDOW, &presets(), &Options::default());
        for (f, p) in fits.iter().zip(presets()) {
            assert!(!f.moved && f.confidence < 0.25, "{f:?}");
            assert_eq!(f.area, p);
        }
    }

    #[test]
    fn a_blank_image_keeps_the_presets() {
        let img = render(WINDOW, &[], 20000, 20000, 0);
        let fits = fit_frames(&img, WINDOW, &presets(), &Options::default());
        assert!(fits.iter().all(|f| !f.moved));
    }

    #[test]
    fn a_shift_beyond_the_search_window_is_not_followed() {
        let img = render(WINDOW, &shifted((6.0, 0.0)), 5000, 40000, 800);
        let fits = fit_frames(&img, WINDOW, &presets(), &Options::default());
        assert!(fits.iter().all(|f| f.shift_mm.0.abs() <= 3.0 + 0.11));
    }

    #[test]
    fn the_preset_may_lie_inside_the_visible_frame() {
        // Visible frames are shifted by (1.5, 0.5); the presets are 1 mm inside them.
        let img = render(WINDOW, &shifted((1.5, 0.5)), 5000, 40000, 800);
        let inner: Vec<ScanArea> = presets()
            .iter()
            .map(|p| rect(p.x + 1.0, p.y + 1.0, p.width - 2.0, p.height - 2.0))
            .collect();
        let opts = Options {
            inset_mm: 1.0,
            ..Options::default()
        };
        let fits = fit_frames(&img, WINDOW, &inner, &opts);
        for (f, p) in fits.iter().zip(&inner) {
            assert!(close(f.shift_mm, (1.5, 0.5), 0.15), "{f:?}");
            assert!((f.area.width - p.width).abs() < 1e-9, "size is kept");
        }
    }

    #[test]
    fn a_sub_window_gives_bed_coordinates() {
        let window = rect(50.0, 5.0, 60.0, 30.0);
        let truth = rect(77.0, 11.2, 30.0, 20.0);
        let img = render(window, &[truth], 5000, 40000, 800);
        let fit = fit_frame(
            &img,
            window,
            rect(75.0, 10.0, 30.0, 20.0),
            &Options::default(),
        );
        assert!(
            close((fit.area.x, fit.area.y), (77.0, 11.2), 0.15),
            "{fit:?}"
        );
    }

    #[test]
    fn candidates_outside_the_image_are_skipped() {
        // The frame touches the window border; the search must not panic or wrap.
        let window = rect(0.0, 0.0, 40.0, 25.0);
        let img = render(window, &[rect(1.0, 1.0, 36.0, 22.0)], 5000, 40000, 800);
        let fit = fit_frame(
            &img,
            window,
            rect(1.0, 1.0, 36.0, 22.0),
            &Options::default(),
        );
        assert!(fit.area.width > 0.0);
    }

    /// Scanner whose bed holds one bright frame at a fixed true position.
    struct Fake {
        info: ScannerInfo,
        truth: ScanArea,
    }

    impl Scanner for Fake {
        fn info(&self) -> &ScannerInfo {
            &self.info
        }
        fn capabilities(&self) -> Result<Capabilities> {
            Ok(Capabilities::default())
        }
        fn scan(&self, job: &ScanJob, _: Progress, _: &CancelToken) -> Result<RawImage> {
            // Render at the requested dpi; the tests use PPM-independent geometry.
            let ppm = job.dpi as f64 / 25.4;
            let (w, h) = (
                (job.area.width * ppm).round() as u32,
                (job.area.height * ppm).round() as u32,
            );
            let mut data = Vec::with_capacity((w * h) as usize);
            for py in 0..h {
                for px in 0..w {
                    let (x, y) = (job.area.x + px as f64 / ppm, job.area.y + py as f64 / ppm);
                    let t = &self.truth;
                    let inside = x >= t.x && x < t.x + t.width && y >= t.y && y < t.y + t.height;
                    data.push(if inside { 40000 } else { 5000 });
                }
            }
            Ok(RawImage::new(w, h, 1, data)?)
        }
    }

    #[test]
    fn adjust_before_scan_finds_the_true_position() {
        let fake = Fake {
            info: ScannerInfo {
                id: "f".into(),
                vendor: "v".into(),
                model: "m".into(),
            },
            truth: rect(62.4, 31.0, 36.0, 24.0),
        };
        let job = ScanJob {
            source: Source::Transparency,
            color: ColorMode::Color,
            bit_depth: 16,
            dpi: 4800,
            preview: false,
            area: rect(60.0, 30.0, 36.0, 24.0),
        };
        let caps = SourceCaps {
            source: Source::Transparency,
            resolutions: vec![300, 600, 1200, 4800],
            modes: vec![],
            depths: vec![],
            quick_preview: false,
            max_area: Some(rect(0.0, 0.0, 216.0, 250.0)),
        };
        let fit = adjust_before_scan(
            &fake,
            &job,
            Some(&caps),
            &Options::default(),
            &CancelToken::default(),
        )
        .unwrap();
        assert!(fit.moved, "{fit:?}");
        assert!(
            close((fit.area.x, fit.area.y), (62.4, 31.0), 0.2),
            "{fit:?}"
        );
    }
}
