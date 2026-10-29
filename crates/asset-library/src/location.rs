//! Where an asset's bytes live: a plain file, or an entry inside a Dungeondraft pack.

use std::io;
use std::path::{Path, PathBuf};

use crate::scanner::join_rel;

#[derive(Clone, Debug)]
pub enum Location {
    File(PathBuf),
    Entry { pack: PathBuf, inner: String, offset: u64, size: u64 },
}

impl Location {
    /// From an asset row's source path, rel_path and (for pack entries) container columns.
    pub fn new(root: &str, rel: &str, container: Option<&str>, offset: Option<i64>, size: Option<i64>) -> Self {
        let root = Path::new(root);
        match (container, offset, size) {
            (Some(c), Some(offset), Some(size)) => Location::Entry {
                pack: join_rel(root, c),
                inner: rel.strip_prefix(c).unwrap_or(rel).trim_start_matches('/').to_string(),
                offset: offset as u64,
                size: size as u64,
            },
            _ => Location::File(join_rel(root, rel)),
        }
    }

    pub fn read(&self) -> io::Result<Vec<u8>> {
        match self {
            Location::File(p) => std::fs::read(p),
            Location::Entry { pack, offset, size, .. } => crate::ddpack::read_entry(pack, *offset, *size),
        }
    }

    /// The file on disk: the asset itself, or the pack holding it.
    pub fn disk_path(&self) -> &Path {
        match self {
            Location::File(p) => p,
            Location::Entry { pack, .. } => pack,
        }
    }

    pub fn exists(&self) -> bool {
        self.disk_path().exists()
    }

    pub fn file_name(&self) -> String {
        let name = match self {
            Location::File(p) => p.file_name().map(|n| n.to_string_lossy().into_owned()),
            Location::Entry { inner, .. } => inner.rsplit('/').next().map(str::to_string),
        };
        name.unwrap_or_default()
    }

    /// For people: the file path, or `pack path › path inside the pack`.
    pub fn display(&self) -> String {
        match self {
            Location::File(p) => p.to_string_lossy().into_owned(),
            Location::Entry { pack, inner, .. } => format!("{} › {inner}", pack.display()),
        }
    }

    /// A real file with the asset's contents, for apps that need one (drag-out, open): the
    /// file itself, or the entry extracted to `cache/<hash[:2]>/<hash>/<file name>`.
    pub fn materialize(&self, cache: &Path, hash: &str) -> io::Result<PathBuf> {
        let Location::Entry { .. } = self else { return Ok(self.disk_path().to_path_buf()) };
        let dir = cache.join(hash.get(..2).unwrap_or(hash)).join(hash);
        let out = dir.join(self.file_name());
        if !out.exists() {
            std::fs::create_dir_all(&dir)?;
            let tmp = dir.join(format!(".{}.tmp", std::process::id()));
            std::fs::write(&tmp, self.read()?)?;
            std::fs::rename(&tmp, &out)?;
        }
        Ok(out)
    }
}

/// Delete extracted files older than `max_age` (they're only needed while dragging/opening).
pub fn prune_extracted(cache: &Path, max_age: std::time::Duration) {
    let Ok(dirs) = std::fs::read_dir(cache) else { return };
    for prefix in dirs.flatten() {
        for entry in std::fs::read_dir(prefix.path()).into_iter().flatten().flatten() {
            let old = entry.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().unwrap_or_default() > max_age);
            if old {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let _ = std::fs::remove_dir(prefix.path()); // only succeeds once empty
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_materializes_entries() {
        let dir = tempfile::tempdir().unwrap();
        let pack = dir.path().join("P.dungeondraft_pack");
        crate::ddpack::tests::write_pack(&pack, &[("res://packs/x/textures/objects/a/chair.png", b"CHAIR")]);
        let p = crate::ddpack::open(&pack).unwrap();
        let e = &p.entries[0];
        let root = dir.path().to_string_lossy();
        let rel = format!("P.dungeondraft_pack/{}", e.path);
        let loc = Location::new(&root, &rel, Some("P.dungeondraft_pack"), Some(e.offset as i64), Some(e.size as i64));
        assert_eq!(loc.read().unwrap(), b"CHAIR");
        assert_eq!(loc.file_name(), "chair.png");
        assert!(loc.exists() && loc.display().ends_with("P.dungeondraft_pack › textures/objects/a/chair.png"));
        let out = loc.materialize(&dir.path().join("cache"), "abcdef").unwrap();
        assert!(out.ends_with("ab/abcdef/chair.png"));
        assert_eq!(std::fs::read(&out).unwrap(), b"CHAIR");
        prune_extracted(&dir.path().join("cache"), std::time::Duration::ZERO);
        assert!(!out.exists());

        let file = Location::new(&root, "P.dungeondraft_pack", None, None, None);
        assert_eq!(file.materialize(dir.path(), "x").unwrap(), pack);
    }
}
