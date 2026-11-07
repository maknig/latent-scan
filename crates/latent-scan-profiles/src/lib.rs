//! Named scan presets (source, colour, depth, dpi, area) with user overrides from TOML.

use latent_scan_core::{ColorMode, ScanArea, ScanJob, ScannerInfo, Source};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const BUILTIN: &str = include_str!("../profiles.toml");

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("invalid profile file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("cannot write profiles: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("unknown profile `{0}`")]
    Unknown(String),
    #[error("no config directory (set HOME or XDG_CONFIG_HOME)")]
    NoConfigDir,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The scanner this profile was made for: part of its model name (`9000F`, `X980`), compared
    /// case-insensitively. None fits every scanner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanner: Option<String>,
    pub source: Source,
    pub color: ColorMode,
    pub bit_depth: u8,
    pub dpi: u32,
    /// Film is a negative: the UI turns on negative conversion for it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub negative: bool,
    /// Clockwise rotation of the saved scans and the preview (0, 90, 180, 270): how the holder
    /// is inserted.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rotate: u16,
    /// Bounding/first frame; kept so profiles without `frames` still load.
    pub area: ScanArea,
    /// One area per film frame in scan order (mm on the bed). Empty means just `area`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frames: Vec<ScanArea>,
}

fn is_zero(v: &u16) -> bool {
    *v == 0
}

impl Profile {
    /// Whether the profile belongs to `scanner` (holder areas differ between scanners).
    pub fn fits(&self, scanner: &ScannerInfo) -> bool {
        let Some(tag) = self.scanner.as_deref().map(str::to_lowercase) else {
            return true;
        };
        let model = scanner.model.to_lowercase();
        model.contains(&tag) || (!model.is_empty() && tag.contains(&model))
    }

    /// The areas to scan, in order: `frames`, or `area` when there are none.
    pub fn frame_areas(&self) -> Vec<ScanArea> {
        if self.frames.is_empty() {
            vec![self.area]
        } else {
            self.frames.clone()
        }
    }

    /// One job per frame, in scan order.
    pub fn jobs(&self) -> Vec<ScanJob> {
        self.frame_areas()
            .into_iter()
            .map(|area| ScanJob { area, ..self.job() })
            .collect()
    }

    pub fn job(&self) -> ScanJob {
        ScanJob {
            source: self.source,
            color: self.color,
            bit_depth: self.bit_depth,
            dpi: self.dpi,
            preview: false,
            area: self.area,
        }
    }
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    profile: Vec<Profile>,
}

#[derive(Serialize)]
struct Out<'a> {
    profile: &'a [Profile],
}

pub struct Profiles(Vec<Profile>);

impl Profiles {
    pub fn builtin() -> Self {
        Self::parse(BUILTIN).expect("built-in profiles are valid")
    }

    pub fn parse(text: &str) -> Result<Self, ProfileError> {
        Ok(Self(toml::from_str::<File>(text)?.profile))
    }

    /// Built-in profiles overlaid with the user's file. A broken user file is reported in the
    /// second value and otherwise ignored.
    pub fn load() -> (Self, Option<String>) {
        let mut profiles = Self::builtin();
        let warning = user_profile_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| profiles.merge(&text).err())
            .map(|e| format!("user profiles ignored: {e}"));
        (profiles, warning)
    }

    /// Adds profiles from `text`; same-named entries are replaced in place.
    pub fn merge(&mut self, text: &str) -> Result<(), ProfileError> {
        for p in Self::parse(text)?.0 {
            self.upsert(p);
        }
        Ok(())
    }

    /// Stores `profile` (replacing a same-named one) in the user file and in `self`.
    pub fn save_user(&mut self, profile: Profile) -> Result<(), ProfileError> {
        let path = user_profile_path().ok_or(ProfileError::NoConfigDir)?;
        save_to(&path, &profile)?;
        self.upsert(profile);
        Ok(())
    }

    fn upsert(&mut self, p: Profile) {
        match self.0.iter_mut().find(|e| e.name == p.name) {
            Some(slot) => *slot = p,
            None => self.0.push(p),
        }
    }

    pub fn all(&self) -> &[Profile] {
        &self.0
    }

    /// The profiles that fit `scanner`, with their index in [`Self::all`].
    pub fn for_scanner<'a>(
        &'a self,
        scanner: &'a ScannerInfo,
    ) -> impl Iterator<Item = (usize, &'a Profile)> {
        self.0
            .iter()
            .enumerate()
            .filter(move |(_, p)| p.fits(scanner))
    }

    pub fn get(&self, name: &str) -> Result<&Profile, ProfileError> {
        self.0
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| ProfileError::Unknown(name.into()))
    }
}

/// `$XDG_CONFIG_HOME/latent-scan`, else `~/.config/latent-scan`: where user profiles and
/// settings live.
pub fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("latent-scan"))
}

/// `<config dir>/profiles.toml`.
pub fn user_profile_path() -> Option<PathBuf> {
    Some(config_dir()?.join("profiles.toml"))
}

/// Adds or replaces `profile` in the TOML file at `path`, creating it if needed.
fn save_to(path: &Path, profile: &Profile) -> Result<(), ProfileError> {
    let mut file = Profiles(vec![]);
    if let Ok(text) = std::fs::read_to_string(path) {
        file.merge(&text)?;
    }
    file.upsert(profile.clone());
    let text = toml::to_string_pretty(&Out { profile: &file.0 })?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, x: f64) -> Profile {
        Profile {
            name: name.into(),
            description: String::new(),
            scanner: None,
            source: Source::Transparency,
            color: ColorMode::Color,
            bit_depth: 16,
            dpi: 2400,
            area: ScanArea {
                x,
                y: 20.0,
                width: 28.0,
                height: 160.0,
            },
            frames: vec![],
            negative: false,
            rotate: 0,
        }
    }

    fn info(model: &str) -> ScannerInfo {
        ScannerInfo {
            id: "d".into(),
            vendor: "V".into(),
            model: model.into(),
        }
    }

    #[test]
    fn profiles_follow_the_scanner() {
        let mut any = profile("any", 1.0);
        let mut canon = profile("canon", 2.0);
        canon.scanner = Some("9000f".into());
        let mut epson = profile("epson", 3.0);
        epson.scanner = Some("X980".into());
        any.scanner = None;
        let all = Profiles(vec![any, canon, epson]);
        let names = |m: &str| -> Vec<(usize, String)> {
            all.for_scanner(&info(m))
                .map(|(i, p)| (i, p.name.clone()))
                .collect()
        };
        let n = |v: &[(usize, &str)]| -> Vec<(usize, String)> {
            v.iter().map(|(i, s)| (*i, s.to_string())).collect()
        };
        assert_eq!(
            names("Canoscan 9000F multi-function peripheral"),
            n(&[(0, "any"), (1, "canon")])
        );
        assert_eq!(names("GT-X980"), n(&[(0, "any"), (2, "epson")]));
        assert_eq!(names("Other"), n(&[(0, "any")]));
    }

    #[test]
    fn scanner_tag_round_trips_and_is_optional() {
        let text = "[[profile]]\nname = \"a\"\nsource = \"flatbed\"\ncolor = \"color\"\nbit_depth = 16\ndpi = 600\narea = { x = 0.0, y = 0.0, width = 1.0, height = 1.0 }\n";
        let p = Profiles::parse(text).unwrap();
        assert_eq!(p.all()[0].scanner, None);
        let tagged = Profiles::parse(&format!("{text}scanner = \"X980\"\n")).unwrap();
        assert_eq!(tagged.all()[0].scanner.as_deref(), Some("X980"));
    }

    #[test]
    fn builtin_loads_and_resolves() {
        let p = Profiles::builtin();
        assert_eq!(
            p.get("35mm-strip").unwrap().job().source,
            Source::Transparency
        );
        assert!(p.get("nope").is_err());
    }

    #[test]
    fn merge_replaces_by_name_in_place() {
        let mut p = Profiles::builtin();
        let n = p.all().len();
        p.merge(
            "[[profile]]\nname='print-flatbed'\nsource='flatbed'\ncolor='gray'\nbit_depth=8\n\
             dpi=300\narea={x=0.0,y=0.0,width=100.0,height=100.0}",
        )
        .unwrap();
        assert_eq!(p.all().len(), n);
        assert_eq!(p.all()[0].name, "print-flatbed");
        assert_eq!(p.get("print-flatbed").unwrap().dpi, 300);
    }

    #[test]
    fn frames_round_trip_and_default_to_the_area() {
        let mut p = profile("strip", 10.0);
        assert_eq!(p.frame_areas(), vec![p.area]);
        assert_eq!(p.jobs().len(), 1);

        let frame = |x: f64| ScanArea { x, ..p.area };
        p.frames = vec![frame(10.0), frame(45.0), frame(80.0)];
        let dir = std::env::temp_dir().join(format!("scan-frames-{}", std::process::id()));
        let path = dir.join("profiles.toml");
        save_to(&path, &p).unwrap();
        let back = Profiles::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let back = back.get("strip").unwrap();
        assert_eq!(back, &p);
        let xs: Vec<f64> = back.jobs().iter().map(|j| j.area.x).collect();
        assert_eq!(xs, vec![10.0, 45.0, 80.0]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rotation_round_trips_and_legacy_profiles_default_to_zero() {
        let mut p = profile("r", 1.0);
        p.rotate = 90;
        let dir = std::env::temp_dir().join(format!("scan-rot-{}", std::process::id()));
        let path = dir.join("profiles.toml");
        save_to(&path, &p).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("rotate = 90"));
        assert_eq!(Profiles::parse(&text).unwrap().get("r").unwrap().rotate, 90);
        std::fs::remove_dir_all(dir).ok();

        let legacy = "[[profile]]\nname='old'\nsource='flatbed'\ncolor='color'\nbit_depth=8\ndpi=300\n[profile.area]\nx=0.0\ny=0.0\nwidth=1.0\nheight=1.0\n";
        assert_eq!(
            Profiles::parse(legacy).unwrap().get("old").unwrap().rotate,
            0
        );
        assert!(!toml::to_string(&Out {
            profile: &[profile("z", 0.0)]
        })
        .unwrap()
        .contains("rotate"));
    }

    #[test]
    fn saved_profiles_round_trip_and_replace() {
        let dir = std::env::temp_dir().join(format!("latent-scan-profiles-{}", std::process::id()));
        let path = dir.join("nested/profiles.toml");
        save_to(&path, &profile("a", 1.0)).unwrap();
        save_to(&path, &profile("b", 2.0)).unwrap();
        save_to(&path, &profile("a", 3.0)).unwrap();

        let loaded = Profiles::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.all().len(), 2);
        assert_eq!(loaded.get("a").unwrap(), &profile("a", 3.0));
        assert_eq!(loaded.get("b").unwrap(), &profile("b", 2.0));
        std::fs::remove_dir_all(dir).ok();
    }
}
