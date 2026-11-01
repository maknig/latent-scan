use clap::{Parser, Subcommand};
use scan_archive::Archive;
use scan_core::{CancelToken, Scanner, ScannerBackend, ScannerInfo};
use scan_pipeline::tone::{Adjust, ToneOptions, ToneStage};
use scan_pipeline::{BitDepth, Invert, OutputFormat, Pipeline, Rotate};
use scan_profiles::Profiles;
use scan_sane::SaneBackend;
use std::path::PathBuf;
use std::process::exit;

#[derive(Parser)]
#[command(
    name = "latent-scan",
    about = "Scan film directly into a latent archive"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once at startup
enum Cmd {
    /// List attached scanners.
    List,
    /// Show capabilities of a scanner (default: first found).
    Caps {
        #[arg(short, long)]
        device: Option<String>,
    },
    /// List scan profiles.
    Profiles,
    /// Scan with a profile and store the result.
    Scan {
        #[arg(short, long)]
        profile: String,
        #[arg(short, long)]
        device: Option<String>,
        /// Override the profile's resolution.
        #[arg(long)]
        dpi: Option<u32>,
        /// Rotate clockwise by 0, 90, 180 or 270 degrees (default: the profile's rotation).
        #[arg(long)]
        rotate: Option<u16>,
        /// Plain invert of every sample.
        #[arg(long)]
        invert: bool,
        /// Negative conversion: invert and remove the orange mask (default on for negative profiles).
        #[arg(long)]
        negative: bool,
        /// Stretch each channel to the full range.
        #[arg(long)]
        auto_levels: bool,
        /// Scan in gray (B&W) whatever the profile says.
        #[arg(long)]
        gray: bool,
        /// Fit each frame to the film on a 600 dpi pre-scan before scanning it.
        #[arg(long)]
        adjust: bool,
        /// Exposure change in stops.
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        exposure: f32,
        /// Black point in percent (0 to 40).
        #[arg(long, default_value_t = 0.0)]
        black: f32,
        /// White point in percent (60 to 100).
        #[arg(long, default_value_t = 100.0)]
        white: f32,
        /// Midtone gamma (above 1 brightens).
        #[arg(long, default_value_t = 1.0)]
        midtones: f32,
        /// Contrast around mid-grey, flatter (-1) to steeper (1).
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        contrast: f32,
        /// Shadows without moving black, darker (-1) to brighter (1).
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        darks: f32,
        /// Highlights without moving white, darker (-1) to brighter (1).
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        whites: f32,
        /// Colour balance, cool (-1) to warm (1).
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        warmth: f32,
        /// Tint, magenta (-1) to green (1).
        #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
        tint: f32,
        /// Output format (jpg = JPEG, always 8 bit).
        #[arg(long, value_parser = ["tiff", "png", "jpg"], default_value = "tiff")]
        format: String,
        /// Bits per sample of the saved file (the scan itself is always taken at the profile's depth).
        #[arg(long, value_parser = ["16", "8"], default_value = "16")]
        depth: String,
        /// File name prefix; frames are named `<prefix>-<number>`.
        #[arg(long, default_value = "scan")]
        name: String,
        /// Number of the first frame. Without it a single frame is stored as `<prefix>`.
        #[arg(long)]
        start: Option<u32>,
        /// Write to this TIFF path instead of an archive.
        #[arg(short, long, conflicts_with_all = ["archive", "table", "id"])]
        out: Option<PathBuf>,
        /// Latent archive folder.
        #[arg(long, requires_all = ["table", "id"])]
        archive: Option<PathBuf>,
        #[arg(long)]
        table: Option<String>,
        #[arg(long)]
        id: Option<u64>,
    },
}

fn open(backend: &SaneBackend, device: Option<&str>) -> Result<Box<dyn Scanner>, String> {
    let found = backend.discover().map_err(|e| e.to_string())?;
    let info: &ScannerInfo = match device {
        Some(d) => found.iter().find(|i| i.id == d),
        None => found.first(),
    }
    .ok_or("no scanner found")?;
    backend.open(info).map_err(|e| e.to_string())
}

fn run(cli: Cli) -> Result<(), String> {
    let backend = SaneBackend::default();
    let e = |e: &dyn std::fmt::Display| e.to_string();
    match cli.cmd {
        Cmd::List => {
            for i in backend.discover().map_err(|x| e(&x))? {
                println!("{}\t{} {}", i.id, i.vendor, i.model);
            }
        }
        Cmd::Caps { device } => {
            let s = open(&backend, device.as_deref())?;
            println!("{:#?}", s.capabilities().map_err(|x| e(&x))?);
        }
        Cmd::Profiles => {
            for p in Profiles::builtin().all() {
                println!("{:<16} {}", p.name, p.description);
            }
        }
        Cmd::Scan {
            profile,
            device,
            dpi,
            rotate,
            invert,
            negative,
            auto_levels,
            gray,
            exposure,
            black,
            white,
            midtones,
            warmth,
            tint,
            contrast,
            darks,
            whites,
            adjust,
            format,
            depth,
            name,
            start,
            out,
            archive,
            table,
            id,
        } => {
            let (profiles, warning) = Profiles::load();
            if let Some(w) = warning {
                eprintln!("warning: {w}");
            }
            let profile = profiles.get(&profile).map_err(|x| e(&x))?;
            let mut jobs = profile.jobs();
            for job in &mut jobs {
                job.dpi = dpi.unwrap_or(job.dpi);
                if gray {
                    job.color = scan_core::ColorMode::Gray;
                }
            }
            let tone = ToneOptions {
                negative: negative || profile.negative,
                auto_levels,
                adjust: Adjust {
                    exposure_ev: exposure.clamp(-2.0, 2.0),
                    black: (black / 100.0).clamp(0.0, 0.4),
                    white: (white / 100.0).clamp(0.6, 1.0),
                    gamma: midtones.clamp(0.5, 2.0),
                    warmth: warmth.clamp(-1.0, 1.0),
                    tint: tint.clamp(-1.0, 1.0),
                    contrast: contrast.clamp(-1.0, 1.0),
                    darks: darks.clamp(-1.0, 1.0),
                    whites: whites.clamp(-1.0, 1.0),
                },
            };
            let depth = if depth == "8" {
                BitDepth::Eight
            } else {
                BitDepth::Sixteen
            };
            let format = match format.as_str() {
                "png" => OutputFormat::Png,
                "jpg" => OutputFormat::Jpeg,
                _ => OutputFormat::Tiff,
            };
            if out.is_some() && jobs.len() > 1 {
                return Err("profile has several frames; use --archive, not --out".into());
            }
            let scanner = open(&backend, device.as_deref())?;
            let total = jobs.len();
            let caps = scanner.capabilities().map_err(|x| e(&x))?;
            for (i, job) in jobs.iter().enumerate() {
                let mut job = job.clone();
                if adjust {
                    let fit = scan_detect::adjust_before_scan(
                        scanner.as_ref(),
                        &job,
                        caps.get(job.source),
                        &scan_detect::Options::default(),
                        &CancelToken::default(),
                    )
                    .map_err(|x| e(&x))?;
                    eprintln!(
                        "frame {}/{total}: {} by {:+.2}, {:+.2} mm (confidence {:.2})",
                        i + 1,
                        if fit.moved { "adjusted" } else { "kept" },
                        fit.shift_mm.0,
                        fit.shift_mm.1,
                        fit.confidence
                    );
                    job.area = fit.area;
                }
                let progress = |p: f32| eprint!("\rframe {}/{total}: {:3.0}%", i + 1, p * 100.0);
                let raw = scanner
                    .scan(&job, &progress, &CancelToken::default())
                    .map_err(|x| e(&x))?;
                eprintln!();
                let mut pipe = Pipeline::default()
                    .with(Rotate(rotate.unwrap_or(profile.rotate)))
                    .with(ToneStage(tone));
                if invert {
                    pipe = pipe.with(Invert);
                }
                let img = pipe.run(raw).map_err(|x| e(&x))?;

                let stem = match (start, total) {
                    (None, 1) => name.clone(),
                    (s, _) => scan_archive::naming::frame_stem(&name, s.unwrap_or(1) + i as u32),
                };
                let (path, link) = match (&archive, &table, id) {
                    (Some(a), Some(t), Some(row)) => {
                        let a = Archive::new(a);
                        let target = a.target(t, row, &stem, format.ext()).map_err(|x| e(&x))?;
                        (target.abs.clone(), Some((a, t.clone(), row, target.rel)))
                    }
                    _ => (
                        out.clone()
                            .unwrap_or_else(|| PathBuf::from(format!("{stem}.{}", format.ext()))),
                        None,
                    ),
                };
                scan_pipeline::write_image_depth(&img, &path, depth).map_err(|x| e(&x))?;
                if let Some((a, t, row, rel)) = link {
                    a.add_image(&t, row, &rel).map_err(|x| e(&x))?;
                }
                println!("{}", path.display());
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(msg) = run(Cli::parse()) {
        eprintln!("error: {msg}");
        exit(1);
    }
}
