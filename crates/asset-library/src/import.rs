//! Import a library made by the Python app: its database, thumbnails and model cache.
//!
//! The Python app's folder is only read: its database files are copied into our data dir
//! first, and the copy is what gets opened. Colours and embeddings are then recomputed in
//! the background (see PLAN.md); old embeddings keep serving search until replaced.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use crate::db::Database;

/// The folder holding `library.db`: the Python app's `data/` folder or the app folder itself.
pub fn find_data_dir(picked: &Path) -> Option<PathBuf> {
    [picked.to_path_buf(), picked.join("data")].into_iter().find(|d| d.join("library.db").is_file())
}

pub struct Imported {
    pub assets: i64,
    pub thumbnails: usize,
    pub copied_models: bool,
}

pub fn import_python_library(db: &Database, src: &Path, data_dir: &Path, model_dir: &Path) -> anyhow::Result<Imported> {
    let src = find_data_dir(src).context("No library.db in that folder or its data/ subfolder")?;
    if src.canonicalize().ok() == data_dir.canonicalize().ok() {
        bail!("That is this app's own library");
    }

    let staging = data_dir.join("import-staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    for suffix in ["", "-wal", "-shm"] {
        let from = src.join(format!("library.db{suffix}"));
        if from.exists() {
            std::fs::copy(&from, staging.join(format!("library.db{suffix}")))?;
        }
    }
    let result = (|| -> anyhow::Result<i64> {
        db.restore_from(&staging.join("library.db"))?;
        db.tx(|c| {
            // Recompute with this app's decoders; keep old vectors usable meanwhile.
            c.execute("UPDATE image_features SET colors = NULL", [])?;
            c.execute(
                "UPDATE image_features SET embed_model = embed_model || ' (python)'
                 WHERE embed_model IS NOT NULL AND embed_model NOT LIKE '% (python)'",
                [],
            )?;
            crate::db::reindex_fts(c, None)?;
            c.query_row("SELECT count(*) FROM assets", [], |r| r.get(0))
        })
        .map_err(Into::into)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    let assets = result?;

    let thumbnails = copy_tree(&src.join("thumbs"), &data_dir.join("thumbs"))?;
    let mut copied_models = false;
    for name in ["models--Qdrant--clip-ViT-B-32-vision", "models--Qdrant--clip-ViT-B-32-text"] {
        let (from, to) = (src.join("models").join(name), model_dir.join(name));
        if from.is_dir() && !to.exists() {
            copy_tree(&from, &to)?;
            copied_models = true;
        }
    }
    Ok(Imported { assets, thumbnails, copied_models })
}

/// Copy files that don't exist yet; recreate symlinks (Hugging Face caches use them).
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<usize> {
    if !from.is_dir() {
        return Ok(0);
    }
    std::fs::create_dir_all(to)?;
    let mut n = 0;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (src, dest) = (entry.path(), to.join(entry.file_name()));
        let ft = entry.file_type()?;
        if ft.is_dir() {
            n += copy_tree(&src, &dest)?;
        } else if dest.symlink_metadata().is_err() {
            #[cfg(unix)]
            if ft.is_symlink() {
                std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dest)?;
                continue;
            }
            std::fs::copy(&src, &dest)?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_and_marks_for_recompute() {
        let py = tempfile::tempdir().unwrap();
        let ours = tempfile::tempdir().unwrap();
        let data = py.path().join("data");
        {
            let src = Database::open(&data.join("library.db")).unwrap();
            src.tx(|c| {
                c.execute_batch(
                    "PRAGMA user_version = 0;
                     INSERT INTO sources (path, name) VALUES ('/x', 'x');
                     INSERT INTO assets (source_id, rel_path, file_hash, size, mtime, name)
                       VALUES (1, 'a/b.png', 'abcd', 1, 0, 'b');
                     INSERT INTO image_features VALUES ('abcd', '{}', x'00', 'Qdrant/clip-ViT-B-32-vision');",
                )
            })
            .unwrap();
        }
        std::fs::create_dir_all(data.join("thumbs/ab")).unwrap();
        std::fs::write(data.join("thumbs/ab/abcd.webp"), b"w").unwrap();

        let db = Database::open(&ours.path().join("library.db")).unwrap();
        let r = import_python_library(&db, py.path(), ours.path(), &ours.path().join("models")).unwrap();
        assert_eq!((r.assets, r.thumbnails), (1, 1));
        let (colors, model, version): (Option<String>, String, i64) = db
            .tx(|c| {
                let v = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
                c.query_row("SELECT colors, embed_model FROM image_features", [], |r| Ok((r.get(0)?, r.get(1)?, v)))
            })
            .unwrap();
        assert_eq!(colors, None);
        assert_eq!(model, "Qdrant/clip-ViT-B-32-vision (python)");
        assert!(version >= 1);
        assert!(ours.path().join("thumbs/ab/abcd.webp").exists());
    }
}
