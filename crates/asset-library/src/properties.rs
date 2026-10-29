//! Properties: per-type fields with one value per asset (see PLAN.md, M8).
//!
//! Tags say what the subject is; properties say how the asset is made or used (a token's
//! view and size, a map's grid). Field definitions are user-editable.

use std::collections::HashMap;

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::db::{marks, normalize_tag, reindex_fts};

/// Query words a field key can't shadow.
pub const RESERVED_KEYS: &[&str] = &["tag", "type", "source", "color", "similar", "is", "has", "w", "h"];

#[derive(Clone, Debug, Serialize)]
pub struct Field {
    pub id: i64,
    pub asset_type: String,
    pub key: String,
    pub label: String,
    pub kind: String,
    pub options: Vec<String>,
    pub position: i64,
}

#[derive(Debug)]
pub struct Invalid(pub String);

impl From<rusqlite::Error> for Invalid {
    fn from(e: rusqlite::Error) -> Self {
        Invalid(e.to_string())
    }
}

type Result<T> = std::result::Result<T, Invalid>;

fn invalid<T>(msg: impl Into<String>) -> Result<T> {
    Err(Invalid(msg.into()))
}

fn row_field(r: &rusqlite::Row) -> rusqlite::Result<Field> {
    let options: String = r.get(5)?;
    Ok(Field {
        id: r.get(0)?,
        asset_type: r.get(1)?,
        key: r.get(2)?,
        label: r.get(3)?,
        kind: r.get(4)?,
        options: serde_json::from_str(&options).unwrap_or_default(),
        position: r.get(6)?,
    })
}

const FIELD_COLS: &str = "id, asset_type, key, label, kind, options, position";

pub fn list_fields(c: &Connection) -> rusqlite::Result<Vec<Field>> {
    let mut stmt = c.prepare(&format!("SELECT {FIELD_COLS} FROM fields ORDER BY asset_type, position, id"))?;
    let rows = stmt.query_map([], row_field)?.collect();
    rows
}

pub fn get_field(c: &Connection, id: i64) -> Result<Field> {
    c.query_row(&format!("SELECT {FIELD_COLS} FROM fields WHERE id = ?"), params![id], row_field)
        .optional()?
        .ok_or_else(|| Invalid("Property not found".into()))
}

/// Lowercase letters, digits, `-` and `_`, like a tag.
pub fn normalize_key(raw: &str) -> String {
    normalize_tag(raw).chars().filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_').collect()
}

fn check_key(c: &Connection, asset_type: &str, key: &str, except: Option<i64>) -> Result<()> {
    if key.is_empty() {
        return invalid("A property needs a key (letters, digits, - or _)");
    }
    if RESERVED_KEYS.contains(&key) {
        return invalid(format!("\"{key}\" is a search keyword; pick another key"));
    }
    let clash: Option<i64> = c
        .query_row("SELECT id FROM fields WHERE asset_type = ? AND key = ?", params![asset_type, key], |r| r.get(0))
        .optional()?;
    if clash.is_some_and(|id| Some(id) != except) {
        return invalid(format!("{asset_type} already has a \"{key}\" property"));
    }
    Ok(())
}

/// Trimmed, non-empty, without case-insensitive duplicates.
fn clean_options(options: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for o in options.iter().map(|o| o.trim()).filter(|o| !o.is_empty()) {
        if !out.iter().any(|x| x.eq_ignore_ascii_case(o)) {
            out.push(o.to_string());
        }
    }
    out
}

#[derive(Deserialize)]
pub struct NewField {
    pub asset_type: String,
    pub key: String,
    pub label: Option<String>,
    pub kind: String,
    #[serde(default)]
    pub options: Vec<String>,
}

pub fn create_field(c: &Connection, f: NewField) -> Result<Field> {
    let key = normalize_key(&f.key);
    check_key(c, &f.asset_type, &key, None)?;
    if !["choice", "number", "text"].contains(&f.kind.as_str()) {
        return invalid("Kind must be choice, number or text");
    }
    let options = if f.kind == "choice" { clean_options(&f.options) } else { Vec::new() };
    let label = f.label.map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).unwrap_or_else(|| key.clone());
    let position: i64 = c.query_row(
        "SELECT coalesce(max(position) + 1, 0) FROM fields WHERE asset_type = ?",
        params![f.asset_type],
        |r| r.get(0),
    )?;
    c.execute(
        "INSERT INTO fields (asset_type, key, label, kind, options, position) VALUES (?,?,?,?,?,?)",
        params![f.asset_type, key, label, f.kind, serde_json::to_string(&options).unwrap_or_default(), position],
    )?;
    get_field(c, c.last_insert_rowid())
}

#[derive(Deserialize)]
pub struct Rename {
    pub from: String,
    pub to: String,
}

#[derive(Deserialize, Default)]
pub struct FieldPatch {
    pub label: Option<String>,
    pub key: Option<String>,
    pub position: Option<i64>,
    pub add_option: Option<String>,
    /// Renames the option (choice) or value (text) on every asset; merges into an existing one.
    pub rename_value: Option<Rename>,
    /// Removes a choice option and clears it from every asset.
    pub remove_option: Option<String>,
}

pub fn update_field(c: &Connection, id: i64, p: FieldPatch) -> Result<Field> {
    let mut f = get_field(c, id)?;
    if let Some(label) = p.label.map(|l| l.trim().to_string()).filter(|l| !l.is_empty()) {
        c.execute("UPDATE fields SET label = ? WHERE id = ?", params![label, id])?;
    }
    if let Some(key) = p.key {
        let key = normalize_key(&key);
        check_key(c, &f.asset_type, &key, Some(id))?;
        c.execute("UPDATE fields SET key = ? WHERE id = ?", params![key, id])?;
    }
    if let Some(pos) = p.position {
        // Move within the type's order, shifting the others.
        let mut ids: Vec<i64> = {
            let mut stmt = c.prepare("SELECT id FROM fields WHERE asset_type = ? ORDER BY position, id")?;
            let ids = stmt.query_map(params![f.asset_type], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            ids
        };
        ids.retain(|&x| x != id);
        ids.insert((pos.max(0) as usize).min(ids.len()), id);
        for (n, fid) in ids.iter().enumerate() {
            c.execute("UPDATE fields SET position = ? WHERE id = ?", params![n as i64, fid])?;
        }
    }
    if let Some(opt) = p.add_option {
        if f.kind != "choice" {
            return invalid("Only choice properties have options");
        }
        f.options = clean_options(&[f.options, vec![opt]].concat());
        save_options(c, id, &f.options)?;
    }
    if let Some(Rename { from, to }) = p.rename_value {
        let to = to.trim().to_string();
        if to.is_empty() {
            return invalid("The new value can't be empty");
        }
        if f.kind == "choice" {
            let renamed: Vec<String> =
                f.options.iter().map(|o| if o.eq_ignore_ascii_case(&from) { to.clone() } else { o.clone() }).collect();
            f.options = clean_options(&renamed);
            save_options(c, id, &f.options)?;
            // Merging into an existing option keeps that option's spelling.
            let canonical = f.options.iter().find(|o| o.eq_ignore_ascii_case(&to)).cloned().unwrap_or(to);
            c.execute(
                "UPDATE asset_fields SET value = ? WHERE field_id = ? AND value = ? COLLATE NOCASE",
                params![canonical, id, from],
            )?;
        } else if f.kind == "text" {
            c.execute("UPDATE asset_fields SET value = ? WHERE field_id = ? AND value = ?", params![to, id, from])?;
        } else {
            return invalid("Number values can't be renamed");
        }
    }
    if let Some(opt) = p.remove_option {
        f.options.retain(|o| !o.eq_ignore_ascii_case(&opt));
        save_options(c, id, &f.options)?;
        c.execute("DELETE FROM asset_fields WHERE field_id = ? AND value = ? COLLATE NOCASE", params![id, opt])?;
    }
    get_field(c, id)
}

fn save_options(c: &Connection, id: i64, options: &[String]) -> rusqlite::Result<usize> {
    c.execute("UPDATE fields SET options = ? WHERE id = ?", params![serde_json::to_string(options).unwrap_or_default(), id])
}

/// Deletes the field and every value of it.
pub fn delete_field(c: &Connection, id: i64) -> rusqlite::Result<()> {
    c.execute("DELETE FROM fields WHERE id = ?", params![id])?;
    Ok(())
}

/// The stored form of a JSON value for `field`: `None` clears it.
pub fn coerce(field: &Field, raw: &Value) -> Result<Option<SqlValue>> {
    let text = match raw {
        Value::Null => return Ok(None),
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return invalid(format!("{}: not a value", field.label)),
    };
    if text.is_empty() {
        return Ok(None);
    }
    match field.kind.as_str() {
        "number" => match text.parse::<f64>() {
            Ok(n) if n.is_finite() => Ok(Some(SqlValue::Real(n))),
            _ => invalid(format!("{}: \"{text}\" isn't a number", field.label)),
        },
        "choice" => match field.options.iter().find(|o| o.eq_ignore_ascii_case(&text)) {
            Some(o) => Ok(Some(SqlValue::Text(o.clone()))),
            None => invalid(format!("{}: \"{text}\" isn't one of {}", field.label, field.options.join(", "))),
        },
        _ => Ok(Some(SqlValue::Text(text))),
    }
}

fn to_json(v: SqlValue) -> Value {
    match v {
        SqlValue::Real(n) if n.fract() == 0.0 && n.abs() < 1e15 => Value::from(n as i64),
        SqlValue::Real(n) => Value::from(n),
        SqlValue::Integer(n) => Value::from(n),
        SqlValue::Text(s) => Value::from(s),
        _ => Value::Null,
    }
}

/// Fields by asset type, then key.
fn fields_by_type(c: &Connection) -> rusqlite::Result<HashMap<String, HashMap<String, Field>>> {
    let mut out: HashMap<String, HashMap<String, Field>> = HashMap::new();
    for f in list_fields(c)? {
        out.entry(f.asset_type.clone()).or_default().insert(f.key.to_lowercase(), f);
    }
    Ok(out)
}

/// Set (or clear, with null) property values by key on each asset whose type has that key.
/// With `strict`, a key the asset's type lacks is an error; otherwise it's skipped.
/// Returns how many values were written.
pub fn set_values(c: &Connection, ids: &[i64], values: &Map<String, Value>, strict: bool) -> Result<usize> {
    let fields = fields_by_type(c)?;
    let mut written = 0;
    for &id in ids {
        let Some(asset_type): Option<String> =
            c.query_row("SELECT asset_type FROM assets WHERE id = ?", params![id], |r| r.get(0)).optional()?
        else {
            continue;
        };
        for (key, raw) in values {
            let Some(field) = fields.get(&asset_type).and_then(|m| m.get(&key.to_lowercase())) else {
                if strict {
                    return invalid(format!("{asset_type} has no \"{key}\" property"));
                }
                continue;
            };
            match coerce(field, raw)? {
                Some(v) => {
                    c.execute(
                        "INSERT INTO asset_fields (asset_id, field_id, value) VALUES (?, ?, ?)
                         ON CONFLICT(asset_id, field_id) DO UPDATE SET value = excluded.value",
                        params![id, field.id, v],
                    )?;
                }
                None => {
                    c.execute("DELETE FROM asset_fields WHERE asset_id = ? AND field_id = ?", params![id, field.id])?;
                }
            }
            written += 1;
        }
    }
    Ok(written)
}

/// `{key: value}` per asset, in field order.
pub fn values_for(c: &Connection, ids: &[i64]) -> rusqlite::Result<HashMap<i64, Map<String, Value>>> {
    let mut out: HashMap<i64, Map<String, Value>> = ids.iter().map(|&i| (i, Map::new())).collect();
    for chunk in ids.chunks(900) {
        let mut stmt = c.prepare(&format!(
            "SELECT af.asset_id, f.key, af.value FROM asset_fields af JOIN fields f ON f.id = af.field_id
             WHERE af.asset_id IN ({}) ORDER BY f.position, f.id",
            marks(chunk.len())
        ))?;
        let rows = stmt.query_map(params_from_iter(chunk), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, SqlValue>(2)?)))?;
        for row in rows {
            let (id, key, v) = row?;
            out.entry(id).or_default().insert(key, to_json(v));
        }
    }
    Ok(out)
}

/// After a type change: move values to the same-key field of the new type, drop the rest.
pub fn retype(c: &Connection, ids: &[i64], new_type: &str) -> rusqlite::Result<()> {
    let fields = fields_by_type(c)?;
    let target = fields.get(new_type);
    let key_of: HashMap<i64, String> = list_fields(c)?.into_iter().map(|f| (f.id, f.key.to_lowercase())).collect();
    for &id in ids {
        let held: Vec<i64> = {
            let mut stmt = c.prepare("SELECT field_id FROM asset_fields WHERE asset_id = ?")?;
            let held = stmt.query_map(params![id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            held
        };
        for fid in held {
            let moved_to = key_of.get(&fid).and_then(|k| target?.get(k)).map(|f| f.id);
            match moved_to {
                Some(new_id) if new_id == fid => {}
                Some(new_id) => {
                    c.execute(
                        "UPDATE OR REPLACE asset_fields SET field_id = ? WHERE asset_id = ? AND field_id = ?",
                        params![new_id, id, fid],
                    )?;
                }
                None => {
                    c.execute("DELETE FROM asset_fields WHERE asset_id = ? AND field_id = ?", params![id, fid])?;
                }
            }
        }
    }
    Ok(())
}

#[derive(Serialize)]
pub struct Facet {
    pub key: String,
    pub label: String,
    pub kind: String,
    pub values: Vec<FacetValue>,
}

#[derive(Serialize)]
pub struct FacetValue {
    pub value: Value,
    pub count: i64,
}

/// For the sidebar: each property key (merged across types, first-defined label) with its
/// values and counts over assets that aren't missing. Choice values follow option order.
pub fn facets(c: &Connection) -> rusqlite::Result<Vec<Facet>> {
    let mut out: Vec<Facet> = Vec::new();
    let mut options: HashMap<String, Vec<String>> = HashMap::new();
    let mut fields = list_fields(c)?;
    fields.sort_by_key(|f| (f.position, f.id));
    for f in fields {
        let k = f.key.to_lowercase();
        let opts = options.entry(k.clone()).or_default();
        for o in &f.options {
            if !opts.iter().any(|x| x.eq_ignore_ascii_case(o)) {
                opts.push(o.clone());
            }
        }
        if !out.iter().any(|x| x.key.to_lowercase() == k) {
            out.push(Facet { key: f.key, label: f.label, kind: f.kind, values: Vec::new() });
        }
    }
    let mut stmt = c.prepare(
        "SELECT lower(f.key), af.value, count(*) FROM asset_fields af
         JOIN fields f ON f.id = af.field_id JOIN assets a ON a.id = af.asset_id AND a.missing = 0
         GROUP BY lower(f.key), af.value",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, SqlValue>(1)?, r.get::<_, i64>(2)?)))?;
    for row in rows {
        let (k, v, n) = row?;
        if let Some(f) = out.iter_mut().find(|f| f.key.to_lowercase() == k) {
            f.values.push(FacetValue { value: to_json(v), count: n });
        }
    }
    for f in &mut out {
        let opts = &options[&f.key.to_lowercase()];
        let rank = |v: &Value| v.as_str().and_then(|s| opts.iter().position(|o| o.eq_ignore_ascii_case(s))).unwrap_or(usize::MAX);
        f.values.sort_by(|a, b| {
            rank(&a.value).cmp(&rank(&b.value)).then_with(|| match (a.value.as_f64(), b.value.as_f64()) {
                (Some(x), Some(y)) => x.total_cmp(&y),
                _ => a.value.to_string().to_lowercase().cmp(&b.value.to_string().to_lowercase()),
            })
        });
    }
    Ok(out)
}

#[derive(Serialize)]
pub struct Promoted {
    pub moved: usize,
    pub skipped: usize,
}

/// Set `key = value` on every asset tagged `tag` whose type has that property, and remove
/// the tag from them. Assets whose type lacks the property keep the tag.
pub fn promote_tag(c: &Connection, tag: &str, key: &str, value: &Value) -> Result<Promoted> {
    let tag = normalize_tag(tag);
    let Some(tag_id): Option<i64> =
        c.query_row("SELECT id FROM tags WHERE name = ?", params![tag], |r| r.get(0)).optional()?
    else {
        return invalid("Tag not found");
    };
    let ids: Vec<i64> = {
        let mut stmt = c.prepare("SELECT asset_id FROM asset_tags WHERE tag_id = ?")?;
        let ids = stmt.query_map(params![tag_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        ids
    };
    let fields = fields_by_type(c)?;
    let key = key.to_lowercase();
    let (mut moved, mut skipped) = (Vec::new(), 0);
    for id in ids {
        let t: String = c.query_row("SELECT asset_type FROM assets WHERE id = ?", params![id], |r| r.get(0))?;
        match fields.get(&t).and_then(|m| m.get(&key)) {
            Some(field) => {
                let Some(v) = coerce(field, value)? else { return invalid("Choose a value") };
                c.execute(
                    "INSERT INTO asset_fields (asset_id, field_id, value) VALUES (?, ?, ?)
                     ON CONFLICT(asset_id, field_id) DO UPDATE SET value = excluded.value",
                    params![id, field.id, v],
                )?;
                c.execute("DELETE FROM asset_tags WHERE asset_id = ? AND tag_id = ?", params![id, tag_id])?;
                moved.push(id);
            }
            None => skipped += 1,
        }
    }
    if moved.is_empty() && skipped > 0 {
        return invalid(format!("None of the tagged assets has a \"{key}\" property"));
    }
    c.execute("DELETE FROM tags WHERE id = ? AND id NOT IN (SELECT tag_id FROM asset_tags)", params![tag_id])?;
    reindex_fts(c, Some(&moved))?;
    Ok(Promoted { moved: moved.len(), skipped })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    fn lib() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("library.db")).unwrap();
        db.tx(|c| {
            c.execute_batch(
                "INSERT INTO sources (path, name) VALUES ('/x', 'x');
                 INSERT INTO assets (source_id, rel_path, file_hash, size, mtime, name, asset_type) VALUES
                   (1, 'a.png', 'h1', 1, 0, 'a', 'token'), (1, 'b.png', 'h2', 1, 0, 'b', 'map');",
            )
        })
        .unwrap();
        (dir, db)
    }

    fn vals(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn coerces_and_sets_values() {
        let (_d, db) = lib();
        db.tx(|c| -> Result<()> {
            set_values(c, &[1], &vals(&[("size", "LARGE".into()), ("pack", " FA ".into())]), true)?;
            set_values(c, &[2], &vals(&[("grid", "70".into())]), true)?;
            assert!(set_values(c, &[1], &vals(&[("size", "enormous".into())]), true).is_err());
            assert!(set_values(c, &[1], &vals(&[("grid", 5.into())]), true).is_err());
            assert_eq!(set_values(c, &[1, 2], &vals(&[("size", "tiny".into())]), false)?, 1);
            let v = values_for(c, &[1, 2])?;
            assert_eq!(v[&1]["size"], "tiny");
            assert_eq!(v[&1]["pack"], "FA");
            assert_eq!(v[&2]["grid"], 70);
            set_values(c, &[1], &vals(&[("pack", Value::Null)]), true)?;
            assert!(!values_for(c, &[1])?[&1].contains_key("pack"));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn edits_definitions() {
        let (_d, db) = lib();
        db.tx(|c| -> Result<()> {
            assert!(create_field(c, NewField { asset_type: "token".into(), key: "tag".into(), label: None, kind: "text".into(), options: vec![] }).is_err());
            assert!(create_field(c, NewField { asset_type: "token".into(), key: "Size".into(), label: None, kind: "text".into(), options: vec![] }).is_err());
            let base = create_field(c, NewField { asset_type: "token".into(), key: "Base Shape".into(), label: Some("Base".into()), kind: "choice".into(), options: vec!["round".into(), " Round".into(), "square".into()] })?;
            assert_eq!((base.key.as_str(), base.options.len()), ("base-shape", 2));
            set_values(c, &[1], &vals(&[("base-shape", "square".into())]), true)?;
            update_field(c, base.id, FieldPatch { rename_value: Some(Rename { from: "square".into(), to: "Round".into() }), ..Default::default() })?;
            assert_eq!(values_for(c, &[1])?[&1]["base-shape"], "round");
            assert_eq!(get_field(c, base.id)?.options, ["round"]);
            update_field(c, base.id, FieldPatch { remove_option: Some("round".into()), ..Default::default() })?;
            assert!(!values_for(c, &[1])?[&1].contains_key("base-shape"));
            update_field(c, base.id, FieldPatch { position: Some(0), ..Default::default() })?;
            assert_eq!(list_fields(c)?.iter().find(|f| f.asset_type == "token").unwrap().id, base.id);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn retype_moves_shared_keys() {
        let (_d, db) = lib();
        db.tx(|c| -> Result<()> {
            set_values(c, &[1], &vals(&[("size", "large".into()), ("pack", "FA".into())]), true)?;
            c.execute("UPDATE assets SET asset_type = 'map' WHERE id = 1", [])?;
            retype(c, &[1], "map")?;
            let v = values_for(c, &[1])?;
            assert_eq!(v[&1].len(), 1);
            assert_eq!(v[&1]["pack"], "FA");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn promotes_tags_and_counts_facets() {
        let (_d, db) = lib();
        db.tx(|c| -> Result<()> {
            let tid = crate::db::get_or_create_tags(c, &["fa_tokens"])?[0];
            c.execute("INSERT INTO asset_tags VALUES (1, ?), (2, ?)", params![tid, tid])?;
            let r = promote_tag(c, "fa_tokens", "pack", &"Forgotten Adventures".into())?;
            assert_eq!((r.moved, r.skipped), (2, 0)); // token and map both have pack
            let gone: i64 = c.query_row("SELECT count(*) FROM tags", [], |r| r.get(0))?;
            assert_eq!(gone, 0);
            let f = facets(c)?;
            let pack = f.iter().find(|f| f.key == "pack").unwrap();
            assert_eq!(pack.values[0].count, 2);
            assert_eq!(f.iter().map(|f| f.key.as_str()).collect::<Vec<_>>(), ["view", "grid", "size", "variant", "pack"]);
            Ok(())
        })
        .unwrap();
    }
}
