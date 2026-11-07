//! Building and running `scanimage` invocations.

use latent_scan_core::{
    CancelToken, Capabilities, ColorMode, Progress, RawImage, Result, ScanError, ScanJob, Source,
    SourceCaps,
};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// One `scanimage` at a time: a second open of the device while another runs fails with an I/O error.
static DEVICE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn device_lock() -> std::sync::MutexGuard<'static, ()> {
    DEVICE.lock().unwrap_or_else(|e| e.into_inner())
}

fn trace() -> bool {
    std::env::var_os("LATENT_SCAN_TRACE").is_some()
}

/// Runs `scanimage` and returns its stdout (used for `-A`).
pub(crate) fn output(program: &str, args: &[String]) -> Result<String> {
    let _guard = device_lock();
    if trace() {
        eprintln!("{program} {}", args.join(" "));
    }
    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| ScanError::Backend(format!("cannot run {program}: {e}")))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Picks the best mode the source offers: the 16-bit mode if listed, else the plain one
/// (the transparency unit has only `Color` / `Gray`).
fn pick_mode(modes: &[String], color: ColorMode, bit_depth: u8) -> Result<String> {
    let wanted: &[&str] = match (color, bit_depth) {
        (ColorMode::Color, 16) => &["48 bits color", "color"],
        (ColorMode::Gray, 16) => &["16 bits gray", "gray"],
        (ColorMode::Color, _) => &["color"],
        (ColorMode::Gray, _) => &["gray"],
    };
    wanted
        .iter()
        .find_map(|w| modes.iter().find(|m| m.eq_ignore_ascii_case(w)))
        .cloned()
        .ok_or_else(|| ScanError::Unsupported(format!("no {color:?} mode at {bit_depth} bit")))
}

pub(crate) fn source_name(source: Source) -> &'static str {
    match source {
        Source::Flatbed => "Flatbed",
        Source::Transparency => "Transparency Unit",
    }
}

/// Source must come first: changing it resets mode and resolution on pixma.
pub fn scan_args(device: &str, job: &ScanJob, caps: &Capabilities) -> Result<Vec<String>> {
    let src: &SourceCaps = caps
        .get(job.source)
        .ok_or_else(|| ScanError::Unsupported(format!("source {:?}", job.source)))?;
    if !src.resolutions.is_empty() && !src.resolutions.contains(&job.dpi) {
        return Err(ScanError::Unsupported(format!(
            "{} dpi for {:?} (available: {:?})",
            job.dpi, job.source, src.resolutions
        )));
    }
    let mode = pick_mode(&src.modes, job.color, job.bit_depth)?;
    let a = &job.area;
    let mut args: Vec<String> = ["-d", device, "--source", source_name(job.source), "--mode"]
        .map(String::from)
        .to_vec();
    args.push(mode);
    // Epson: the depth is its own option and inactive until the mode is set, so it follows `--mode`.
    if src.depths.contains(&job.bit_depth) {
        args.extend(["--depth".into(), job.bit_depth.to_string()]);
    }
    args.extend(
        [
            "--resolution",
            &job.dpi.to_string(),
            "-l",
            &a.x.to_string(),
            "-t",
            &a.y.to_string(),
            "-x",
            &a.width.to_string(),
            "-y",
            &a.height.to_string(),
            "--format=tiff",
            "-p",
        ]
        .map(String::from),
    );
    // Epson: a quick scan for the overview (fast carriage); other scanners have no such option.
    if job.preview && src.quick_preview {
        args.push("--preview=yes".into());
    }
    Ok(args)
}

/// Runs scanimage, streaming `Progress: n%` lines from stderr into `progress`.
/// On cancel the child gets SIGTERM so scanimage can release the scanner cleanly.
pub fn run(
    program: &str,
    args: &[String],
    out: &Path,
    progress: Progress,
    cancel: &CancelToken,
) -> Result<()> {
    let _guard = device_lock();
    if trace() {
        eprintln!("{program} {}", args.join(" "));
    }
    let file = std::fs::File::create(out)?;
    let mut child = Command::new(program)
        .args(args)
        .stdout(file)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ScanError::Backend(format!("cannot run {program}: {e}")))?;
    let stderr = child.stderr.take().expect("stderr is piped");
    let (status, log) = std::thread::scope(|s| {
        let reader = s.spawn(|| read_stderr(stderr, progress));
        let mut terminated_at = None;
        let status = loop {
            if let Some(st) = child.try_wait()? {
                break st;
            }
            if cancel.is_cancelled() {
                match terminated_at {
                    None => {
                        let _ = Command::new("kill").arg(child.id().to_string()).status();
                        terminated_at = Some(Instant::now());
                    }
                    // Fall back to SIGKILL if the driver does not stop in time.
                    Some(t) if t.elapsed() > Duration::from_secs(10) => child.kill()?,
                    Some(_) => {}
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        Ok::<_, std::io::Error>((status, reader.join().unwrap_or_default()))
    })?;
    if cancel.is_cancelled() {
        Err(ScanError::Cancelled)
    } else if status.success() {
        progress(1.0);
        Ok(())
    } else {
        Err(ScanError::Backend(failure_text(&log, args)))
    }
}

/// The scanner's own message, with a hint for the I/O errors an Epson gives when it is busy or
/// was opened twice; the command line is added under `LATENT_SCAN_TRACE`.
fn failure_text(log: &str, args: &[String]) -> String {
    let mut text = log.trim().to_string();
    if text.to_ascii_lowercase().contains("i/o") {
        text.push_str(" (scanner busy or not ready: wait until the carriage stops, or replug it)");
    }
    if trace() {
        text.push_str(&format!(" [scanimage {}]", args.join(" ")));
    }
    text
}

/// Forwards progress lines and returns the remaining stderr text.
fn read_stderr(mut stderr: impl Read, progress: Progress) -> String {
    let mut log = String::new();
    let (mut buf, mut line) = ([0u8; 256], String::new());
    while let Ok(n) = stderr.read(&mut buf) {
        if n == 0 {
            break;
        }
        for &b in &buf[..n] {
            if b == b'\r' || b == b'\n' {
                if let Some(p) = line.strip_prefix("Progress: ") {
                    if let Ok(v) = p.trim_end_matches('%').trim().parse::<f32>() {
                        progress(v / 100.0);
                    }
                } else if !line.is_empty() {
                    log.push_str(&line);
                    log.push('\n');
                }
                line.clear();
            } else {
                line.push(b as char);
            }
        }
    }
    log
}

pub fn read_tiff(path: &Path) -> Result<RawImage> {
    let data = |e: &dyn std::fmt::Display| ScanError::Data(e.to_string());
    let mut reader = image::ImageReader::open(path)?
        .with_guessed_format()
        .map_err(|e| data(&e))?;
    // High-resolution 16-bit scans exceed the decoder's default allocation limit.
    reader.no_limits();
    from_dynamic(reader.decode().map_err(|e| data(&e))?)
}

/// Converts without copying when the image is already 16-bit; 8-bit samples are widened to the
/// full 16-bit range (`v * 257`).
fn from_dynamic(img: image::DynamicImage) -> Result<RawImage> {
    use image::DynamicImage::*;
    let (w, h) = (img.width(), img.height());
    let widen = |v: Vec<u8>| v.into_iter().map(|s| s as u16 * 257).collect();
    let image = match img {
        ImageLuma16(b) => RawImage::new(w, h, 1, b.into_raw()),
        ImageRgb16(b) => RawImage::new(w, h, 3, b.into_raw()),
        ImageLuma8(b) => RawImage::new(w, h, 1, widen(b.into_raw())),
        ImageRgb8(b) => RawImage::new(w, h, 3, widen(b.into_raw())),
        other => RawImage::new(w, h, 3, other.into_rgb16().into_raw()),
    };
    Ok(image?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    use latent_scan_core::ScanArea;

    fn caps() -> Capabilities {
        let flatbed = include_str!("../../../tests/fixtures/canon9000f_options.txt");
        let tpu = include_str!("../../../tests/fixtures/canon9000f_options_tpu.txt");
        Capabilities {
            sources: vec![
                parse::source_caps(flatbed, Source::Flatbed),
                parse::source_caps(tpu, Source::Transparency),
            ],
        }
    }

    fn job(source: Source, color: ColorMode, bit_depth: u8, dpi: u32) -> ScanJob {
        ScanJob {
            source,
            color,
            bit_depth,
            dpi,
            preview: false,
            area: ScanArea {
                x: 1.0,
                y: 2.0,
                width: 36.0,
                height: 24.0,
            },
        }
    }

    fn arg_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let i = args.iter().position(|a| a == flag).unwrap();
        &args[i + 1]
    }

    #[test]
    fn source_comes_before_mode_and_resolution() {
        let j = job(Source::Flatbed, ColorMode::Color, 16, 600);
        let a = scan_args("pixma:1", &j, &caps()).unwrap();
        let pos = |s: &str| a.iter().position(|x| x == s).unwrap();
        assert!(pos("--source") < pos("--mode") && pos("--mode") < pos("--resolution"));
        assert_eq!(arg_after(&a, "--mode"), "48 bits color");
        assert_eq!(arg_after(&a, "--source"), "Flatbed");
    }

    #[test]
    fn transparency_unit_falls_back_to_plain_modes() {
        let c = caps();
        let color = scan_args(
            "d",
            &job(Source::Transparency, ColorMode::Color, 16, 2400),
            &c,
        );
        assert_eq!(arg_after(&color.unwrap(), "--mode"), "Color");
        let gray = scan_args(
            "d",
            &job(Source::Transparency, ColorMode::Gray, 16, 4800),
            &c,
        );
        assert_eq!(arg_after(&gray.unwrap(), "--mode"), "Gray");
        let flat_gray = scan_args("d", &job(Source::Flatbed, ColorMode::Gray, 16, 600), &c);
        assert_eq!(arg_after(&flat_gray.unwrap(), "--mode"), "16 bits gray");
    }

    #[test]
    fn quick_preview_is_only_sent_where_the_device_has_it() {
        let epson = Capabilities {
            sources: vec![parse::source_caps(
                include_str!("../../../tests/fixtures/epson_v800_options_tpu.txt"),
                Source::Transparency,
            )],
        };
        let mut j = job(Source::Transparency, ColorMode::Color, 8, 150);
        let plain = scan_args("d", &j, &epson).unwrap();
        assert!(!plain.iter().any(|a| a.starts_with("--preview")));
        j.preview = true;
        assert!(scan_args("d", &j, &epson)
            .unwrap()
            .contains(&"--preview=yes".to_string()));
        let mut f = job(Source::Flatbed, ColorMode::Color, 16, 600);
        f.preview = true;
        let canon = scan_args("d", &f, &caps()).unwrap();
        assert!(!canon.iter().any(|a| a.starts_with("--preview")));
    }

    #[test]
    fn epson_depth_follows_mode() {
        let c = Capabilities {
            sources: vec![parse::source_caps(
                include_str!("../../../tests/fixtures/epson_v800_options_tpu.txt"),
                Source::Transparency,
            )],
        };
        let a = scan_args(
            "d",
            &job(Source::Transparency, ColorMode::Color, 16, 3200),
            &c,
        )
        .unwrap();
        assert_eq!(arg_after(&a, "--mode"), "Color");
        assert_eq!(arg_after(&a, "--depth"), "16");
        let pos = |s: &str| a.iter().position(|x| x == s).unwrap();
        assert!(pos("--mode") < pos("--depth") && pos("--depth") < pos("--resolution"));
        let canon = scan_args(
            "d",
            &job(Source::Flatbed, ColorMode::Color, 16, 600),
            &caps(),
        )
        .unwrap();
        assert!(!canon.contains(&"--depth".to_string()));
    }

    #[test]
    fn dpi_is_checked_against_the_selected_source() {
        let c = caps();
        let flat = job(Source::Flatbed, ColorMode::Color, 16, 150);
        assert!(scan_args("d", &flat, &c).is_ok());
        let tpu = job(Source::Transparency, ColorMode::Color, 16, 150);
        assert!(scan_args("d", &tpu, &c).is_err());
        let odd = job(Source::Flatbed, ColorMode::Color, 16, 1000);
        assert!(scan_args("d", &odd, &c).is_err());
    }

    #[test]
    fn eight_bit_samples_are_widened_to_full_range() {
        let g = image::DynamicImage::ImageLuma8(
            image::GrayImage::from_raw(2, 1, vec![0, 255]).unwrap(),
        );
        assert_eq!(from_dynamic(g).unwrap().data, vec![0, 65535]);
        let rgb = image::DynamicImage::ImageRgb16(
            image::ImageBuffer::from_raw(1, 1, vec![1u16, 2, 3]).unwrap(),
        );
        let r = from_dynamic(rgb).unwrap();
        assert_eq!((r.channels, r.data), (3, vec![1, 2, 3]));
    }
}
