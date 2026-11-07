//! Red, green and blue histogram of the edited preview.

use egui::{Color32, ColorImage, Pos2, Rect, Stroke, Ui, Vec2};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Histogram {
    pub r: [u32; 256],
    pub g: [u32; 256],
    pub b: [u32; 256],
    /// All pixels have equal channels: drawn as one grey curve.
    pub gray: bool,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            r: [0; 256],
            g: [0; 256],
            b: [0; 256],
            gray: true,
        }
    }
}

impl Histogram {
    /// Counts the 8-bit channels of a display image.
    pub fn of(img: &ColorImage) -> Self {
        let mut h = Self::default();
        for p in &img.pixels {
            h.r[p.r() as usize] += 1;
            h.g[p.g() as usize] += 1;
            h.b[p.b() as usize] += 1;
            h.gray &= p.r() == p.g() && p.g() == p.b();
        }
        h
    }

    /// The height the curves are scaled to: the tallest bin *except* the two end bins, so a
    /// spike of clipped pixels at 0 or 255 does not flatten the rest. At least 1.
    pub fn scale(&self) -> u32 {
        [&self.r, &self.g, &self.b]
            .iter()
            .flat_map(|c| c[1..255].iter().copied())
            .max()
            .unwrap_or(1)
            .max(1)
    }

    /// Pixels counted (the same for every channel).
    #[cfg(test)]
    pub fn total(&self) -> u64 {
        self.r.iter().map(|&c| c as u64).sum()
    }

    /// Bar heights `0.0..=1.0` of a channel for the given scale (end-bin spikes are cut off).
    pub fn heights(channel: &[u32; 256], scale: u32) -> Vec<f32> {
        channel
            .iter()
            .map(|&c| (c as f32 / scale as f32).min(1.0))
            .collect()
    }
}

/// Draws the histogram in a box of `size`.
pub fn paint(ui: &mut Ui, h: &Histogram, size: Vec2) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, Color32::from_gray(18));
    let grid = Stroke::new(1.0_f32, Color32::from_white_alpha(25));
    for i in 1..4 {
        let x = rect.left() + rect.width() * i as f32 / 4.0;
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            grid,
        );
    }
    let scale = h.scale();
    let bar = rect.width() / 256.0;
    let channels: Vec<(&[u32; 256], Color32)> = if h.gray {
        vec![(&h.g, Color32::from_rgba_unmultiplied(210, 210, 210, 150))]
    } else {
        vec![
            (&h.r, Color32::from_rgba_unmultiplied(235, 60, 60, 110)),
            (&h.g, Color32::from_rgba_unmultiplied(60, 210, 80, 110)),
            (&h.b, Color32::from_rgba_unmultiplied(70, 110, 245, 110)),
        ]
    };
    for (channel, color) in channels {
        for (i, height) in Histogram::heights(channel, scale).into_iter().enumerate() {
            if height <= 0.0 {
                continue;
            }
            let x = rect.left() + i as f32 * bar;
            let bar_rect = Rect::from_min_max(
                Pos2::new(x, rect.bottom() - height * (rect.height() - 2.0)),
                Pos2::new(x + bar.max(1.0), rect.bottom()),
            );
            painter.rect_filled(bar_rect, 0.0, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(pixels: &[(u8, u8, u8)]) -> ColorImage {
        let rgb: Vec<u8> = pixels.iter().flat_map(|&(r, g, b)| [r, g, b]).collect();
        ColorImage::from_rgb([pixels.len(), 1], &rgb)
    }

    #[test]
    fn counts_every_pixel_in_every_channel() {
        let h = Histogram::of(&image(&[(0, 10, 255), (0, 20, 255), (7, 20, 1)]));
        assert_eq!(h.total(), 3);
        assert_eq!(h.g.iter().map(|&c| c as u64).sum::<u64>(), 3);
        assert_eq!(h.b.iter().map(|&c| c as u64).sum::<u64>(), 3);
        assert_eq!((h.r[0], h.r[7]), (2, 1));
        assert_eq!((h.g[10], h.g[20]), (1, 2));
        assert_eq!((h.b[255], h.b[1]), (2, 1));
    }

    #[test]
    fn grey_images_are_detected() {
        assert!(Histogram::of(&image(&[(5, 5, 5), (200, 200, 200)])).gray);
        assert!(!Histogram::of(&image(&[(5, 5, 5), (200, 201, 200)])).gray);
        assert!(
            Histogram::of(&ColorImage::from_rgb([0, 0], &[])).gray,
            "empty counts as grey"
        );
    }

    #[test]
    fn the_scale_ignores_the_end_bins_so_a_spike_does_not_flatten_the_rest() {
        let mut pixels = vec![(0, 0, 0); 1000];
        pixels.extend(vec![(100, 100, 100); 10]);
        pixels.extend(vec![(255, 255, 255); 500]);
        let h = Histogram::of(&image(&pixels));
        assert_eq!(h.scale(), 10, "set by the biggest of the inner bins");
        let heights = Histogram::heights(&h.r, h.scale());
        assert_eq!(
            (heights[0], heights[255]),
            (1.0, 1.0),
            "the spikes are cut off at full height"
        );
        assert_eq!(heights[100], 1.0);
        assert_eq!(heights[50], 0.0);
    }

    #[test]
    fn an_empty_histogram_has_a_usable_scale() {
        assert_eq!(Histogram::default().scale(), 1);
        assert!(Histogram::heights(&[0; 256], 1).iter().all(|&h| h == 0.0));
    }

    #[test]
    fn colour_images_keep_their_own_channel_curves() {
        let h = Histogram::of(&image(&[(255, 0, 0), (255, 0, 0)]));
        assert!(!h.gray);
        assert_eq!((h.r[255], h.g[0], h.b[0]), (2, 2, 2));
    }
}
