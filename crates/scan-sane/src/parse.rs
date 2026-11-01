//! Parsers for `scanimage --formatted-device-list` / `-A` output.

use scan_core::{ScanArea, ScannerInfo, Source, SourceCaps};

/// Parses `--formatted-device-list=%d|%v|%m%n` lines.
pub fn device_list(text: &str) -> Vec<ScannerInfo> {
    text.lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, '|');
            let (id, vendor, model) = (p.next()?, p.next()?, p.next()?);
            (!id.is_empty()).then(|| ScannerInfo {
                id: id.into(),
                vendor: vendor.into(),
                model: model.into(),
            })
        })
        .collect()
}

/// Returns the `|`-separated or range value list of an option line named by its full flag
/// (`--mode`, `-x`), e.g. for `--resolution auto||75|150|300dpi [75]` the part
/// `auto||75|150|300dpi`.
fn option_values<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name} ");
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with(&prefix))?;
    let rest = &line[prefix.len()..];
    Some(rest.rsplit_once(" [").map_or(rest, |(v, _)| v))
}

fn range_max(text: &str, name: &str) -> Option<f64> {
    let v = option_values(text, name)?;
    let end = v.split("..").nth(1)?;
    let num: String = end
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    num.parse().ok()
}

pub fn modes(text: &str) -> Vec<String> {
    option_values(text, "--mode")
        .map(|v| {
            v.split('|')
                .filter(|m| !m.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Sources listed by the `--source` option.
pub fn sources(text: &str) -> Vec<Source> {
    option_values(text, "--source")
        .map(|v| {
            v.split('|')
                .filter_map(|s| match s.to_ascii_lowercase().as_str() {
                    "flatbed" => Some(Source::Flatbed),
                    s if s.contains("transparency") || s.contains("tpu") => {
                        Some(Source::Transparency)
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Options of `text`, which must be `-A` output taken with `source` selected.
pub fn source_caps(text: &str, source: Source) -> SourceCaps {
    let resolutions = option_values(text, "--resolution")
        .map(|v| {
            v.trim_end_matches("dpi")
                .split('|')
                .filter_map(|s| s.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default();
    let max_area = match (range_max(text, "-x"), range_max(text, "-y")) {
        (Some(width), Some(height)) => Some(ScanArea {
            x: 0.0,
            y: 0.0,
            width,
            height,
        }),
        _ => None,
    };
    SourceCaps {
        source,
        resolutions,
        modes: modes(text),
        max_area,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLATBED: &str = include_str!("../../../tests/fixtures/canon9000f_options.txt");
    const TPU: &str = include_str!("../../../tests/fixtures/canon9000f_options_tpu.txt");

    #[test]
    fn parses_device_list() {
        let d = device_list("pixma:04A91908|CANON|Canoscan 9000F multi-function peripheral\n");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].id, "pixma:04A91908");
        assert_eq!(d[0].vendor, "CANON");
    }

    #[test]
    fn lists_sources() {
        assert_eq!(
            sources(FLATBED),
            vec![Source::Flatbed, Source::Transparency]
        );
    }

    #[test]
    fn parses_flatbed_caps() {
        let c = source_caps(FLATBED, Source::Flatbed);
        assert_eq!(c.resolutions, vec![75, 150, 300, 600, 1200, 2400, 4800]);
        assert!(c.modes.contains(&"48 bits color".to_string()));
        let a = c.max_area.unwrap();
        assert!((a.width - 216.069).abs() < 1e-6 && (a.height - 297.011).abs() < 1e-6);
    }

    #[test]
    fn transparency_unit_differs_from_flatbed() {
        let c = source_caps(TPU, Source::Transparency);
        assert_eq!(c.resolutions, vec![300, 600, 1200, 2400, 4800, 9600]);
        assert_eq!(c.modes, vec!["auto", "Color", "Gray", "Infrared"]);
        assert!(c.max_area.is_some());
    }

    #[test]
    fn resolution_at_least_rounds_up_then_clamps() {
        let c = source_caps(TPU, Source::Transparency);
        assert_eq!(c.resolution_at_least(150), Some(300));
        assert_eq!(c.resolution_at_least(4800), Some(4800));
        assert_eq!(c.resolution_at_least(20000), Some(9600));
    }
}
