//! Get selected assets out of the library: copy to a folder, or build a zip.

use std::collections::HashSet;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use zip::write::SimpleFileOptions;

use crate::location::Location;
use zip::{CompressionMethod, DateTime, ZipWriter};

pub struct Item {
    /// Where it lands under `source_name` when not flattened.
    pub rel_path: String,
    pub source_name: String,
    pub location: Location,
}

/// `name (n).ext` for the n-th duplicate of `name.ext`.
fn numbered(path: &Path, n: u32) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    path.with_file_name(format!("{stem} ({n}){ext}"))
}

fn unique(dest: PathBuf, used: &mut HashSet<String>) -> PathBuf {
    let (mut candidate, mut n) = (dest.clone(), 1);
    while candidate.exists() || used.contains(&candidate.to_string_lossy().to_lowercase()) {
        candidate = numbered(&dest, n);
        n += 1;
    }
    used.insert(candidate.to_string_lossy().to_lowercase());
    candidate
}

/// Relative target as `/`-separated parts: just the file name, or source name + rel path.
fn target_parts(item: &Item, flatten: bool) -> Vec<&str> {
    let mut parts: Vec<&str> = item.rel_path.split('/').filter(|p| !p.is_empty()).collect();
    if flatten {
        parts.drain(..parts.len().saturating_sub(1));
    } else {
        parts.insert(0, &item.source_name);
    }
    parts
}

/// Copy items into `dest_dir` without overwriting anything; returns the paths written.
pub fn copy_to_folder(items: &[Item], dest_dir: &Path, flatten: bool) -> std::io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dest_dir)?;
    let mut used = HashSet::new();
    let mut written = Vec::new();
    for item in items {
        let mut dest = dest_dir.to_path_buf();
        dest.extend(target_parts(item, flatten));
        let dest = unique(dest, &mut used);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match &item.location {
            Location::File(src) => {
                std::fs::copy(src, &dest)?;
                // Keep the modification time, like shutil.copy2.
                if let Ok(modified) = std::fs::metadata(src).and_then(|m| m.modified()) {
                    let _ = std::fs::File::options().write(true).open(&dest).and_then(|f| f.set_modified(modified));
                }
            }
            entry => std::fs::write(&dest, entry.read()?)?,
        }
        written.push(dest);
    }
    Ok(written)
}

/// An uncompressed zip (images are already compressed) with duplicate names numbered.
pub fn build_zip(items: &[Item], flatten: bool) -> anyhow::Result<Vec<u8>> {
    Ok(write_zip(Cursor::new(Vec::new()), items, flatten)?.into_inner())
}

/// Write the zip to `out` (a file, for exports too big to hold in memory).
pub fn write_zip<W: Write + std::io::Seek>(out: W, items: &[Item], flatten: bool) -> anyhow::Result<W> {
    let mut zf = ZipWriter::new(out);
    let mut used = HashSet::new();
    for item in items {
        let base = target_parts(item, flatten).join("/");
        let (mut name, mut n) = (base.clone(), 1);
        while used.contains(&name.to_lowercase()) {
            name = numbered(Path::new(&base), n).to_string_lossy().replace('\\', "/");
            n += 1;
        }
        used.insert(name.to_lowercase());
        let data = item.location.read()?;
        let mut opts = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(data.len() as u64 >= u32::MAX as u64);
        if let Some(t) = zip_time(item.location.disk_path()) {
            opts = opts.last_modified_time(t);
        }
        zf.start_file(name, opts)?;
        zf.write_all(&data)?;
    }
    Ok(zf.finish()?)
}

/// The file's modification time (UTC) as a zip timestamp.
fn zip_time(path: &Path) -> Option<DateTime> {
    let secs = std::fs::metadata(path).ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as u16;
    let (h, m, s) = ((rem / 3600) as u8, (rem % 3600 / 60) as u8, (rem % 60) as u8);
    DateTime::from_date_and_time(year, month, day, h, m, s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(dir: &Path, rel: &str) -> Item {
        let full_path = crate::scanner::join_rel(dir, rel);
        std::fs::create_dir_all(full_path.parent().unwrap()).unwrap();
        std::fs::write(&full_path, rel).unwrap();
        Item { rel_path: rel.into(), source_name: "src".into(), location: Location::File(full_path) }
    }

    #[test]
    fn zip_numbers_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let items = [item(dir.path(), "a/x.png"), item(dir.path(), "b/X.png"), item(dir.path(), "c/x.png")];
        let data = build_zip(&items, true).unwrap();
        let zf = zip::ZipArchive::new(Cursor::new(data)).unwrap();
        let mut names: Vec<String> = zf.file_names().map(|n| n.unwrap().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["X (1).png", "x (2).png", "x.png"]);
    }

    #[test]
    fn copy_keeps_structure_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let items = [item(dir.path(), "a/x.png")];
        let out = dir.path().join("out");
        copy_to_folder(&items, &out, false).unwrap();
        let again = copy_to_folder(&items, &out, false).unwrap();
        assert!(out.join("src/a/x.png").exists());
        assert_eq!(again, [out.join("src/a/x (1).png")]);
    }
}
