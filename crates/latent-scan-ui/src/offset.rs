//! The systematic offset between the overview and the full-resolution scans.
//!
//! The scanner starts a 2400 dpi scan a little away from where the 300 dpi preview started
//! (measured: a constant ~0.7 mm along the carriage axis), so a frame placed on the preview is
//! scanned slightly off. Every scan is measured against its preview (`compare_with_overview`);
//! the errors are collected per resolution pair and the median is applied, negated, to the next
//! scans. A manual offset on top lets the user fine-tune.

use latent_scan_core::Source;
use serde::{Deserialize, Serialize};

/// Offsets beyond this are not believed (a wrong match, not a scanner).
pub const LIMIT_MM: f64 = 3.0;
/// Matches below this correlation are not learned from.
pub const MIN_CORRELATION: f32 = 0.8;
/// How many of the latest measurements the median is taken over.
const KEEP: usize = 5;

/// What an offset belongs to: the source and the preview and scan resolutions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub source: Source,
    pub preview_dpi: u32,
    pub scan_dpi: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Learned {
    pub source: Source,
    pub preview_dpi: u32,
    pub scan_dpi: u32,
    /// The latest measured errors (x, y) in mm: where the scan lay minus where it was asked.
    pub errors: Vec<[f64; 2]>,
}

impl Learned {
    fn is(&self, key: Key) -> bool {
        (self.source, self.preview_dpi, self.scan_dpi)
            == (key.source, key.preview_dpi, key.scan_dpi)
    }

    /// The correction: minus the median error.
    pub fn offset(&self) -> (f64, f64) {
        let median = |axis: usize| {
            let mut v: Vec<f64> = self.errors.iter().map(|e| e[axis]).collect();
            v.sort_by(|a, b| a.total_cmp(b));
            match v.len() {
                0 => 0.0,
                n if n % 2 == 1 => v[n / 2],
                n => (v[n / 2 - 1] + v[n / 2]) / 2.0,
            }
        };
        (-median(0), -median(1))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OffsetSettings {
    /// Fine-tuning by hand (mm), added to the learned offset.
    pub manual: [f64; 2],
    pub learned: Vec<Learned>,
}

impl OffsetSettings {
    pub fn learned_for(&self, key: Key) -> Option<&Learned> {
        self.learned.iter().find(|l| l.is(key))
    }

    /// Learned plus manual offset (mm) for `key`.
    pub fn total(&self, key: Option<Key>) -> (f64, f64) {
        let learned = key
            .and_then(|k| self.learned_for(k))
            .map_or((0.0, 0.0), Learned::offset);
        (
            learned.0 + self.manual[0].clamp(-LIMIT_MM, LIMIT_MM),
            learned.1 + self.manual[1].clamp(-LIMIT_MM, LIMIT_MM),
        )
    }

    /// Remembers a measured error; returns whether it was believed.
    pub fn learn(&mut self, key: Key, error: (f64, f64), correlation: f32) -> bool {
        if correlation < MIN_CORRELATION
            || error.0.abs() > LIMIT_MM
            || error.1.abs() > LIMIT_MM
            || !error.0.is_finite()
            || !error.1.is_finite()
        {
            return false;
        }
        let entry = match self.learned.iter().position(|l| l.is(key)) {
            Some(i) => &mut self.learned[i],
            None => {
                self.learned.push(Learned {
                    source: key.source,
                    preview_dpi: key.preview_dpi,
                    scan_dpi: key.scan_dpi,
                    errors: vec![],
                });
                self.learned.last_mut().expect("just pushed")
            }
        };
        entry.errors.push([error.0, error.1]);
        if entry.errors.len() > KEEP {
            entry.errors.remove(0);
        }
        true
    }

    /// Forgets what was learned for `key`.
    pub fn forget(&mut self, key: Key) {
        self.learned.retain(|l| !l.is(key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(scan_dpi: u32) -> Key {
        Key {
            source: Source::Transparency,
            preview_dpi: 300,
            scan_dpi,
        }
    }

    #[test]
    fn the_offset_is_minus_the_median_of_the_good_matches() {
        let mut o = OffsetSettings::default();
        assert_eq!(o.total(Some(key(2400))), (0.0, 0.0));
        for e in [-0.70, -0.74, -0.72] {
            assert!(o.learn(key(2400), (0.08, e), 0.9));
        }
        let (x, y) = o.total(Some(key(2400)));
        assert!(
            (x + 0.08).abs() < 1e-9 && (y - 0.72).abs() < 1e-9,
            "{x} {y}"
        );
    }

    #[test]
    fn weak_or_implausible_matches_are_ignored() {
        let mut o = OffsetSettings::default();
        assert!(!o.learn(key(2400), (0.0, -0.7), 0.5), "weak");
        assert!(!o.learn(key(2400), (0.0, 5.0), 0.95), "implausible");
        assert!(!o.learn(key(2400), (f64::NAN, 0.0), 0.95));
        assert!(o.learned.is_empty());
    }

    #[test]
    fn only_the_latest_five_count_and_one_outlier_does_not_move_the_median() {
        let mut o = OffsetSettings::default();
        for e in [-0.7, -0.7, -0.7, -0.7, -0.7] {
            o.learn(key(2400), (0.0, e), 0.9);
        }
        o.learn(key(2400), (0.0, 1.9), 0.9);
        assert_eq!(o.learned_for(key(2400)).unwrap().errors.len(), 5);
        assert!((o.total(Some(key(2400))).1 - 0.7).abs() < 1e-9);
    }

    #[test]
    fn every_resolution_pair_has_its_own_offset_and_manual_adds_on_top() {
        let mut o = OffsetSettings::default();
        o.learn(key(2400), (0.0, -0.7), 0.9);
        assert_eq!(
            o.total(Some(key(4800))),
            (0.0, 0.0),
            "another dpi starts at zero"
        );
        o.manual = [0.0, 0.1];
        assert!((o.total(Some(key(2400))).1 - 0.8).abs() < 1e-9);
        assert!(
            (o.total(None).1 - 0.1).abs() < 1e-9,
            "no preview: manual only"
        );
        o.forget(key(2400));
        assert!((o.total(Some(key(2400))).1 - 0.1).abs() < 1e-9);
    }

    #[test]
    fn settings_without_offsets_still_load() {
        let o: OffsetSettings = toml::from_str("").unwrap();
        assert_eq!(o, OffsetSettings::default());
        let text = toml::to_string(&{
            let mut o = OffsetSettings::default();
            o.learn(key(2400), (0.0, -0.7), 0.9);
            o
        })
        .unwrap();
        let back: OffsetSettings = toml::from_str(&text).unwrap();
        assert_eq!(back.learned.len(), 1);
    }
}
