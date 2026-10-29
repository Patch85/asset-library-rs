//! Index image files in place, as in the original's `app/scanner.py`.
//!
//! Files are never moved or modified. Each scan:
//!   1. walks the source folder and refreshes rows whose size/mtime changed,
//!   2. flags rows whose file disappeared as `missing`,
//!   3. for new files, first looks for a missing asset with the same content hash
//!      (a moved/renamed file) and re-points it so tags and notes survive;
//!      otherwise inserts a new asset.

use std::collections::{HashMap, HashSet};
use std::fs::{self, Metadata};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::UNIX_EPOCH;

use anyhow::bail;
use image::{DynamicImage, ImageDecoder, ImageReader};
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::Serialize;
use sha1::{Digest, Sha1};

use crate::config::IMAGE_EXTENSIONS;
use crate::db::{Database, get_or_create_tags, marks, reindex_fts};

#[derive(Clone, Default, Serialize)]
pub struct ScanStatus {
    pub running: bool,
    pub source: String,
    pub processed: u64,
    pub added: u64,
    pub updated: u64,
    pub moved: u64,
    pub missing: u64,
    /// Files with an image extension that aren't readable images.
    pub skipped: u64,
    /// Index rows dropped because the file is now excluded or unreadable.
    pub removed: u64,
    pub errors: Vec<String>,
}

pub struct Scanner {
    db: Arc<Database>,
    status: Mutex<ScanStatus>,
    scan_lock: Mutex<()>,
}

struct Source {
    path: String,
    name: String,
    asset_type: String,
    folder_tags: bool,
}

struct Known {
    id: i64,
    size: i64,
    mtime: f64,
    missing: bool,
    width: Option<i64>,
}

impl Scanner {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db, status: Mutex::default(), scan_lock: Mutex::default() }
    }

    pub fn status(&self) -> MutexGuard<'_, ScanStatus> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Scan the sources on a background thread, then run `after`. Returns false if a scan
    /// is already running. `after` runs before `running` clears, so a job it starts can mark
    /// itself running first and pollers never see an idle gap.
    pub fn scan_in_background(
        self: &Arc<Self>,
        source_ids: Vec<i64>,
        after: impl FnOnce() + Send + 'static,
    ) -> bool {
        {
            let mut st = self.status();
            if st.running {
                return false;
            }
            st.running = true; // claim immediately so the UI sees it
        }
        let this = Arc::clone(self);
        std::thread::spawn(move || {
            for sid in source_ids {
                this.scan_source(sid);
            }
            // Unlike the original, `running` stays set until every source is done, so
            // pollers can't see an idle gap between sources.
            after();
            this.status().running = false;
        });
        true
    }

    fn scan_source(&self, source_id: i64) {
        let _guard = self.scan_lock.lock().unwrap_or_else(|e| e.into_inner());
        *self.status() = ScanStatus { running: true, ..Default::default() };
        if let Err(e) = self.scan(source_id) {
            self.status().errors.push(e.to_string()); // surface to the UI
        }
    }

    fn file_error(&self, rel: &str, e: impl std::fmt::Display) {
        self.status().errors.push(format!("{rel}: {e}"));
    }

    fn scan(&self, source_id: i64) -> anyhow::Result<()> {
        let (src, known) = self.db.tx(|c| -> anyhow::Result<_> {
            let src = c
                .query_row(
                    "SELECT path, name, asset_type, folder_tags FROM sources WHERE id = ?",
                    params![source_id],
                    |r| {
                        Ok(Source {
                            path: r.get(0)?,
                            name: r.get(1)?,
                            asset_type: r.get(2)?,
                            folder_tags: r.get(3)?,
                        })
                    },
                )
                .optional()?;
            let Some(src) = src else { bail!("No source {source_id}") };
            let mut stmt = c.prepare(
                "SELECT id, rel_path, size, mtime, missing, width FROM assets WHERE source_id = ?",
            )?;
            let known = stmt
                .query_map(params![source_id], |r| {
                    Ok((
                        r.get::<_, String>(1)?,
                        Known {
                            id: r.get(0)?,
                            size: r.get(2)?,
                            mtime: r.get(3)?,
                            missing: r.get(4)?,
                            width: r.get(5)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<HashMap<_, _>>>()?;
            Ok((src, known))
        })?;
        let root = PathBuf::from(&src.path);
        self.status().source = src.name.clone();
        if !root.is_dir() {
            bail!("Folder not found: {}", root.display());
        }

        let mut seen: HashSet<String> = HashSet::new();
        let mut new_files: Vec<(PathBuf, String)> = Vec::new();
        let mut touched: Vec<i64> = Vec::new();
        let mut remove: Vec<i64> = Vec::new();

        // Pass 1: refresh known files, collect new ones.
        for (full, rel) in iter_images(&root) {
            self.status().processed += 1;
            seen.insert(rel.clone());
            let Some(row) = known.get(&rel) else {
                new_files.push((full, rel));
                continue;
            };
            let md = match fs::metadata(&full) {
                Ok(md) => md,
                Err(e) => {
                    self.file_error(&rel, e);
                    continue;
                }
            };
            let unchanged = md.len() as i64 == row.size && mtime(&md) == row.mtime;
            if unchanged && row.width.is_some() {
                if row.missing {
                    self.db.tx(|c| {
                        c.execute("UPDATE assets SET missing = 0 WHERE id = ?", params![row.id])
                    })?;
                }
                continue;
            }
            let Some((w, h, alpha)) = image_info(&full) else {
                remove.push(row.id);
                continue;
            };
            let digest = match file_hash(&full) {
                Ok(d) => d,
                Err(e) => {
                    self.file_error(&rel, e);
                    continue;
                }
            };
            self.db.tx(|c| {
                c.execute(
                    "UPDATE assets SET file_hash=?, size=?, mtime=?, width=?, height=?,
                     has_alpha=?, missing=0 WHERE id=?",
                    params![digest, md.len() as i64, mtime(&md), w, h, alpha, row.id],
                )
            })?;
            self.status().updated += 1;
        }

        // Not walked but still on disk => now excluded (e.g. __MACOSX); drop from index.
        let unseen: Vec<(&String, &Known)> =
            known.iter().filter(|(rel, _)| !seen.contains(*rel)).collect();
        remove.extend(unseen.iter().filter(|(rel, _)| join_rel(&root, rel).exists()).map(|(_, r)| r.id));

        // Flag vanished files before matching moves so they're candidates.
        let gone: Vec<i64> = unseen
            .iter()
            .filter(|(_, r)| !r.missing && !remove.contains(&r.id))
            .map(|(_, r)| r.id)
            .collect();
        if !gone.is_empty() {
            self.db.tx(|c| {
                let mut stmt = c.prepare("UPDATE assets SET missing = 1 WHERE id = ?")?;
                for id in &gone {
                    stmt.execute(params![id])?;
                }
                Ok::<_, rusqlite::Error>(())
            })?;
        }
        self.status().missing = gone.len() as u64;

        // Pass 2: new files -- either a moved asset or a brand-new one.
        for (full, rel) in new_files {
            let md = match fs::metadata(&full) {
                Ok(md) => md,
                Err(e) => {
                    self.file_error(&rel, e);
                    continue;
                }
            };
            let Some((w, h, alpha)) = image_info(&full) else {
                self.status().skipped += 1;
                continue;
            };
            let digest = match file_hash(&full) {
                Ok(d) => d,
                Err(e) => {
                    self.file_error(&rel, e);
                    continue;
                }
            };
            let (id, moved) = self.db.tx(|c| -> rusqlite::Result<_> {
                if let Some(id) = find_moved(c, &digest)? {
                    c.execute(
                        "UPDATE assets SET source_id=?, rel_path=?, size=?, mtime=?, missing=0
                         WHERE id=?",
                        params![source_id, rel, md.len() as i64, mtime(&md), id],
                    )?;
                    return Ok((id, true));
                }
                c.execute(
                    "INSERT INTO assets (source_id, rel_path, file_hash, size, mtime,
                     width, height, has_alpha, asset_type, name)
                     VALUES (?,?,?,?,?,?,?,?,?,?)",
                    params![
                        source_id,
                        rel,
                        digest,
                        md.len() as i64,
                        mtime(&md),
                        w,
                        h,
                        alpha,
                        src.asset_type,
                        display_name(&rel)
                    ],
                )?;
                let id = c.last_insert_rowid();
                if src.folder_tags {
                    for tag_id in get_or_create_tags(c, &folder_tag_names(&rel))? {
                        c.execute(
                            "INSERT OR IGNORE INTO asset_tags (asset_id, tag_id) VALUES (?,?)",
                            params![id, tag_id],
                        )?;
                    }
                }
                Ok((id, false))
            })?;
            touched.push(id);
            let mut st = self.status();
            if moved {
                st.moved += 1;
                st.missing = st.missing.saturating_sub(1);
            } else {
                st.added += 1;
            }
        }

        self.db.tx(|c| -> rusqlite::Result<()> {
            if !remove.is_empty() {
                let m = marks(remove.len());
                c.execute(
                    &format!("DELETE FROM assets_fts WHERE rowid IN ({m})"),
                    params_from_iter(&remove),
                )?;
                c.execute(&format!("DELETE FROM assets WHERE id IN ({m})"), params_from_iter(&remove))?;
                c.execute("DELETE FROM tags WHERE id NOT IN (SELECT tag_id FROM asset_tags)", [])?;
            }
            self.status().removed = remove.len() as u64;
            reindex_fts(c, Some(&touched))?;
            c.execute(
                "UPDATE sources SET last_scan = datetime('now') WHERE id = ?",
                params![source_id],
            )?;
            Ok(())
        })?;
        Ok(())
    }
}

/// An existing asset with this hash whose file no longer exists on disk.
fn find_moved(c: &Connection, digest: &str) -> rusqlite::Result<Option<i64>> {
    let mut stmt = c.prepare(
        "SELECT a.id, a.rel_path, s.path FROM assets a
         JOIN sources s ON s.id = a.source_id WHERE a.file_hash = ?
         ORDER BY a.missing DESC",
    )?;
    let rows = stmt.query_map(params![digest], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })?;
    for row in rows {
        let (id, rel, root) = row?;
        if !join_rel(Path::new(&root), &rel).exists() {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// `root` joined with a stored `/`-separated relative path, using native separators.
pub fn join_rel(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    p.extend(rel.split('/').filter(|s| !s.is_empty()));
    p
}

/// Modification time as Python's `os.stat().st_mtime` computes it, so stored values compare
/// equal and an imported library isn't rehashed.
fn mtime(md: &Metadata) -> f64 {
    match md.modified().map(|t| t.duration_since(UNIX_EPOCH)) {
        Ok(Ok(d)) => d.as_secs() as f64 + d.subsec_nanos() as f64 * 1e-9,
        Ok(Err(e)) => -e.duration().as_secs_f64(),
        Err(_) => 0.0,
    }
}

pub fn file_hash(path: &Path) -> io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha1::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Width, height and whether any pixel is transparent; `None` if the file isn't a readable
/// image. Like Pillow, only the header is read unless the format can carry alpha.
pub fn image_info(path: &Path) -> Option<(u32, u32, bool)> {
    let decoder = ImageReader::open(path).ok()?.with_guessed_format().ok()?.into_decoder().ok()?;
    let (w, h) = decoder.dimensions();
    if !decoder.color_type().has_alpha() {
        return Some((w, h, false));
    }
    // Many opaque images are saved as RGBA; check for real transparency.
    let img = DynamicImage::from_decoder(decoder).ok()?;
    let alpha = match img.as_rgba8() {
        Some(buf) => buf.pixels().any(|p| p[3] < 255),
        None => img.to_rgba8().pixels().any(|p| p[3] < 255),
    };
    Some((w, h, alpha))
}

/// The file stem with underscores and runs of whitespace collapsed to single spaces.
pub fn display_name(rel_path: &str) -> String {
    let stem = Path::new(rel_path).file_stem().and_then(|s| s.to_str()).unwrap_or("");
    stem.replace('_', " ").split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn folder_tag_names(rel_path: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = rel_path.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    parts.pop(); // the file name
    parts
}

/// macOS zip leftovers: __MACOSX/ folders and "._name.png" resource forks look like images
/// by extension but aren't. Compared lowercase.
const SKIP_DIRS: &[&str] = &["__macosx", "$recycle.bin", "system volume information"];

/// Image files under `root` as (full path, `/`-separated relative path), in the same
/// top-down order as Python's `os.walk`. Hidden files and folders are skipped, and
/// symlinked folders are not followed. Names that aren't valid UTF-8 are skipped, since the
/// database stores paths as text.
pub fn iter_images(root: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    walk(root, "", &mut out);
    out
}

fn walk(dir: &Path, rel_dir: &str, out: &mut Vec<(PathBuf, String)>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else { continue };
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let rel = if rel_dir.is_empty() { name.to_string() } else { format!("{rel_dir}/{name}") };
        let Ok(ft) = entry.file_type() else { continue };
        let is_dir = if ft.is_symlink() { path.is_dir() } else { ft.is_dir() };
        if is_dir {
            if !ft.is_symlink() && !SKIP_DIRS.contains(&name.to_lowercase().as_str()) {
                subdirs.push((path, rel));
            }
            continue;
        }
        let ext = Path::new(name).extension().and_then(|e| e.to_str()).map(str::to_lowercase);
        if ext.is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.as_str())) {
            out.push((path, rel));
        }
    }
    for (path, rel) in subdirs {
        walk(&path, &rel, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_folder_tags() {
        assert_eq!(display_name("Undead/Skeleton_Archer.png"), "Skeleton Archer");
        assert_eq!(display_name("a/b/goblin  boss__2.webp"), "goblin boss 2");
        assert_eq!(display_name("x/v1.2.png"), "v1.2");
        assert_eq!(folder_tag_names("Undead/Big Ones/x.png"), ["Undead", "Big Ones"]);
        assert!(folder_tag_names("x.png").is_empty());
    }

    #[test]
    fn hashes_like_sha1() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        fs::write(&p, b"abc").unwrap();
        assert_eq!(file_hash(&p).unwrap(), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn walks_like_os_walk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for rel in ["a.png", "B.JPG", "skip.txt", ".hidden.png", "sub/c.gif", "__MACOSX/d.png",
                    ".git/e.png", "sub/deeper/f.webp"] {
            let p = join_rel(root, rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, b"").unwrap();
        }
        let mut rels: Vec<String> = iter_images(root).into_iter().map(|(_, r)| r).collect();
        rels.sort();
        assert_eq!(rels, ["B.JPG", "a.png", "sub/c.gif", "sub/deeper/f.webp"]);
    }
}
