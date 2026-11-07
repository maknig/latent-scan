//! SANE backend. Drives the `scanimage` CLI; covers Canon `pixma` and Epson `epson2`/`epsonds`.
//! Can be swapped for libsane FFI later behind the same `ScannerBackend` trait.

mod command;
mod parse;

use latent_scan_core::{
    CancelToken, Capabilities, Progress, RawImage, Result, ScanError, ScanJob, Scanner,
    ScannerBackend, ScannerInfo, Source,
};
use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;

/// Options per device id, shared by all scanners of the process. Every `scanimage` call opens the
/// device, which makes an Epson home its carriage, so a scan must not read them again; the jobs
/// of the UI each open a new `SaneScanner`. Discovery (`capabilities`) refreshes the entry.
static CAPS: Mutex<Option<HashMap<String, Capabilities>>> = Mutex::new(None);

fn cached_caps(id: &str) -> Option<Capabilities> {
    let map = CAPS.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref()?.get(id).cloned()
}

pub struct SaneBackend {
    program: String,
}

impl Default for SaneBackend {
    fn default() -> Self {
        Self {
            program: "scanimage".into(),
        }
    }
}

impl SaneBackend {
    pub fn with_program(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl ScannerBackend for SaneBackend {
    fn discover(&self) -> Result<Vec<ScannerInfo>> {
        let out = Command::new(&self.program)
            .arg("--formatted-device-list=%d|%v|%m%n")
            .output()
            .map_err(|e| ScanError::Backend(format!("cannot run {}: {e}", self.program)))?;
        Ok(parse::device_list(&String::from_utf8_lossy(&out.stdout)))
    }

    fn open(&self, info: &ScannerInfo) -> Result<Box<dyn Scanner>> {
        Ok(Box::new(SaneScanner {
            program: self.program.clone(),
            info: info.clone(),
        }))
    }
}

pub struct SaneScanner {
    program: String,
    info: ScannerInfo,
}

impl SaneScanner {
    fn read_capabilities(&self) -> Result<Capabilities> {
        // `-A` prints the option list on stdout (plus a notice on stderr). The options differ
        // per source, so read them once for each source the device lists; the default source
        // is already in the first output.
        let options = |source: Option<Source>| -> Result<String> {
            let mut args = vec!["-A".to_string(), "-d".into(), self.info.id.clone()];
            if let Some(s) = source {
                args.extend(["--source".into(), command::source_name(s).into()]);
            }
            command::output(&self.program, &args)
        };
        let first = options(None)?;
        let default = parse::default_source(&first);
        let sources = parse::sources(&first)
            .into_iter()
            .map(|s| {
                let text = if default == Some(s) {
                    first.clone()
                } else {
                    options(Some(s))?
                };
                Ok(parse::source_caps(&text, s))
            })
            .collect::<Result<_>>()?;
        Ok(Capabilities { sources })
    }
}

impl Scanner for SaneScanner {
    fn info(&self) -> &ScannerInfo {
        &self.info
    }

    fn capabilities(&self) -> Result<Capabilities> {
        let caps = self.read_capabilities()?;
        let mut map = CAPS.lock().unwrap_or_else(|e| e.into_inner());
        map.get_or_insert_with(HashMap::new)
            .insert(self.info.id.clone(), caps.clone());
        Ok(caps)
    }

    fn scan(&self, job: &ScanJob, progress: Progress, cancel: &CancelToken) -> Result<RawImage> {
        let caps = match cached_caps(&self.info.id) {
            Some(c) => c,
            None => self.capabilities()?,
        };
        let args = command::scan_args(&self.info.id, job, &caps)?;
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("scan.tiff");
        command::run(&self.program, &args, &file, progress, cancel)?;
        command::read_tiff(&file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn scans_reuse_the_capabilities_of_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures");
        let log = dir.path().join("calls");
        let script = dir.path().join("scanimage");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$*\" in\n  *Transparency*) cat {f}/epson_v800_options_tpu.txt;;\n  *) cat {f}/epson_v800_options.txt;;\nesac\n",
                log = log.display(),
                f = fixtures
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let backend = SaneBackend::with_program(script.to_str().unwrap());
        let info = ScannerInfo {
            id: "epson2:cache-test".into(),
            vendor: "Epson".into(),
            model: "GT-X980".into(),
        };
        let first = backend.open(&info).unwrap().capabilities().unwrap();
        assert_eq!(first.sources.len(), 2);
        // A job opens its own scanner; it must not read the options again.
        let job = ScanJob {
            source: Source::Transparency,
            color: latent_scan_core::ColorMode::Color,
            bit_depth: 8,
            dpi: 150,
            preview: false,
            area: latent_scan_core::ScanArea {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
        };
        let _ = backend
            .open(&info)
            .unwrap()
            .scan(&job, &|_| {}, &CancelToken::default());
        let calls = std::fs::read_to_string(&log).unwrap();
        let reads = calls.lines().filter(|l| l.starts_with("-A")).count();
        // default (flatbed) + transparency unit, once
        assert_eq!(reads, 2, "{calls}");
        assert_eq!(calls.lines().count(), 3, "{calls}");
    }
}
