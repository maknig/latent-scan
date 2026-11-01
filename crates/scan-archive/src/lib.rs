//! Latent archive conventions: where scans are stored and how rows reference them.
//!
//! Mirrors `latent_data::fs::import_image` and the `latent.data.json` layout so the two stay
//! compatible; on merge this crate can be replaced by a direct `latent-data` dependency.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub mod naming;

pub const DATA_FILE: &str = "latent.data.json";

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid {DATA_FILE}: {0}")]
    Json(#[from] serde_json::Error),
    #[error("table `{0}` not found in archive")]
    NoTable(String),
    #[error("row {1} not found in table `{0}`")]
    NoRow(String, u64),
}

pub struct Archive {
    root: PathBuf,
}

/// Where a scan will be written. `rel` uses forward slashes, as stored in the data file.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub rel: String,
    pub abs: PathBuf,
}

impl Archive {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `images/<table>/<id:04>/<name>.<ext>`; clashes get a `-n` suffix like latent's importer.
    pub fn target(
        &self,
        table: &str,
        row_id: u64,
        stem: &str,
        ext: &str,
    ) -> std::io::Result<Target> {
        let rel_dir = format!("images/{table}/{row_id:04}");
        let dir = self.root.join(&rel_dir);
        let abs = unique_path(&dir, stem, ext)?;
        let fname = abs.file_name().map(|f| f.to_string_lossy().into_owned());
        Ok(Target {
            rel: format!("{rel_dir}/{}", fname.unwrap_or_default()),
            abs,
        })
    }

    /// Appends `rel` to the row's `images` list (a JSON array on disk, as latent writes it;
    /// a path already in the list is not added twice). A string value, which earlier versions
    /// wrote and latent rejects, is converted to a list. Writes atomically and keeps the
    /// previous file as `.bak`.
    pub fn add_image(&self, table: &str, row_id: u64, rel: &str) -> Result<(), ArchiveError> {
        let path = self.root.join(DATA_FILE);
        let mut doc: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        let rows = doc["tables"][table]["rows"]
            .as_array_mut()
            .ok_or_else(|| ArchiveError::NoTable(table.into()))?;
        let row = rows
            .iter_mut()
            .find(|r| ["id", "_id"].iter().any(|k| r[*k].as_u64() == Some(row_id)))
            .ok_or_else(|| ArchiveError::NoRow(table.into(), row_id))?;
        let mut images = image_list(&row["images"]);
        if !images.iter().any(|i| i == rel) {
            images.push(rel.to_string());
        }
        row["images"] = json!(images);

        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&doc)?)?;
        std::fs::copy(&path, path.with_extension("json.bak"))?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

/// A file path `<dir>/<stem>.<ext>` that does not exist yet (clashes get a `-n` suffix, like
/// latent's importer); creates `dir` when needed.
fn unique_path(dir: &Path, stem: &str, ext: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut n = 0;
    loop {
        let name = if n == 0 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem}-{n}.{ext}")
        };
        let abs = dir.join(name);
        if !abs.exists() {
            return Ok(abs);
        }
        n += 1;
    }
}

/// Target for a scan saved into a plain folder (no archive): `<dir>/<stem>.<ext>` with the same
/// `-n` clash rule as [`Archive::target`].
pub fn folder_target(dir: &Path, stem: &str, ext: &str) -> std::io::Result<PathBuf> {
    unique_path(dir, stem, ext)
}

/// Whether `dir` looks like the root of a latent archive.
pub fn is_archive(dir: &Path) -> bool {
    dir.join(DATA_FILE).is_file()
}

/// The paths of an `images` cell: a JSON array of strings, or a newline-separated string.
pub fn image_list(cell: &Value) -> Vec<String> {
    let clean = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    match cell {
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .filter_map(clean)
            .collect(),
        Value::String(s) => s.lines().filter_map(clean).collect(),
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_suffixes_clashes() {
        let d = tempfile::tempdir().unwrap();
        let a = Archive::new(d.path());
        let t = a.target("roll_film", 7, "scan", "tiff").unwrap();
        assert_eq!(t.rel, "images/roll_film/0007/scan.tiff");
        std::fs::write(&t.abs, b"x").unwrap();
        assert_eq!(
            a.target("roll_film", 7, "scan", "tiff").unwrap().rel,
            "images/roll_film/0007/scan-1.tiff"
        );
    }

    fn archive_with(rows: &str) -> (tempfile::TempDir, Archive) {
        let d = tempfile::tempdir().unwrap();
        let data = format!(r#"{{"tables":{{"roll_film":{{"next_id":9,"rows":{rows}}}}}}}"#);
        std::fs::write(d.path().join(DATA_FILE), data).unwrap();
        let a = Archive::new(d.path());
        (d, a)
    }

    fn images_of(d: &tempfile::TempDir, row: usize) -> Value {
        let text = std::fs::read_to_string(d.path().join(DATA_FILE)).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        v["tables"]["roll_film"]["rows"][row]["images"].clone()
    }

    #[test]
    fn folder_targets_use_the_same_clash_rule() {
        let d = tempfile::tempdir().unwrap();
        let sub = d.path().join("scans/new");
        let a = folder_target(&sub, "frame-01", "png").unwrap();
        assert_eq!(a, sub.join("frame-01.png"), "the folder is created");
        std::fs::write(&a, b"x").unwrap();
        assert_eq!(
            folder_target(&sub, "frame-01", "png").unwrap(),
            sub.join("frame-01-1.png")
        );
        assert_eq!(
            folder_target(&sub, "frame-01", "tiff").unwrap(),
            sub.join("frame-01.tiff")
        );
    }

    #[test]
    fn only_a_folder_with_a_data_file_is_an_archive() {
        let d = tempfile::tempdir().unwrap();
        assert!(!is_archive(d.path()));
        std::fs::write(d.path().join(DATA_FILE), "{}").unwrap();
        assert!(is_archive(d.path()));
    }

    #[test]
    fn images_are_written_as_a_json_list_in_order() {
        let (d, a) = archive_with(r#"[{"_id":1},{"id":2,"images":["a.jpg"]}]"#);
        a.add_image("roll_film", 1, "c.tiff").unwrap();
        a.add_image("roll_film", 2, "b.tiff").unwrap();
        a.add_image("roll_film", 2, "d.png").unwrap();
        assert_eq!(images_of(&d, 0), json!(["c.tiff"]));
        assert_eq!(images_of(&d, 1), json!(["a.jpg", "b.tiff", "d.png"]));
        assert!(d.path().join("latent.data.json.bak").exists());
    }

    #[test]
    fn adding_the_same_path_twice_keeps_one_entry() {
        let (d, a) = archive_with(r#"[{"_id":1}]"#);
        a.add_image("roll_film", 1, "x.tiff").unwrap();
        a.add_image("roll_film", 1, "x.tiff").unwrap();
        assert_eq!(images_of(&d, 0), json!(["x.tiff"]));
    }

    #[test]
    fn a_string_value_written_by_older_versions_becomes_a_list() {
        let (d, a) = archive_with(r#"[{"_id":1,"images":"a.jpg\nb.jpg\n"}]"#);
        a.add_image("roll_film", 1, "c.tiff").unwrap();
        assert_eq!(images_of(&d, 0), json!(["a.jpg", "b.jpg", "c.tiff"]));
    }

    #[test]
    fn image_list_reads_both_forms_and_ignores_junk() {
        assert_eq!(image_list(&json!(["a", " b ", "", 3])), ["a", "b"]);
        assert_eq!(image_list(&json!("a\n\n b")), ["a", "b"]);
        assert!(image_list(&Value::Null).is_empty());
    }

    #[test]
    fn unknown_table_or_row_is_an_error() {
        let (_d, a) = archive_with(r#"[{"_id":1}]"#);
        assert!(matches!(
            a.add_image("roll_film", 9, "x"),
            Err(ArchiveError::NoRow(..))
        ));
        assert!(matches!(
            a.add_image("nope", 1, "x"),
            Err(ArchiveError::NoTable(_))
        ));
    }
}
