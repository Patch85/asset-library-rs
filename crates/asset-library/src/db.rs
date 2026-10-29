//! SQLite storage, schema-compatible with the Python original (`app/db.py`).
//!
//! Every indexed file is a row in `assets` with an `asset_type`; type-specific fields live in
//! `assets.extra` (JSON). Schema changes go through `PRAGMA user_version` migrations so a
//! library created by the Python app opens as-is.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, Transaction, params, params_from_iter};

/// The original's schema, verbatim. `IF NOT EXISTS` makes it a no-op on a Python library.
pub(crate) const SCHEMA_V1: &str = r#"
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

/// Properties: per-type fields with one value per asset (see PLAN.md, M8).
const PROPERTIES_V2: &str = r#"
CREATE TABLE fields (
    id          INTEGER PRIMARY KEY,
    asset_type  TEXT NOT NULL,
    key         TEXT NOT NULL COLLATE NOCASE,   -- used in search: key:value
    label       TEXT NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN ('choice', 'number', 'text')),
    options     TEXT NOT NULL DEFAULT '[]',     -- JSON list of values, for choice fields
    position    INTEGER NOT NULL DEFAULT 0,
    UNIQUE (asset_type, key)
);

CREATE TABLE asset_fields (
    asset_id INTEGER NOT NULL REFERENCES assets(id) ON DELETE CASCADE,
    field_id INTEGER NOT NULL REFERENCES fields(id) ON DELETE CASCADE,
    value    NOT NULL,                          -- TEXT, or REAL for number fields
    PRIMARY KEY (asset_id, field_id)
);
CREATE INDEX idx_asset_fields_value ON asset_fields(field_id, value);

INSERT INTO fields (asset_type, key, label, kind, options, position) VALUES
    ('token', 'view', 'View', 'choice', '["top-down","portrait","isometric","side"]', 0),
    ('token', 'size', 'Size', 'choice', '["tiny","small","medium","large","huge","gargantuan"]', 1),
    ('token', 'pack', 'Pack', 'text', '[]', 2),
    ('map', 'grid', 'Grid (px per square)', 'number', '[]', 0),
    ('map', 'variant', 'Variant', 'choice', '["day","night","dawn","dusk","rain","snow","fog"]', 1),
    ('map', 'pack', 'Pack', 'text', '[]', 2);
"#;

/// Dungeondraft packs, indexed in place (see PLAN.md, M9).
const PACKS_V3: &str = r#"
-- For an image inside a pack: the pack's path within the source, and where the bytes are.
ALTER TABLE assets ADD COLUMN container TEXT;
ALTER TABLE assets ADD COLUMN entry_offset INTEGER;
ALTER TABLE assets ADD COLUMN entry_size INTEGER;

-- Packs seen by the scanner; one is only re-read when its size or mtime changes.
CREATE TABLE containers (
    source_id INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    rel_path  TEXT NOT NULL,
    size      INTEGER NOT NULL,
    mtime     REAL NOT NULL,
    name      TEXT NOT NULL,
    author    TEXT NOT NULL DEFAULT '',
    version   TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (source_id, rel_path)
);

INSERT OR IGNORE INTO fields (asset_type, key, label, kind, options, position) VALUES
    ('prop', 'pack', 'Pack', 'text', '[]', 0),
    ('tile', 'pack', 'Pack', 'text', '[]', 0),
    ('texture', 'kind', 'Kind', 'choice', '["terrain","pattern","material"]', 0),
    ('texture', 'pack', 'Pack', 'text', '[]', 1),
    ('other', 'kind', 'Kind', 'choice', '["wall","path","portal","roof","light"]', 0),
    ('other', 'pack', 'Pack', 'text', '[]', 1);
"#;

/// Migration `i` takes the database from `user_version` i to i + 1.
const MIGRATIONS: &[&str] = &[SCHEMA_V1, PROPERTIES_V2, PACKS_V3];

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
        // Safe with WAL (no corruption on power loss, at most the last commits are lost) and
        // much faster for the scanner's many small transactions.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
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

    /// Replace the whole database with the one at `src`, then migrate it.
    pub fn restore_from(&self, src: &Path) -> rusqlite::Result<()> {
        let source = Connection::open(src)?;
        let mut conn = self.lock();
        rusqlite::backup::Backup::new(&source, &mut conn)?.run_to_completion(256, std::time::Duration::ZERO, None)?;
        migrate(&mut conn)
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

/// Ids per `IN (...)` list; SQLite allows at most 32,766 variables per statement.
pub const CHUNK: usize = 900;

/// Rebuild FTS rows for the given assets, or for all assets when `asset_ids` is `None`.
pub fn reindex_fts(conn: &Connection, asset_ids: Option<&[i64]>) -> rusqlite::Result<()> {
    let insert = |filter: &str, ids: &[i64]| {
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
        )
    };
    match asset_ids {
        None => {
            conn.execute("DELETE FROM assets_fts", [])?;
            insert("", &[])?;
        }
        Some(ids) => {
            for chunk in ids.chunks(CHUNK) {
                let m = marks(chunk.len());
                conn.execute(&format!("DELETE FROM assets_fts WHERE rowid IN ({m})"), params_from_iter(chunk))?;
                insert(&format!("WHERE a.id IN ({m})"), chunk)?;
            }
        }
    }
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

    /// A scan can touch more assets than SQLite allows variables in one statement.
    #[test]
    fn reindexes_more_ids_than_sqlite_variables() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("library.db")).unwrap();
        let n = 40_000;
        db.tx(|c| {
            c.execute("INSERT INTO sources (path, name) VALUES ('/x', 'x')", [])?;
            let mut stmt = c.prepare(
                "INSERT INTO assets (source_id, rel_path, file_hash, size, mtime, name) VALUES (1, ?, 'h', 1, 0, ?)",
            )?;
            for i in 0..n {
                stmt.execute(params![format!("f{i}.png"), format!("goblin {i}")])?;
            }
            let ids: Vec<i64> = (1..=n).collect();
            reindex_fts(c, Some(&ids))?;
            let found: i64 = c.query_row("SELECT count(*) FROM assets_fts WHERE assets_fts MATCH 'goblin'", [], |r| r.get(0))?;
            assert_eq!(found, n);
            Ok::<_, rusqlite::Error>(())
        })
        .unwrap();
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
