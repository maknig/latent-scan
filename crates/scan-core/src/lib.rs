//! Backend-agnostic scanner types and traits. No I/O lives here.

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("no scanner found")]
    NoDevice,
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("scan cancelled")]
    Cancelled,
    #[error("scanner backend failed: {0}")]
    Backend(String),
    #[error("invalid scan data: {0}")]
    Data(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, ScanError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Reflective originals, e.g. prints and enlargements.
    Flatbed,
    /// Film via the transparency unit (lid light).
    Transparency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    Color,
    Gray,
}

/// Scan window in millimetres, origin at the scanner's top-left.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScanArea {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanJob {
    pub source: Source,
    pub color: ColorMode,
    /// 8 or 16 bits per channel.
    pub bit_depth: u8,
    pub dpi: u32,
    pub area: ScanArea,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScannerInfo {
    /// Backend-specific device id, e.g. `pixma:04A91908`.
    pub id: String,
    pub vendor: String,
    pub model: String,
}

/// Options a scanner offers for one source; they differ per source (e.g. the transparency unit
/// has no 16-bit modes and no resolutions below 300 dpi on the 9000F).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceCaps {
    pub source: Source,
    pub resolutions: Vec<u32>,
    /// Backend mode names, e.g. `48 bits color`.
    pub modes: Vec<String>,
    /// Largest scan window the backend accepts for this source.
    pub max_area: Option<ScanArea>,
}

impl SourceCaps {
    /// The smallest offered resolution of at least `dpi`, else the largest one.
    pub fn resolution_at_least(&self, dpi: u32) -> Option<u32> {
        let mut r = self.resolutions.clone();
        r.sort_unstable();
        r.iter().copied().find(|&d| d >= dpi).or(r.last().copied())
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub sources: Vec<SourceCaps>,
}

impl Capabilities {
    pub fn get(&self, source: Source) -> Option<&SourceCaps> {
        self.sources.iter().find(|s| s.source == source)
    }
}

/// Interleaved 16-bit samples; 8-bit data is widened to the full 16-bit range.
#[derive(Debug, Clone, PartialEq)]
pub struct RawImage {
    pub width: u32,
    pub height: u32,
    /// 1 (gray) or 3 (RGB).
    pub channels: u8,
    pub data: Vec<u16>,
}

impl RawImage {
    pub fn new(width: u32, height: u32, channels: u8, data: Vec<u16>) -> Result<Self> {
        if data.len() != width as usize * height as usize * channels as usize {
            return Err(ScanError::Data(
                "sample count does not match dimensions".into(),
            ));
        }
        Ok(Self {
            width,
            height,
            channels,
            data,
        })
    }
}

/// Shared cancel flag; clones observe the same state.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Progress callback, called with a fraction in `0.0..=1.0`.
pub type Progress<'a> = &'a (dyn Fn(f32) + Send + Sync);

pub trait Scanner {
    fn info(&self) -> &ScannerInfo;
    fn capabilities(&self) -> Result<Capabilities>;
    /// Blocks until done; returns `ScanError::Cancelled` if `cancel` is triggered meanwhile.
    fn scan(&self, job: &ScanJob, progress: Progress, cancel: &CancelToken) -> Result<RawImage>;
}

pub trait ScannerBackend {
    fn discover(&self) -> Result<Vec<ScannerInfo>>;
    fn open(&self, info: &ScannerInfo) -> Result<Box<dyn Scanner>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_image_checks_length() {
        assert!(RawImage::new(2, 2, 3, vec![0; 12]).is_ok());
        assert!(RawImage::new(2, 2, 3, vec![0; 11]).is_err());
    }
}
