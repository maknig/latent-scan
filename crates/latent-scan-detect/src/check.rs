//! Where a finished scan really lies on the preview it was placed on.
//!
//! The scan is compared with the overview around the area that was asked for: its luma is
//! shrunk to the overview's resolution and matched against the overview over a small shift and
//! a small scale along each axis (normalized cross-correlation, coarse to fine). A scale of 1.008
//! along y means the scan covers 0.8 % more length than the preview frame showed.

use latent_scan_core::{RawImage, ScanArea};

/// Largest shift searched (mm) and largest scale deviation.
const SEARCH_MM: f64 = 2.0;
const SCALE_RANGE: f64 = 0.03;
/// Samples used while searching; the match is evaluated on a thinned grid.
const SEARCH_SAMPLES: usize = 40_000;
/// Matches below this correlation are not reported.
const MIN_CORRELATION: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameCheck {
    /// Size of the scan from its pixels (mm).
    pub got_mm: (f64, f64),
    /// Where the scan lies relative to the asked area (mm, +x right, +y down), if it could be
    /// matched.
    pub shift_mm: Option<(f64, f64)>,
    /// Length covered by the scan relative to the asked size, per axis (1.0 = as asked).
    pub scale: Option<(f64, f64)>,
    /// Correlation of the best match (0..1).
    pub correlation: f32,
}

fn luma(img: &RawImage) -> Vec<f32> {
    if img.channels == 3 {
        img.data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|p| 0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32)
            .collect()
    } else {
        img.data.iter().map(|&v| v as f32).collect()
    }
}

/// The luma of `img` averaged down to `tw` x `th` cells (every source pixel is read once, nothing
/// large is allocated: scans are tens of megapixels).
fn shrink(img: &RawImage, tw: usize, th: usize) -> Vec<f32> {
    let (w, h, n) = (
        img.width as usize,
        img.height as usize,
        img.channels as usize,
    );
    let luma_at = |x: usize, y: usize| -> f32 {
        let p = &img.data[(y * w + x) * n..(y * w + x + 1) * n];
        if n == 3 {
            0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32
        } else {
            p[0] as f32
        }
    };
    let mut out = Vec::with_capacity(tw * th);
    for ty in 0..th {
        let (y0, y1) = (ty * h / th, ((ty + 1) * h / th).max(ty * h / th + 1).min(h));
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, ((tx + 1) * w / tw).max(tx * w / tw + 1).min(w));
            let mut sum = 0f64;
            for y in y0..y1 {
                for x in x0..x1 {
                    sum += luma_at(x, y) as f64;
                }
            }
            out.push((sum / ((x1 - x0) * (y1 - y0)) as f64) as f32);
        }
    }
    out
}

/// Bilinear value of a plane at fractional pixel coordinates (edges clamped).
fn sample(plane: &[f32], w: usize, h: usize, x: f64, y: f64) -> f32 {
    let x = x.clamp(0.0, w as f64 - 1.0);
    let y = y.clamp(0.0, h as f64 - 1.0);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (tx, ty) = ((x - x0 as f64) as f32, (y - y0 as f64) as f32);
    let top = plane[y0 * w + x0] * (1.0 - tx) + plane[y0 * w + x1] * tx;
    let bottom = plane[y1 * w + x0] * (1.0 - tx) + plane[y1 * w + x1] * tx;
    top * (1.0 - ty) + bottom * ty
}

/// The pose being searched: shift (mm) and scale of the scan on the overview.
#[derive(Debug, Clone, Copy)]
struct Pose {
    dx: f64,
    dy: f64,
    sx: f64,
    sy: f64,
}

struct Matcher<'a> {
    overview: &'a [f32],
    ow: usize,
    oh: usize,
    /// Overview pixels per mm.
    ppm: (f64, f64),
    area: ScanArea,
    asked: ScanArea,
    /// The shrunk scan, `tw` x `th`.
    target: Vec<f32>,
    tw: usize,
    th: usize,
    /// Sampling stride over the target while searching.
    stride: usize,
}

impl Matcher<'_> {
    /// Normalized cross-correlation of the target with the overview under `pose`.
    fn score(&self, pose: Pose) -> f32 {
        let (cx, cy) = (
            self.asked.x + self.asked.width / 2.0 + pose.dx,
            self.asked.y + self.asked.height / 2.0 + pose.dy,
        );
        let (mut n, mut sa, mut sb, mut saa, mut sbb, mut sab) =
            (0f64, 0f64, 0f64, 0f64, 0f64, 0f64);
        for ty in (0..self.th).step_by(self.stride) {
            let v = (ty as f64 + 0.5) / self.th as f64 - 0.5;
            let y_mm = cy + v * self.asked.height * pose.sy;
            let oy = (y_mm - self.area.y) * self.ppm.1 - 0.5;
            for tx in (0..self.tw).step_by(self.stride) {
                let u = (tx as f64 + 0.5) / self.tw as f64 - 0.5;
                let x_mm = cx + u * self.asked.width * pose.sx;
                let ox = (x_mm - self.area.x) * self.ppm.0 - 0.5;
                let a = self.target[ty * self.tw + tx] as f64;
                let b = sample(self.overview, self.ow, self.oh, ox, oy) as f64;
                n += 1.0;
                sa += a;
                sb += b;
                saa += a * a;
                sbb += b * b;
                sab += a * b;
            }
        }
        let (va, vb) = (saa - sa * sa / n, sbb - sb * sb / n);
        if va <= 1e-6 || vb <= 1e-6 {
            return 0.0;
        }
        ((sab - sa * sb / n) / (va * vb).sqrt()) as f32
    }
}

/// Compares `scan` (taken at `dpi` for the area `asked`) with the `overview` that covers
/// `overview_area`. The position and scale are `None` when the picture has too little structure
/// to be matched.
pub fn compare_with_overview(
    scan: &RawImage,
    asked: ScanArea,
    dpi: u32,
    overview: &RawImage,
    overview_area: ScanArea,
) -> FrameCheck {
    let mm = |px: u32| px as f64 / dpi.max(1) as f64 * 25.4;
    let mut check = FrameCheck {
        got_mm: (mm(scan.width), mm(scan.height)),
        shift_mm: None,
        scale: None,
        correlation: 0.0,
    };
    let (ow, oh) = (overview.width as usize, overview.height as usize);
    if scan.width == 0
        || scan.height == 0
        || ow == 0
        || oh == 0
        || asked.width <= 0.0
        || asked.height <= 0.0
        || overview_area.width <= 0.0
        || overview_area.height <= 0.0
    {
        return check;
    }
    let ppm = (
        ow as f64 / overview_area.width,
        oh as f64 / overview_area.height,
    );
    // The scan at the overview's resolution; a frame this small is searched in no time.
    let tw = ((asked.width * ppm.0).round() as usize).clamp(8, 4000);
    let th = ((asked.height * ppm.1).round() as usize).clamp(8, 4000);
    let target = shrink(scan, tw, th);
    let plane = luma(overview);
    let stride = (((tw * th) as f64 / SEARCH_SAMPLES as f64).sqrt().ceil() as usize).max(1);
    let m = Matcher {
        overview: &plane,
        ow,
        oh,
        ppm,
        area: overview_area,
        asked,
        target,
        tw,
        th,
        stride,
    };

    // Coarse: shift only, at the asked scale.
    let mut best = Pose {
        dx: 0.0,
        dy: 0.0,
        sx: 1.0,
        sy: 1.0,
    };
    let mut best_score = m.score(best);
    let step = 0.25;
    let n = (SEARCH_MM / step) as i32;
    for iy in -n..=n {
        for ix in -n..=n {
            let pose = Pose {
                dx: ix as f64 * step,
                dy: iy as f64 * step,
                ..best
            };
            let s = m.score(pose);
            if s > best_score {
                (best, best_score) = (pose, s);
            }
        }
    }
    // Fine: all four parameters, steps halving.
    let (mut shift_step, mut scale_step) = (step, 0.01);
    while shift_step > 0.008 {
        let mut improved = true;
        while improved {
            improved = false;
            for which in 0..4 {
                for sign in [-1.0, 1.0] {
                    let mut pose = best;
                    match which {
                        0 => pose.dx += sign * shift_step,
                        1 => pose.dy += sign * shift_step,
                        2 => pose.sx += sign * scale_step,
                        _ => pose.sy += sign * scale_step,
                    }
                    if pose.dx.abs() > SEARCH_MM + step
                        || pose.dy.abs() > SEARCH_MM + step
                        || (pose.sx - 1.0).abs() > SCALE_RANGE
                        || (pose.sy - 1.0).abs() > SCALE_RANGE
                    {
                        continue;
                    }
                    let s = m.score(pose);
                    if s > best_score + 1e-5 {
                        (best, best_score) = (pose, s);
                        improved = true;
                    }
                }
            }
        }
        shift_step /= 2.0;
        scale_step /= 2.0;
    }
    check.correlation = best_score;
    if best_score >= MIN_CORRELATION {
        check.shift_mm = Some((best.dx, best.dy));
        check.scale = Some((best.sx, best.sy));
    }
    check
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A smooth picture of the bed: a function of the position in mm.
    fn bed(x: f64, y: f64) -> u16 {
        let v = 30000.0
            + 8000.0 * (x * 0.35).sin()
            + 6000.0 * (y * 0.5 + 1.0).sin()
            + 5000.0 * ((x + y) * 0.21).sin()
            + if x > 42.0 { 7000.0 } else { 0.0 }
            + if y > 61.0 { -6000.0 } else { 0.0 };
        v.clamp(0.0, 65535.0) as u16
    }

    /// `area` of the bed sampled at `ppm` pixels per mm.
    fn picture(area: ScanArea, ppm: f64) -> RawImage {
        let (w, h) = (
            (area.width * ppm).round() as u32,
            (area.height * ppm).round() as u32,
        );
        let mut data = Vec::new();
        for y in 0..h {
            for x in 0..w {
                data.push(bed(
                    area.x + (x as f64 + 0.5) / ppm,
                    area.y + (y as f64 + 0.5) / ppm,
                ));
            }
        }
        RawImage::new(w, h, 1, data).unwrap()
    }

    fn area(x: f64, y: f64, w: f64, h: f64) -> ScanArea {
        ScanArea {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn a_scan_of_exactly_the_asked_area_matches_without_shift_or_scale() {
        let overview_area = area(0.0, 0.0, 100.0, 100.0);
        let overview = picture(overview_area, 3.0);
        let asked = area(20.0, 25.0, 40.0, 50.0);
        let scan = picture(asked, 6.0);
        let c = compare_with_overview(&scan, asked, 152, &overview, overview_area);
        let (dx, dy) = c.shift_mm.expect("matched");
        let (sx, sy) = c.scale.unwrap();
        assert!(dx.abs() < 0.05 && dy.abs() < 0.05, "{dx} {dy}");
        assert!(
            (sx - 1.0).abs() < 0.003 && (sy - 1.0).abs() < 0.003,
            "{sx} {sy}"
        );
        assert!(c.correlation > 0.95);
    }

    #[test]
    fn a_shifted_and_longer_scan_is_measured() {
        let overview_area = area(0.0, 0.0, 100.0, 100.0);
        let overview = picture(overview_area, 3.0);
        let asked = area(20.0, 25.0, 40.0, 50.0);
        // The scanner covered 0.6 mm to the right, 0.4 mm up and 1.2 % more length along y.
        let (cx, cy) = (40.0 + 0.6, 50.0 - 0.4);
        let real = area(cx - 20.0, cy - 25.0 * 1.012, 40.0, 50.0 * 1.012);
        let scan = picture(real, 6.0);
        let c = compare_with_overview(&scan, asked, 152, &overview, overview_area);
        let (dx, dy) = c.shift_mm.expect("matched");
        let (sx, sy) = c.scale.unwrap();
        assert!(
            (dx - 0.6).abs() < 0.08 && (dy + 0.4).abs() < 0.08,
            "{dx} {dy}"
        );
        assert!((sx - 1.0).abs() < 0.004, "{sx}");
        assert!((sy - 1.012).abs() < 0.004, "{sy}");
    }

    #[test]
    fn the_size_comes_from_the_pixels() {
        let overview_area = area(0.0, 0.0, 100.0, 100.0);
        let overview = picture(overview_area, 3.0);
        let asked = area(20.0, 25.0, 40.0, 50.0);
        let scan = RawImage::new(24, 30, 1, vec![30000; 24 * 30]).unwrap();
        let c = compare_with_overview(&scan, asked, 25, &overview, overview_area);
        assert!((c.got_mm.0 - 24.384).abs() < 1e-6 && (c.got_mm.1 - 30.48).abs() < 1e-6);
        assert_eq!(
            (c.shift_mm, c.scale),
            (None, None),
            "a flat picture cannot be matched"
        );
    }
}
