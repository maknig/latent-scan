//! SANE backend. Drives the `scanimage` CLI; covers Canon `pixma` and Epson `epson2`/`epsonds`.
//! Can be swapped for libsane FFI later behind the same `ScannerBackend` trait.

mod command;
mod parse;

use scan_core::{
    CancelToken, Capabilities, Progress, RawImage, Result, ScanError, ScanJob, Scanner,
    ScannerBackend, ScannerInfo, Source,
};
use std::process::Command;

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

impl Scanner for SaneScanner {
    fn info(&self) -> &ScannerInfo {
        &self.info
    }

    fn capabilities(&self) -> Result<Capabilities> {
        // `-A` prints the option list on stdout (plus a notice on stderr). The options differ
        // per source, so read them once for each source the device lists.
        let options = |source: Option<Source>| -> Result<String> {
            let mut cmd = Command::new(&self.program);
            cmd.args(["-A", "-d", &self.info.id]);
            if let Some(s) = source {
                cmd.args(["--source", command::source_name(s)]);
            }
            Ok(String::from_utf8_lossy(&cmd.output()?.stdout).into_owned())
        };
        let sources = parse::sources(&options(None)?)
            .into_iter()
            .map(|s| Ok(parse::source_caps(&options(Some(s))?, s)))
            .collect::<Result<_>>()?;
        Ok(Capabilities { sources })
    }

    fn scan(&self, job: &ScanJob, progress: Progress, cancel: &CancelToken) -> Result<RawImage> {
        let caps = self.capabilities()?;
        let args = command::scan_args(&self.info.id, job, &caps)?;
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("scan.tiff");
        command::run(&self.program, &args, &file, progress, cancel)?;
        command::read_tiff(&file)
    }
}
