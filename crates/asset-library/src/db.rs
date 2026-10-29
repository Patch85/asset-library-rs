//! SQLite storage, schema-compatible with the Python original (`app/db.py`).
//!
//! Every indexed file is a row in `assets` with an `asset_type`; type-specific fields live in
//! `assets.extra` (JSON). Schema changes go through `PRAGMA user_version` migrations so a
//! library created by the Python app opens as-is.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, Transaction, params, params_from_iter};

/// The original's schema, verbatim. `IF NOT EXISTS` makes it a no-op on a Python library.
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS sources (
    id          INTEGER PRIMARY KEY,
    path        TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    asset_type  TEXT NOT NULL DEFAULT 'token',   -- default type for files found here
    folder_tags INTEGER NOT NULL DEFAULT 1,      -- auto-tag new files with folder names
    last_scan   TEXT
);

CREATE TABLE IF NOT EXISTS assets (
    id          INTEGER PRIMARY KEY,
    source_id   INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    rel_path    TEXT NOT NULL,                   -- relative to source path
    file_hash   TEXT NOT NULL,
    size        INTEGER NOT NULL,
    mtime       REAL NOT NULL,
    width       INTEGER,
    height      INTEGER,
    has_alpha   INTEGER NOT NULL DEFAULT 0,
    asset_type  TEXT NOT NULL DEFAULT 'token',
    name        TEXT NOT NULL,
    notes       TEXT NOT NULL DEFAULT '',
    favorite    INTEGER NOT NULL DEFAULT 0,
    missing     INTEGER NOT NULL DEFAULT 0,      -- file vanished on last scan
    extra       TEXT NOT NULL DEFAULT '{}',
    added_at    TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (source_id, rel_path)
);
CREATE INDEX IF NOT EXISTS idx_assets_hash ON assets(file_hash);
CREATE INDEX IF NOT EXISTS idx_assets_type ON assets(asset_type);

CREATE TABLE IF NOT EXISTS tags (
    id    INTEGER PRIMARY KEY,
    name  TEXT NOT NULL UNIQUE COLLATE NOCASE
);

CREATE TABLE IF NOT EXISTS asset_tags (
    asset_id INTEGER NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    tag_id   INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    PRIMARY KEY (asset_id, tag_id)
);
CREATE INDEX IF NOT EXISTS idx_asset_tags_tag ON asset_tags(tag_id);

-- Derived image data, keyed by content hash so moved/duplicate files share it.
CREATE TABLE IF NOT EXISTS image_features (
    file_hash    TEXT PRIMARY KEY,
    colors       TEXT,              -- JSON {color name: fraction of opaque pixels}
    embedding    BLOB,              -- float32, L2-normalised (optional AI extra)
    embed_model  TEXT
);

-- Tag suggestions the user dismissed for one asset, or for every asset.
CREATE TABLE IF NOT EXISTS suggestion_rejects (
    asset_id INTEGER NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    tag      TEXT NOT NULL,
    PRIMARY KEY (asset_id, tag)
);
CREATE TABLE IF NOT EXISTS suggestion_ignore (tag TEXT PRIMARY KEY);

-- Full-text index over name, folder path, notes and tags. Maintained by
-- reindex_fts() rather than triggers so tag changes stay simple.
CREATE VIRTUAL TABLE IF NOT EXISTS assets_fts USING fts5(
    name, path, notes, tags,
    tokenize = "unicode61 tokenchars '-'"
);
"#;

/// Migration `i` takes the database from `user_version` i to i + 1.
const MIGRATIONS: &[&str] = &[SCHEMA_V1];

/// One shared connection; like the original, every access is serialized.
pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        migrate(&mut conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Run `f` in a transaction that commits on `Ok` and rolls back on `Err`.
    pub fn tx<T, E: From<rusqlite::Error>>(
        &self,
        f: impl FnOnce(&Transaction) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic mid-transaction rolls back on drop, so the connection is still usable.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version.max(0) as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

pub fn marks(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// Rebuild FTS rows for the given assets, or for all assets when `asset_ids` is `None`.
pub fn reindex_fts(conn: &Connection, asset_ids: Option<&[i64]>) -> rusqlite::Result<()> {
    let (filter, ids): (String, &[i64]) = match asset_ids {
        Some([]) => return Ok(()),
        Some(ids) => {
            let m = marks(ids.len());
            conn.execute(
                &format!("DELETE FROM assets_fts WHERE rowid IN ({m})"),
                params_from_iter(ids),
            )?;
            (format!("WHERE a.id IN ({m})"), ids)
        }
        None => {
            conn.execute("DELETE FROM assets_fts", [])?;
            (String::new(), &[])
        }
    };
    conn.execute(
        &format!(
            r#"INSERT INTO assets_fts (rowid, name, path, notes, tags)
               SELECT a.id, a.name,
                      replace(replace(replace(a.rel_path, '\', ' '), '/', ' '), '_', ' '),
                      a.notes,
                      coalesce((SELECT group_concat(t.name, ' ') FROM asset_tags at
                                JOIN tags t ON t.id = at.tag_id WHERE at.asset_id = a.id), '')
               FROM assets a {filter}"#
        ),
        params_from_iter(ids),
    )?;
    Ok(())
}

/// Ids for the normalized form of each name, creating missing tags. Empty names are dropped.
pub fn get_or_create_tags<S: AsRef<str>>(
    conn: &Connection,
    names: &[S],
) -> rusqlite::Result<Vec<i64>> {
    let mut ids = Vec::new();
    for raw in names {
        let name = normalize_tag(raw.as_ref());
        if name.is_empty() {
            continue;
        }
        conn.execute("INSERT OR IGNORE INTO tags (name) VALUES (?)", params![name])?;
        ids.push(conn.query_row("SELECT id FROM tags WHERE name = ?", params![name], |r| {
            r.get(0)
        })?);
    }
    Ok(ids)
}

/// Lowercase, with runs of whitespace collapsed to `-`.
pub fn normalize_tag(name: &str) -> String {
    name.split_whitespace().map(str::to_lowercase).collect::<Vec<_>>().join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_tags() {
        assert_eq!(normalize_tag("  Low   Level "), "low-level");
        assert_eq!(normalize_tag("CR-1"), "cr-1");
        assert_eq!(normalize_tag("   "), "");
    }

    #[test]
    fn migrates_once_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        Database::open(&path).unwrap();
        let db = Database::open(&path).unwrap();
        let v: i64 = db
            .tx(|c| c.pragma_query_value(None, "user_version", |r| r.get(0)))
            .unwrap();
        assert_eq!(v, MIGRATIONS.len() as i64);
    }

    /// The real library (gitignored; see tools/make_golden.py) must open without changes.
    #[test]
    fn opens_golden_library() {
        let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden/library.db");
        if !golden.exists() {
            eprintln!("skipped: {} not present", golden.display());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        std::fs::copy(&golden, &path).unwrap();
        let db = Database::open(&path).unwrap();
        let n: i64 = db.tx(|c| c.query_row("SELECT count(*) FROM assets", [], |r| r.get(0))).unwrap();
        assert!(n > 0);
    }
}
