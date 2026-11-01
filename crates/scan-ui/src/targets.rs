//! Films and enlargements of a latent archive that a scan can be attached to.
//! Reads `latent.schema.json` (for table roles) and `latent.data.json`; never writes.

use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct TargetTable {
    pub name: String,
    /// Row id and a short label.
    pub rows: Vec<(u64, String)>,
}

const LABEL_FIELDS: [&str; 4] = ["film_name", "title", "name", "motiv"];
const SCHEMA_FILE: &str = "latent.schema.json";

pub fn load(archive: &Path) -> Result<Vec<TargetTable>, String> {
    let data = std::fs::read_to_string(archive.join(scan_archive::DATA_FILE))
        .map_err(|e| format!("{}: {e}", scan_archive::DATA_FILE))?;
    let schema = std::fs::read_to_string(archive.join(SCHEMA_FILE)).ok();
    parse(&data, schema.as_deref())
}

fn parse(data: &str, schema: Option<&str>) -> Result<Vec<TargetTable>, String> {
    let data: Value = serde_json::from_str(data).map_err(|e| e.to_string())?;
    let tables = data["tables"].as_object().ok_or("no tables in data file")?;

    // Only film and enlargement tables take scans; without a schema offer everything.
    let names: Vec<String> = match schema.and_then(|s| serde_json::from_str::<Value>(s).ok()) {
        Some(s) => s["tables"]
            .as_array()
            .map(|t| {
                t.iter()
                    .filter(|t| matches!(t["role"].as_str(), Some("film" | "enlargement")))
                    .filter_map(|t| t["name"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        None => tables.keys().cloned().collect(),
    };

    Ok(names
        .into_iter()
        .filter_map(|name| {
            let rows = tables.get(&name)?["rows"].as_array()?;
            let rows = rows
                .iter()
                .filter_map(|r| Some((r["id"].as_u64().or_else(|| r["_id"].as_u64())?, label(r))))
                .collect();
            Some(TargetTable { name, rows })
        })
        .collect())
}

/// The archive-relative image paths of row `id` in `table`, as stored in the data file (a list,
/// or a newline-separated string written by older versions). Empty when the row is missing.
pub fn row_images(archive: &Path, table: &str, id: u64) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(archive.join(scan_archive::DATA_FILE)) else {
        return vec![];
    };
    images_of(&text, table, id)
}

/// Image files directly in `dir` (png, tif, tiff, jpg, jpeg), oldest first by modification time,
/// then by name, as file names. Empty when the folder does not exist.
pub fn folder_images(dir: &Path) -> Vec<String> {
    const EXTENSIONS: [&str; 5] = ["png", "tif", "tiff", "jpg", "jpeg"];
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut files: Vec<(std::time::SystemTime, String)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .filter(|e| {
            e.path()
                .extension()
                .map(|x| x.to_string_lossy().to_lowercase())
                .is_some_and(|x| EXTENSIONS.contains(&x.as_str()))
        })
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.file_name().to_string_lossy().into_owned()))
        })
        .collect();
    files.sort();
    files.into_iter().map(|(_, name)| name).collect()
}

fn images_of(data: &str, table: &str, id: u64) -> Vec<String> {
    let Ok(data) = serde_json::from_str::<Value>(data) else {
        return vec![];
    };
    let rows = data["tables"][table]["rows"].as_array();
    let row = rows.and_then(|r| {
        r.iter()
            .find(|r| ["id", "_id"].iter().any(|k| r[*k].as_u64() == Some(id)))
    });
    row.map(|r| scan_archive::image_list(&r["images"]))
        .unwrap_or_default()
}

fn label(row: &Value) -> String {
    let text = |k: &str| row[k].as_str().filter(|s| !s.trim().is_empty());
    let found = LABEL_FIELDS.iter().find_map(|k| text(k)).or_else(|| {
        row.as_object()?
            .iter()
            .find(|(k, v)| !k.starts_with('_') && *k != "images" && v.is_string())
            .and_then(|(_, v)| v.as_str())
    });
    found.unwrap_or("").chars().take(40).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA: &str = r#"{"tables":{
        "roll_film":{"rows":[{"id":1,"film_name":"Ilford FP4","camera":"Nikon"},{"id":2}]},
        "enlargement":{"rows":[{"_id":5,"notice":"dodge"}]},
        "image_info":{"rows":[{"_id":1}]}}}"#;
    const SCHEMA: &str = r#"{"tables":[
        {"name":"roll_film","role":"film"},{"name":"enlargement","role":"enlargement"},
        {"name":"image_info","role":"image_info"}]}"#;

    #[test]
    fn lists_film_and_enlargement_tables_in_schema_order() {
        let t = parse(DATA, Some(SCHEMA)).unwrap();
        assert_eq!(
            t.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["roll_film", "enlargement"]
        );
        assert_eq!(
            t[0].rows,
            vec![(1, "Ilford FP4".to_string()), (2, String::new())]
        );
        assert_eq!(t[1].rows, vec![(5, "dodge".to_string())]);
    }

    #[test]
    fn without_schema_all_tables_are_offered() {
        assert_eq!(parse(DATA, None).unwrap().len(), 3);
    }

    #[test]
    fn row_images_reads_lists_and_legacy_strings() {
        let data = r#"{"tables":{"roll_film":{"rows":[
            {"id":1,"images":["a.png","b.tiff"]},
            {"_id":2,"images":"c.png\nd.png"},
            {"id":3}]}}}"#;
        assert_eq!(images_of(data, "roll_film", 1), ["a.png", "b.tiff"]);
        assert_eq!(images_of(data, "roll_film", 2), ["c.png", "d.png"]);
        assert!(images_of(data, "roll_film", 3).is_empty());
        assert!(images_of(data, "roll_film", 9).is_empty());
        assert!(images_of(data, "nope", 1).is_empty());
        assert!(images_of("not json", "roll_film", 1).is_empty());
    }

    #[test]
    fn folder_images_lists_image_files_oldest_first() {
        let dir = std::env::temp_dir().join(format!("scan-folder-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let touch = |name: &str, secs: u64| {
            let p = dir.join(name);
            std::fs::write(&p, b"x").unwrap();
            let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + secs);
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
        };
        touch("b.png", 10);
        touch("a.TIF", 20);
        touch("c.jpeg", 10);
        touch("notes.txt", 5);
        touch("d.tiff", 30);
        assert_eq!(folder_images(&dir), ["b.png", "c.jpeg", "a.TIF", "d.tiff"]);
        assert!(folder_images(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn invalid_data_is_an_error() {
        assert!(parse("{}", None).is_err());
    }
}
