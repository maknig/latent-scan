//! Settings remembered between sessions (`<config dir>/settings.toml`).

use scan_detect::Options;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Slider ranges of the auto-adjust values; also the limits applied when loading.
pub const SEARCH_MM: (f64, f64) = (0.5, 8.0);
pub const REFINE_MM: (f64, f64) = (0.0, 3.0);
pub const CONFIDENCE: (f32, f32) = (0.0, 1.0);
pub const MARGIN_MM: (f64, f64) = (0.0, 3.0);

/// The auto-adjust values the user can tune.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectSettings {
    pub search_mm: f64,
    pub refine_mm: f64,
    pub min_confidence: f32,
    pub inset_mm: f64,
}

impl Default for DetectSettings {
    fn default() -> Self {
        let d = Options::default();
        Self {
            search_mm: d.search_mm,
            refine_mm: d.refine_mm,
            min_confidence: d.min_confidence,
            inset_mm: d.inset_mm,
        }
    }
}

impl DetectSettings {
    /// Puts every value into its slider range; refine never exceeds search.
    pub fn clamped(mut self) -> Self {
        self.search_mm = self.search_mm.clamp(SEARCH_MM.0, SEARCH_MM.1);
        self.refine_mm = self
            .refine_mm
            .clamp(REFINE_MM.0, REFINE_MM.1)
            .min(self.search_mm);
        self.min_confidence = self.min_confidence.clamp(CONFIDENCE.0, CONFIDENCE.1);
        self.inset_mm = self.inset_mm.clamp(MARGIN_MM.0, MARGIN_MM.1);
        self
    }

    pub fn options(&self) -> Options {
        Options {
            search_mm: self.search_mm,
            refine_mm: self.refine_mm,
            min_confidence: self.min_confidence,
            inset_mm: self.inset_mm,
        }
    }
}

/// Where scans go.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SaveMode {
    /// Plain image files in a folder.
    #[default]
    Folder,
    /// A latent archive: `images/<table>/<id>/` plus the row's image list.
    Archive,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SaveSettings {
    pub mode: SaveMode,
    /// Folder for plain files; `None` means [`default_folder`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    /// Root of the latent archive last used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive: Option<PathBuf>,
    /// Save scans with 8 instead of 16 bits per sample.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub eight_bit: bool,
}

impl SaveSettings {
    pub fn folder_or_default(&self) -> PathBuf {
        self.folder.clone().unwrap_or_else(default_folder)
    }
}

/// `~/Pictures/latent-scan`; created when the first scan is saved.
pub fn default_folder() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join("Pictures/latent-scan")
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub detect: DetectSettings,
    pub save: SaveSettings,
    /// Resolution of the overview scan; `None` = the source's lowest one of at least 150 dpi.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_dpi: Option<u32>,
}

pub fn settings_path() -> Option<PathBuf> {
    Some(scan_profiles::config_dir()?.join("settings.toml"))
}

impl Settings {
    /// Loads the user's settings; a missing file gives the defaults silently, a broken one
    /// gives the defaults and a warning.
    pub fn load() -> (Self, Option<String>) {
        match settings_path() {
            Some(p) => Self::load_from(&p),
            None => (Self::default(), None),
        }
    }

    pub fn load_from(path: &Path) -> (Self, Option<String>) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return (Self::default(), None);
        };
        match toml::from_str::<Settings>(&text) {
            Ok(mut s) => {
                s.detect = s.detect.clamped();
                (s, None)
            }
            Err(e) => (Self::default(), Some(format!("settings ignored: {e}"))),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = settings_path().ok_or("no config directory (set HOME or XDG_CONFIG_HOME)")?;
        self.save_to(&path)
    }

    /// Writes atomically (temp file, then rename).
    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(tmp, path).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scan-settings-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("settings.toml")
    }

    #[test]
    fn defaults_match_the_detector_defaults() {
        assert_eq!(DetectSettings::default().options(), Options::default());
    }

    #[test]
    fn round_trips_through_a_file() {
        let path = temp("rt");
        let s = Settings {
            detect: DetectSettings {
                search_mm: 4.5,
                refine_mm: 0.5,
                min_confidence: 0.4,
                inset_mm: 1.2,
            },
            ..Settings::default()
        };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), (s, None));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn out_of_range_values_are_clamped_and_refine_follows_search() {
        let path = temp("clamp");
        std::fs::write(
            &path,
            "[detect]\nsearch_mm = 100.0\nrefine_mm = 50.0\nmin_confidence = 7.0\ninset_mm = -2.0\n",
        )
        .unwrap();
        let (s, warning) = Settings::load_from(&path);
        assert!(warning.is_none());
        assert_eq!(s.detect.search_mm, SEARCH_MM.1);
        assert_eq!(s.detect.refine_mm, REFINE_MM.1);
        assert_eq!((s.detect.min_confidence, s.detect.inset_mm), (1.0, 0.0));

        let tight = DetectSettings {
            search_mm: 1.0,
            refine_mm: 3.0,
            ..DetectSettings::default()
        }
        .clamped();
        assert!(tight.refine_mm <= tight.search_mm);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn the_save_destination_round_trips_and_defaults_to_a_folder() {
        assert_eq!(Settings::default().save.mode, SaveMode::Folder);
        assert!(Settings::default()
            .save
            .folder_or_default()
            .ends_with("Pictures/latent-scan"));

        let path = temp("save");
        let s = Settings {
            save: SaveSettings {
                mode: SaveMode::Archive,
                folder: Some(PathBuf::from("/data/scans")),
                archive: Some(PathBuf::from("/data/latent")),
                eight_bit: true,
            },
            ..Settings::default()
        };
        s.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("mode = \"archive\""), "{text}");
        assert_eq!(Settings::load_from(&path), (s.clone(), None));
        assert_eq!(s.save.folder_or_default(), PathBuf::from("/data/scans"));

        // Unset paths are left out of the file and load as unset.
        let bare = Settings::default();
        bare.save_to(&path).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("folder ="));
        assert_eq!(Settings::load_from(&path).0.save, SaveSettings::default());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn missing_values_fall_back_to_defaults() {
        let path = temp("partial");
        std::fs::write(&path, "[detect]\nsearch_mm = 2.0\n").unwrap();
        let (s, _) = Settings::load_from(&path);
        assert_eq!(s.detect.search_mm, 2.0);
        assert_eq!(
            s.detect.min_confidence,
            DetectSettings::default().min_confidence
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn a_broken_file_gives_defaults_and_a_warning_and_a_missing_one_none() {
        let path = temp("bad");
        std::fs::write(&path, "this is = not [valid").unwrap();
        let (s, warning) = Settings::load_from(&path);
        assert_eq!(s, Settings::default());
        assert!(warning.unwrap().contains("settings ignored"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
        assert_eq!(Settings::load_from(&path), (Settings::default(), None));
    }
}
