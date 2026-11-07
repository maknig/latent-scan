//! Frames on the scan bed: conversion between the picture's normalized crop rectangles and
//! millimetres on the scanner.

use latent_image_edit_ui::crop::{Crop, FULL, MIN};
use latent_scan_core::ScanArea;

/// The part of `area` (mm) covered by `crop`, rounded to 0.01 mm.
pub fn crop_to_mm(area: ScanArea, crop: Crop) -> ScanArea {
    let r = |v: f64| (v * 100.0).round() / 100.0;
    ScanArea {
        x: r(area.x + crop[0] as f64 * area.width),
        y: r(area.y + crop[1] as f64 * area.height),
        width: r((crop[2] - crop[0]) as f64 * area.width),
        height: r((crop[3] - crop[1]) as f64 * area.height),
    }
}

/// Inverse of [`crop_to_mm`]: where `area` lies within `window`, clamped to the window.
pub fn mm_to_crop(window: ScanArea, area: ScanArea) -> Crop {
    if window.width <= 0.0 || window.height <= 0.0 {
        return FULL;
    }
    let fx = |mm: f64| ((mm - window.x) / window.width) as f32;
    let fy = |mm: f64| ((mm - window.y) / window.height) as f32;
    let l = fx(area.x).clamp(0.0, 1.0 - MIN);
    let t = fy(area.y).clamp(0.0, 1.0 - MIN);
    let r = fx(area.x + area.width).clamp(l + MIN, 1.0);
    let b = fy(area.y + area.height).clamp(t + MIN, 1.0);
    [l, t, r, b]
}

#[cfg(test)]
mod tests {
    use super::*;
    use latent_image_edit_ui::crop::FULL;

    #[test]
    fn full_crop_is_the_whole_area() {
        let a = ScanArea {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 50.0,
        };
        assert_eq!(crop_to_mm(a, FULL), a);
    }

    #[test]
    fn crop_maps_into_the_area() {
        let a = ScanArea {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 50.0,
        };
        let c = crop_to_mm(a, [0.25, 0.5, 0.75, 1.0]);
        assert_eq!(
            c,
            ScanArea {
                x: 35.0,
                y: 45.0,
                width: 50.0,
                height: 25.0
            }
        );
    }

    #[test]
    fn mm_to_crop_inverts_crop_to_mm() {
        let window = ScanArea {
            x: 0.0,
            y: 0.0,
            width: 216.0,
            height: 297.0,
        };
        let c = [0.25, 0.1, 0.5, 0.9];
        let back = mm_to_crop(window, crop_to_mm(window, c));
        for (a, b) in c.iter().zip(back) {
            assert!((a - b).abs() < 1e-3, "{c:?} vs {back:?}");
        }
    }

    #[test]
    fn mm_to_crop_clamps_areas_outside_the_window() {
        let window = ScanArea {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let outside = ScanArea {
            x: 90.0,
            y: -10.0,
            width: 50.0,
            height: 30.0,
        };
        let c = mm_to_crop(window, outside);
        assert!(c.iter().all(|v| (0.0..=1.0).contains(v)));
        assert!(c[2] - c[0] >= MIN - 1e-6 && c[3] - c[1] >= MIN - 1e-6);
    }
}
