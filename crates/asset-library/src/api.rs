//! The REST API under `/api`, matching the original's `app/main.py`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::config::{ASSET_TYPES, COLOR_HEX};
use crate::db::{CHUNK, Database, get_or_create_tags, marks, normalize_tag, reindex_fts};
use crate::desktop::{Desktop, drag_icon};
use crate::export;
use crate::properties::{self, FieldPatch, Invalid, NewField};
use crate::settings::Settings;
use crate::jobs::Jobs;
use crate::location::Location;
use crate::scanner::Scanner;
use crate::search;
use crate::similarity;

pub struct AppState {
    pub db: Arc<Database>,
    pub scanner: Arc<Scanner>,
    pub jobs: Arc<Jobs>,
    pub desktop: Option<Arc<dyn Desktop>>,
    pub data_dir: PathBuf,
    pub model_dir: PathBuf,
}

type St = State<Arc<AppState>>;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/meta", get(meta))
        .route("/api/sources", get(list_sources).post(add_source))
        .route("/api/sources/{sid}", patch(update_source).delete(delete_source))
        .route("/api/scan", post(start_scan))
        .route("/api/scan/status", get(scan_status))
        .route("/api/assets", get(list_assets))
        .route("/api/assets/bulk", post(bulk_update))
        .route("/api/assets/{id}", get(get_asset).patch(update_asset))
        .route("/api/assets/{id}/thumb", get(thumb))
        .route("/api/assets/{id}/file", get(original))
        .route("/api/assets/{id}/reveal", post(reveal))
        .route("/api/export/folder", post(export_folder))
        .route("/api/export/zip", post(export_zip))
        .route("/api/suggestions", post(suggestions))
        .route("/api/suggestions/reject", post(reject_suggestion))
        .route("/api/suggestions/ignored", get(ignored_suggestions))
        .route("/api/suggestions/ignored/{tag}", axum::routing::delete(unignore_suggestion))
        .route("/api/tags", get(list_tags))
        .route("/api/settings", get(get_settings).patch(update_settings))
        .route("/api/pick-folder", post(pick_folder))
        .route("/api/drag", post(start_drag))
        .route("/api/assets/{id}/open", post(open_file))
        .route("/api/export/zip-save", post(export_zip_save))
        .route("/api/import", post(import_library))
        .route("/api/tags/{name}", patch(rename_tag).delete(delete_tag))
        .route("/api/tags/{name}/promote", post(promote_tag))
        .route("/api/fields", get(list_fields).post(create_field))
        .route("/api/fields/{id}", patch(update_field).delete(delete_field))
        .route("/api/facets", get(facets))
        .fallback(get(static_file))
        .with_state(state)
}

/// The web UI, embedded at build time (from the original's `app/static`).
const STATIC: &[(&str, &str, &[u8])] = &[
    ("index.html", "text/html; charset=utf-8", include_bytes!("../static/index.html")),
    ("app.js", "text/javascript; charset=utf-8", include_bytes!("../static/app.js")),
    ("styles.css", "text/css; charset=utf-8", include_bytes!("../static/styles.css")),
];

async fn static_file(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match STATIC.iter().find(|(name, _, _)| *name == path) {
        Some((_, mime, body)) => ([(header::CONTENT_TYPE, *mime)], *body).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "detail": "Not Found" }))).into_response(),
    }
}

impl AppState {
    /// Run a database transaction from an async handler.
    fn tx<T>(&self, f: impl FnOnce(&Connection) -> Result<T, ApiError>) -> Result<T, ApiError> {
        tokio::task::block_in_place(|| self.db.tx(|c| f(c)))
    }

    /// Rescan every source; false if a scan is already running.
    pub fn rescan_all(&self) -> bool {
        let ids = self
            .db
            .tx(|c| {
                let mut stmt = c.prepare("SELECT id FROM sources")?;
                stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()
            })
            .unwrap_or_default();
        self.start_scan(ids)
    }

    fn start_scan(&self, ids: Vec<i64>) -> bool {
        let jobs = Arc::clone(&self.jobs);
        self.scanner.scan_in_background(ids, move || jobs.start())
    }
}

// ---------------------------------------------------------------- errors

/// An HTTP error with FastAPI's `{"detail": ...}` body.
pub struct ApiError(StatusCode, String);

fn err(status: StatusCode, detail: impl Into<String>) -> ApiError {
    ApiError(status, detail.into())
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "detail": self.1 }))).into_response()
    }
}

impl From<Invalid> for ApiError {
    fn from(e: Invalid) -> Self {
        let status = if e.0.ends_with("not found") { StatusCode::NOT_FOUND } else { StatusCode::BAD_REQUEST };
        err(status, e.0)
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

type ApiResult<T = Json<Value>> = Result<T, ApiError>;

fn ok() -> ApiResult {
    Ok(Json(json!({ "ok": true })))
}

/// Endpoints that touch the filesystem are refused for LAN clients (see `--lan`).
fn local_only(addr: SocketAddr) -> Result<(), ApiError> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(err(StatusCode::FORBIDDEN, "Only available on the machine running the library"))
    }
}

// ---------------------------------------------------------------- helpers

/// A row as a JSON object keyed by column name.
fn row_json(row: &Row) -> rusqlite::Result<Map<String, Value>> {
    let stmt = row.as_ref();
    let mut out = Map::new();
    for i in 0..stmt.column_count() {
        let v = match row.get_ref(i)? {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(n) => n.into(),
            ValueRef::Real(f) => f.into(),
            ValueRef::Text(t) => String::from_utf8_lossy(t).into(),
            ValueRef::Blob(_) => Value::Null,
        };
        out.insert(stmt.column_name(i)?.to_string(), v);
    }
    Ok(out)
}

fn to_bool(m: &mut Map<String, Value>, key: &str) {
    if let Some(v) = m.get_mut(key) {
        *v = Value::Bool(v.as_i64().is_some_and(|n| n != 0));
    }
}

fn asset_row(c: &Connection, id: i64) -> ApiResult<Map<String, Value>> {
    c.query_row(
        "SELECT a.*, s.path AS source_path, s.name AS source_name
         FROM assets a JOIN sources s ON s.id = a.source_id WHERE a.id = ?",
        params![id],
        row_json,
    )
    .optional()?
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "Asset not found"))
}

/// Where an asset row's bytes are: a file, or an entry in a Dungeondraft pack.
fn location(row: &Map<String, Value>) -> Location {
    let s = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or_default();
    let n = |k: &str| row.get(k).and_then(Value::as_i64);
    let container = row.get("container").and_then(Value::as_str);
    Location::new(s("source_path"), s("rel_path"), container, n("entry_offset"), n("entry_size"))
}

/// The path an export writes under the source name: the rel path, with a pack's
/// `.dungeondraft_pack` folder named after the pack file without its extension.
fn export_rel(row: &Map<String, Value>) -> String {
    let rel = row.get("rel_path").and_then(Value::as_str).unwrap_or_default();
    match row.get("container").and_then(Value::as_str) {
        Some(c) if rel.starts_with(c) => {
            let stem = c.len() - FsPath::new(c).extension().map_or(0, |e| e.len() + 1);
            format!("{}{}", &c[..stem], &rel[c.len()..])
        }
        _ => rel.to_string(),
    }
}

fn extracted_dir(s: &AppState) -> PathBuf {
    s.data_dir.join("extracted")
}

fn tags_for(c: &Connection, ids: &[i64]) -> rusqlite::Result<HashMap<i64, Vec<String>>> {
    let mut out: HashMap<i64, Vec<String>> = ids.iter().map(|&i| (i, Vec::new())).collect();
    if ids.is_empty() {
        return Ok(out);
    }
    let mut stmt = c.prepare(&format!(
        "SELECT x.asset_id, t.name FROM asset_tags x JOIN tags t ON t.id = x.tag_id
         WHERE x.asset_id IN ({}) ORDER BY t.name",
        marks(ids.len())
    ))?;
    let rows = stmt.query_map(params_from_iter(ids), |r| Ok((r.get(0)?, r.get(1)?)))?;
    for row in rows {
        let (id, name): (i64, String) = row?;
        out.entry(id).or_default().push(name);
    }
    Ok(out)
}

/// Expand a leading `~` to the home directory, like Python's `Path.expanduser`.
fn expand_user(p: &str) -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    match (p.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            PathBuf::from(home).join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(p),
    }
}

// ---------------------------------------------------------------- meta

async fn meta(State(s): St) -> Json<Value> {
    let colors: Map<String, Value> =
        COLOR_HEX.iter().map(|(k, v)| (k.to_string(), (*v).into())).collect();
    Json(json!({
        "asset_types": ASSET_TYPES,
        "sorts": search::SORTS.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        "colors": colors,
        "ai": s.jobs.analysis.ai_available(),
        "desktop": s.desktop.is_some(),
    }))
}

// ---------------------------------------------------------------- sources

#[derive(Deserialize)]
struct SourceIn {
    path: String,
    name: Option<String>,
    #[serde(default = "default_type")]
    asset_type: String,
    #[serde(default = "yes")]
    folder_tags: bool,
}

fn default_type() -> String {
    "token".into()
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct SourcePatch {
    name: Option<String>,
    asset_type: Option<String>,
    folder_tags: Option<bool>,
}

async fn list_sources(State(s): St) -> ApiResult {
    let rows = s.tx(|c| {
        let mut stmt = c.prepare(
            "SELECT s.*, (SELECT count(*) FROM assets a
                          WHERE a.source_id = s.id AND a.missing = 0) AS count
             FROM sources s ORDER BY s.name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let mut m = row_json(r)?;
                to_bool(&mut m, "folder_tags");
                Ok(Value::Object(m))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    Ok(Json(rows.into()))
}

async fn add_source(
    State(s): St,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<SourceIn>,
) -> ApiResult {
    local_only(addr)?;
    let raw = body.path.trim();
    let p = expand_user(raw);
    if raw.is_empty() || !p.is_absolute() || !p.is_dir() {
        return Err(err(StatusCode::BAD_REQUEST, format!("Not a folder: {}", p.display())));
    }
    let p = dunce::canonicalize(&p)
        .map_err(|e| err(StatusCode::BAD_REQUEST, format!("Not a folder: {}: {e}", p.display())))?;
    let path = p.to_string_lossy().into_owned();
    let name = body
        .name
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let sid = s.tx(|c| {
        if c.query_row("SELECT 1 FROM sources WHERE path = ?", params![path], |_| Ok(()))
            .optional()?
            .is_some()
        {
            return Err(err(StatusCode::CONFLICT, "That folder is already a source"));
        }
        c.execute(
            "INSERT INTO sources (path, name, asset_type, folder_tags) VALUES (?,?,?,?)",
            params![path, name, body.asset_type, body.folder_tags],
        )?;
        Ok(c.last_insert_rowid())
    })?;
    s.start_scan(vec![sid]);
    Ok(Json(json!({ "id": sid })))
}

async fn update_source(State(s): St, Path(sid): Path<i64>, Json(body): Json<SourcePatch>) -> ApiResult {
    let mut sets: Vec<&str> = Vec::new();
    let mut vals: Vec<SqlValue> = Vec::new();
    if let Some(v) = body.name {
        sets.push("name = ?");
        vals.push(v.into());
    }
    if let Some(v) = body.asset_type {
        sets.push("asset_type = ?");
        vals.push(v.into());
    }
    if let Some(v) = body.folder_tags {
        sets.push("folder_tags = ?");
        vals.push((v as i64).into());
    }
    if !sets.is_empty() {
        vals.push(sid.into());
        s.tx(|c| {
            c.execute(&format!("UPDATE sources SET {} WHERE id = ?", sets.join(", ")), params_from_iter(vals))?;
            Ok(())
        })?;
    }
    ok()
}

/// Removes the source from the index only. Files on disk are untouched.
async fn delete_source(State(s): St, Path(sid): Path<i64>) -> ApiResult {
    s.tx(|c| {
        c.execute(
            "DELETE FROM assets_fts WHERE rowid IN (SELECT id FROM assets WHERE source_id = ?)",
            params![sid],
        )?;
        c.execute("DELETE FROM sources WHERE id = ?", params![sid])?;
        Ok(())
    })?;
    ok()
}

// ---------------------------------------------------------------- scanning

#[derive(Deserialize)]
struct ScanIn {
    #[serde(default)]
    source_ids: Option<Vec<i64>>,
}

async fn start_scan(State(s): St, Json(body): Json<ScanIn>) -> ApiResult {
    let ids = match body.source_ids {
        Some(ids) if !ids.is_empty() => ids,
        _ => s.tx(|c| {
            let mut stmt = c.prepare("SELECT id FROM sources")?;
            let ids = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
            Ok(ids)
        })?,
    };
    if !s.start_scan(ids) {
        return Err(err(StatusCode::CONFLICT, "A scan is already running"));
    }
    ok()
}

async fn scan_status(State(s): St) -> Json<Value> {
    let mut v = serde_json::to_value(s.scanner.status().clone()).unwrap_or_default();
    v["thumbs"] = serde_json::to_value(s.jobs.thumbs.status().clone()).unwrap_or_default();
    v["analysis"] = serde_json::to_value(s.jobs.analysis.status().clone()).unwrap_or_default();
    v["paused"] = s.jobs.paused().into(); // waiting for mains power; not in the original
    Json(v)
}

// ---------------------------------------------------------------- assets

#[derive(Deserialize)]
struct ListParams {
    #[serde(default)]
    q: String,
    #[serde(default)]
    sort: String,
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

fn default_limit() -> i64 {
    200
}

#[derive(Serialize)]
struct ListItem {
    id: i64,
    name: String,
    rel_path: String,
    asset_type: String,
    width: Option<i64>,
    height: Option<i64>,
    favorite: bool,
    has_alpha: bool,
    missing: bool,
    file_hash: String,
    tags: Vec<String>,
    fields: Map<String, Value>,
}

async fn list_assets(State(s): St, Query(p): Query<ListParams>) -> ApiResult {
    let keys = s.tx(|c| Ok(search::FieldKeys::new(&properties::list_fields(c)?)))?;
    let parsed = search::parse_with(&p.q, &keys);
    let (wh, mut params) = search::build_sql(&parsed, false);
    let mut order = search::order_by(&p.sort);
    let mut source = "assets a";
    if parsed.ranked() {
        // Image search: rank in Rust, then filter/paginate in SQL in rank order.
        let analysis = &s.jobs.analysis;
        if !analysis.ai_available() {
            return Err(err(StatusCode::BAD_REQUEST, "Image search needs the AI extra (see README)"));
        }
        let ranked = tokio::task::block_in_place(|| -> anyhow::Result<_> {
            let idx = analysis.index.get(&s.db)?;
            Ok(if parsed.semantic.is_empty() {
                similarity::rank_similar(&idx, parsed.similar.unwrap_or_default(), similarity::RANKED_LIMIT)
            } else {
                let vec = analysis.clip.embed_text(&parsed.semantic.join(" "))?;
                similarity::rank_by_vector(&idx, &vec, similarity::RANKED_LIMIT, None)
            })
        })
        .map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, format!("Image model unavailable: {e}")))?;
        let ids: Vec<i64> = ranked.iter().map(|r| r.0).collect();
        params.insert(0, serde_json::to_string(&ids).unwrap_or_default().into());
        source = "assets a JOIN json_each(?) r ON r.value = a.id";
        order = "r.key";
    }
    let limit = p.limit.clamp(1, 1000);
    let (total, items) = s.tx(|c| {
        let total: i64 = c.query_row(
            &format!("SELECT count(*) FROM {source} WHERE {wh}"),
            params_from_iter(&params),
            |r| r.get(0),
        )?;
        let mut stmt = c.prepare(&format!(
            "SELECT a.id, a.name, a.rel_path, a.asset_type, a.width, a.height, a.favorite,
                    a.has_alpha, a.missing, a.file_hash
             FROM {source} WHERE {wh} ORDER BY {order} LIMIT ? OFFSET ?"
        ))?;
        let all_params = params.iter().cloned().chain([limit.into(), p.offset.into()]);
        let mut items = stmt
            .query_map(params_from_iter(all_params), |r| {
                Ok(ListItem {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    rel_path: r.get(2)?,
                    asset_type: r.get(3)?,
                    width: r.get(4)?,
                    height: r.get(5)?,
                    favorite: r.get(6)?,
                    has_alpha: r.get(7)?,
                    missing: r.get(8)?,
                    file_hash: r.get(9)?,
                    tags: Vec::new(),
                    fields: Map::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let ids: Vec<i64> = items.iter().map(|i| i.id).collect();
        let mut tags = tags_for(c, &ids)?;
        let mut fields = properties::values_for(c, &ids)?;
        for item in &mut items {
            item.tags = tags.remove(&item.id).unwrap_or_default();
            item.fields = fields.remove(&item.id).unwrap_or_default();
        }
        Ok((total, items))
    })?;
    Ok(Json(json!({ "total": total, "items": items })))
}

fn asset_json(c: &Connection, id: i64) -> ApiResult {
    let mut d = asset_row(c, id)?;
    let loc = location(&d);
    let extra = d.get("extra").and_then(Value::as_str).unwrap_or("{}");
    d["extra"] = serde_json::from_str(if extra.is_empty() { "{}" } else { extra }).unwrap_or_default();
    for key in ["favorite", "has_alpha", "missing"] {
        to_bool(&mut d, key);
    }
    d.insert("tags".into(), tags_for(c, &[id])?.remove(&id).unwrap_or_default().into());
    d.insert("full_path".into(), loc.display().into());
    let hash = d["file_hash"].as_str().unwrap_or_default().to_string();
    let f: Option<(Option<String>, bool)> = c
        .query_row(
            "SELECT colors, embedding IS NOT NULL FROM image_features WHERE file_hash = ?",
            params![hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let colors: Map<String, Value> = f
        .as_ref()
        .and_then(|(c, _)| c.as_deref())
        .and_then(|c| serde_json::from_str(c).ok())
        .unwrap_or_default();
    let mut colors: Vec<(String, Value)> = colors.into_iter().collect();
    let frac = |v: &Value| v.as_f64().unwrap_or(0.0);
    colors.sort_by(|a, b| frac(&b.1).total_cmp(&frac(&a.1)));
    d.insert("colors".into(), Value::Object(colors.into_iter().collect()));
    d.insert("has_embedding".into(), f.is_some_and(|(_, e)| e).into());
    d.insert("fields".into(), Value::Object(properties::values_for(c, &[id])?.remove(&id).unwrap_or_default()));
    Ok(Json(Value::Object(d)))
}

async fn get_asset(State(s): St, Path(id): Path<i64>) -> ApiResult {
    s.tx(|c| asset_json(c, id))
}

#[derive(Deserialize)]
struct AssetPatch {
    name: Option<String>,
    notes: Option<String>,
    favorite: Option<bool>,
    asset_type: Option<String>,
    /// Replaces the full tag set.
    tags: Option<Vec<String>>,
    extra: Option<Map<String, Value>>,
    /// Property values by key; null clears one.
    fields: Option<Map<String, Value>>,
}

async fn update_asset(State(s): St, Path(id): Path<i64>, Json(body): Json<AssetPatch>) -> ApiResult {
    s.tx(|c| {
        asset_row(c, id)?;
        let mut sets: Vec<&str> = Vec::new();
        let mut vals: Vec<SqlValue> = Vec::new();
        let retype_to = body.asset_type.clone();
        for (col, v) in [("name", body.name), ("notes", body.notes), ("asset_type", body.asset_type)] {
            if let Some(v) = v {
                sets.push(col);
                vals.push(v.into());
            }
        }
        if let Some(v) = body.favorite {
            sets.push("favorite");
            vals.push((v as i64).into());
        }
        if let Some(v) = body.extra {
            sets.push("extra");
            vals.push(Value::Object(v).to_string().into());
        }
        if !sets.is_empty() {
            let sets: Vec<String> = sets.iter().map(|k| format!("{k} = ?")).collect();
            vals.push(id.into());
            c.execute(&format!("UPDATE assets SET {} WHERE id = ?", sets.join(", ")), params_from_iter(vals))?;
        }
        if let Some(t) = retype_to {
            properties::retype(c, &[id], &t)?;
        }
        if let Some(fields) = &body.fields {
            properties::set_values(c, &[id], fields, true)?;
        }
        if let Some(tags) = &body.tags {
            c.execute("DELETE FROM asset_tags WHERE asset_id = ?", params![id])?;
            for tid in get_or_create_tags(c, tags)? {
                c.execute("INSERT OR IGNORE INTO asset_tags VALUES (?, ?)", params![id, tid])?;
            }
        }
        reindex_fts(c, Some(&[id]))?;
        asset_json(c, id)
    })
}

#[derive(Deserialize)]
struct BulkIn {
    ids: Vec<i64>,
    #[serde(default)]
    add_tags: Vec<String>,
    #[serde(default)]
    remove_tags: Vec<String>,
    asset_type: Option<String>,
    favorite: Option<bool>,
    /// Property values by key, set on each asset whose type has the key; null clears.
    #[serde(default)]
    set_fields: Map<String, Value>,
}

async fn bulk_update(State(s): St, Json(body): Json<BulkIn>) -> ApiResult {
    let ids = &body.ids;
    if ids.is_empty() {
        return ok();
    }
    s.tx(|c| -> ApiResult<()> {
        for tid in get_or_create_tags(c, &body.add_tags)? {
            let mut stmt = c.prepare("INSERT OR IGNORE INTO asset_tags VALUES (?, ?)")?;
            for id in ids {
                stmt.execute(params![id, tid])?;
            }
        }
        for chunk in ids.chunks(CHUNK) {
            let m = marks(chunk.len());
            let with_ids = |first: SqlValue| std::iter::once(first).chain(chunk.iter().map(|&i| i.into()));
            for name in &body.remove_tags {
                let params = chunk.iter().map(|&i| SqlValue::from(i)).chain([normalize_tag(name).into()]);
                c.execute(
                    &format!(
                        "DELETE FROM asset_tags WHERE asset_id IN ({m})
                         AND tag_id = (SELECT id FROM tags WHERE name = ?)"
                    ),
                    params_from_iter(params),
                )?;
            }
            if let Some(t) = body.asset_type.as_ref().filter(|t| !t.is_empty()) {
                c.execute(
                    &format!("UPDATE assets SET asset_type = ? WHERE id IN ({m})"),
                    params_from_iter(with_ids(t.clone().into())),
                )?;
            }
            if let Some(f) = body.favorite {
                c.execute(
                    &format!("UPDATE assets SET favorite = ? WHERE id IN ({m})"),
                    params_from_iter(with_ids((f as i64).into())),
                )?;
            }
        }
        if let Some(t) = body.asset_type.as_ref().filter(|t| !t.is_empty()) {
            properties::retype(c, ids, t)?;
        }
        if !body.set_fields.is_empty() {
            properties::set_values(c, ids, &body.set_fields, false)?;
        }
        c.execute("DELETE FROM tags WHERE id NOT IN (SELECT tag_id FROM asset_tags)", [])?;
        reindex_fts(c, Some(ids))?;
        Ok(())
    })?;
    ok()
}

// ---------------------------------------------------------------- suggestions

#[derive(Deserialize)]
struct SuggestIn {
    ids: Vec<i64>,
}

#[derive(Serialize)]
struct Merged {
    /// The tag, or `key=value` for a property; what /api/suggestions/reject takes.
    tag: String,
    ids: Vec<i64>,
    count: usize,
    score: f64,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<Value>,
}

/// Merge per-asset suggestions by identity, in first-seen order: averaged score, the ids each
/// applies to, and all sources. Single asset: best first; several: most widely applicable.
fn merge_suggestions(items: impl Iterator<Item = (i64, Merged)>) -> Vec<Merged> {
    let mut merged: Vec<(Merged, std::collections::BTreeSet<String>)> = Vec::new();
    for (aid, sg) in items {
        let pos = match merged.iter().position(|m| m.0.tag == sg.tag) {
            Some(p) => p,
            None => {
                merged.push((Merged { ids: Vec::new(), score: 0.0, source: String::new(), count: 0, ..sg }, Default::default()));
                merged.len() - 1
            }
        };
        let m = &mut merged[pos];
        m.0.ids.push(aid);
        m.0.score += sg.score;
        m.1.insert(sg.source);
    }
    let mut out: Vec<Merged> = merged
        .into_iter()
        .map(|(mut m, sources)| {
            m.count = m.ids.len();
            m.score = similarity::round_to(m.score / m.count as f64, 2);
            m.source = sources.into_iter().collect::<Vec<_>>().join(" / ");
            m
        })
        .collect();
    out.sort_by(|a, b| {
        (b.count as f64 * b.score).total_cmp(&(a.count as f64 * a.score)).then_with(|| a.tag.cmp(&b.tag))
    });
    out
}

/// Single asset: its suggestions. Several: merged, with which assets each applies to.
/// Property values come first, then tags.
async fn suggestions(State(s): St, Json(body): Json<SuggestIn>) -> ApiResult {
    let ids: Vec<i64> = body.ids.into_iter().take(500).collect();
    if ids.is_empty() {
        return Ok(Json(json!([])));
    }
    let (tags, fields) = tokio::task::block_in_place(|| -> rusqlite::Result<_> {
        let idx = s.jobs.analysis.index.get(&s.db)?;
        let tags = similarity::suggest(&s.db, &idx, &ids, 12)?;
        let fields = s.db.tx(|c| properties::suggest_fields(c, &idx, &ids))?;
        Ok((tags, fields))
    })?;
    let mut out = merge_suggestions(fields.into_iter().flat_map(|(aid, list)| {
        list.into_iter().map(move |f| {
            let tag = properties::reject_key(&f.field, &f.value);
            (aid, Merged { tag, ids: vec![], count: 0, score: f.score, source: f.source, field: Some(f.field), label: Some(f.label), value: Some(f.value) })
        })
    }));
    let mut tag_list = merge_suggestions(tags.into_iter().flat_map(|(aid, list)| {
        list.into_iter().map(move |t| {
            (aid, Merged { tag: t.tag, ids: vec![], count: 0, score: t.score, source: t.source.to_string(), field: None, label: None, value: None })
        })
    }));
    tag_list.truncate(25);
    out.extend(tag_list);
    Ok(Json(serde_json::to_value(out).unwrap_or_default()))
}

#[derive(Deserialize)]
struct RejectIn {
    #[serde(default)]
    ids: Vec<i64>,
    tag: String,
    /// Never suggest this tag again.
    #[serde(default)]
    everywhere: bool,
}

async fn reject_suggestion(State(s): St, Json(body): Json<RejectIn>) -> ApiResult {
    let tag = normalize_tag(&body.tag);
    s.tx(|c| {
        if body.everywhere {
            c.execute("INSERT OR IGNORE INTO suggestion_ignore VALUES (?)", params![tag])?;
        }
        for id in &body.ids {
            c.execute("INSERT OR IGNORE INTO suggestion_rejects VALUES (?, ?)", params![id, tag])?;
        }
        Ok(())
    })?;
    ok()
}

async fn ignored_suggestions(State(s): St) -> ApiResult {
    let tags = s.tx(|c| {
        let mut stmt = c.prepare("SELECT tag FROM suggestion_ignore ORDER BY tag")?;
        let tags = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(tags)
    })?;
    Ok(Json(tags.into()))
}

async fn unignore_suggestion(State(s): St, Path(tag): Path<String>) -> ApiResult {
    s.tx(|c| {
        c.execute("DELETE FROM suggestion_ignore WHERE tag = ?", params![normalize_tag(&tag)])?;
        Ok(())
    })?;
    ok()
}

// ---------------------------------------------------------------- tags

async fn list_tags(State(s): St) -> ApiResult {
    let rows = s.tx(|c| {
        let mut stmt = c.prepare(
            "SELECT t.name, count(a.id) FROM tags t
             JOIN asset_tags x ON x.tag_id = t.id
             JOIN assets a ON a.id = x.asset_id AND a.missing = 0
             GROUP BY t.id ORDER BY t.name",
        )?;
        let rows = stmt
            .query_map([], |r| Ok(json!({ "name": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)? })))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;
    Ok(Json(rows.into()))
}

fn assets_with_tag(c: &Connection, tag_id: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = c.prepare("SELECT asset_id FROM asset_tags WHERE tag_id = ?")?;
    stmt.query_map(params![tag_id], |r| r.get(0))?.collect()
}

fn tag_id(c: &Connection, name: &str) -> rusqlite::Result<Option<i64>> {
    c.query_row("SELECT id FROM tags WHERE name = ?", params![name], |r| r.get(0)).optional()
}

#[derive(Deserialize)]
struct TagRename {
    new_name: String,
}

/// Rename a tag; if the new name exists, the two are merged.
async fn rename_tag(State(s): St, Path(name): Path<String>, Json(body): Json<TagRename>) -> ApiResult {
    let (old, new) = (normalize_tag(&name), normalize_tag(&body.new_name));
    if new.is_empty() || old == new {
        return ok();
    }
    s.tx(|c| {
        let old_id = tag_id(c, &old)?.ok_or_else(|| err(StatusCode::NOT_FOUND, "Tag not found"))?;
        let affected = assets_with_tag(c, old_id)?;
        let new_id = get_or_create_tags(c, &[&new])?[0];
        c.execute(
            "INSERT OR IGNORE INTO asset_tags SELECT asset_id, ? FROM asset_tags WHERE tag_id = ?",
            params![new_id, old_id],
        )?;
        c.execute("DELETE FROM tags WHERE id = ?", params![old_id])?;
        reindex_fts(c, Some(&affected))?;
        Ok(())
    })?;
    ok()
}

async fn delete_tag(State(s): St, Path(name): Path<String>) -> ApiResult {
    s.tx(|c| {
        if let Some(id) = tag_id(c, &normalize_tag(&name))? {
            let affected = assets_with_tag(c, id)?;
            c.execute("DELETE FROM tags WHERE id = ?", params![id])?;
            reindex_fts(c, Some(&affected))?;
        }
        Ok(())
    })?;
    ok()
}

// ---------------------------------------------------------------- files

/// Content type by extension, like Python's `mimetypes` for the formats we index.
fn mime_type(path: &FsPath) -> &'static str {
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_lowercase);
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        _ => "application/octet-stream",
    }
}

/// Starlette's `Content-Disposition` for a download: plain if the name needs no quoting,
/// RFC 5987 otherwise.
fn content_disposition(name: &str) -> String {
    let quoted: String = name
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    if quoted == name {
        format!("attachment; filename=\"{name}\"")
    } else {
        format!("attachment; filename*=utf-8''{quoted}")
    }
}

async fn thumb(State(s): St, Path(id): Path<i64>) -> ApiResult<Response> {
    let row = s.tx(|c| asset_row(c, id))?;
    let hash = row["file_hash"].as_str().unwrap_or_default().to_string();
    let source = location(&row);
    let data = tokio::task::block_in_place(|| {
        let path = s.jobs.thumbs.get(&source, &hash)?;
        Ok::<_, anyhow::Error>(std::fs::read(path)?)
    })
    .map_err(|_| err(StatusCode::NOT_FOUND, "Cannot read image"))?;
    Ok((
        [(header::CONTENT_TYPE, "image/webp"), (header::CACHE_CONTROL, "max-age=31536000, immutable")],
        data,
    )
        .into_response())
}

#[derive(Deserialize)]
struct FileParams {
    #[serde(default)]
    download: bool,
}

async fn original(State(s): St, Path(id): Path<i64>, Query(q): Query<FileParams>) -> ApiResult<Response> {
    let loc = location(&s.tx(|c| asset_row(c, id))?);
    let data = tokio::task::block_in_place(|| loc.read()).map_err(|_| err(StatusCode::NOT_FOUND, "File missing on disk"))?;
    let name = loc.file_name();
    let mut resp = ([(header::CONTENT_TYPE, mime_type(FsPath::new(&name)))], data).into_response();
    if q.download
        && let Ok(v) = content_disposition(&name).parse()
    {
        resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

/// Show the file in the system file manager.
pub fn reveal_in_file_manager(path: &FsPath) -> std::io::Result<()> {
    use std::process::Command;
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("explorer");
        c.arg("/select,").arg(path);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg("-R").arg(path);
        c
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(path.parent().unwrap_or(path));
        c
    };
    cmd.spawn().map(drop)
}

async fn reveal(State(s): St, ConnectInfo(addr): ConnectInfo<SocketAddr>, Path(id): Path<i64>) -> ApiResult {
    local_only(addr)?;
    let loc = location(&s.tx(|c| asset_row(c, id))?);
    reveal_in_file_manager(loc.disk_path()).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    ok()
}

// ---------------------------------------------------------------- export

#[derive(Deserialize)]
struct ExportIn {
    #[serde(default)]
    ids: Vec<i64>,
    /// Export every result of this search instead of `ids`.
    q: Option<String>,
    dest: Option<String>,
    #[serde(default = "yes")]
    flatten: bool,
}

/// Zips downloaded through the browser are built in memory; bigger exports go to a folder,
/// or to a file through the desktop app's save dialog.
const MAX_ZIP_DOWNLOAD: i64 = 1 << 30;

/// The ids an export covers: `ids`, or every result of `q` (in name order).
fn export_ids(s: &AppState, body: &ExportIn) -> ApiResult<Vec<i64>> {
    let Some(q) = &body.q else { return Ok(body.ids.clone()) };
    s.tx(|c| {
        let parsed = search::parse_with(q, &search::FieldKeys::new(&properties::list_fields(c)?));
        if parsed.ranked() {
            return Err(err(StatusCode::BAD_REQUEST, "Exporting every result doesn't work with image search; select them instead"));
        }
        let (wh, params) = search::build_sql(&parsed, false);
        let mut stmt = c.prepare(&format!("SELECT a.id FROM assets a WHERE {wh} ORDER BY {}", search::order_by("name")))?;
        let ids = stmt.query_map(params_from_iter(&params), |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(ids)
    })
}

/// Total stored size of the assets, in bytes.
fn total_size(s: &AppState, ids: &[i64]) -> ApiResult<i64> {
    s.tx(|c| {
        let mut total = 0;
        for chunk in ids.chunks(900) {
            total += c.query_row(
                &format!("SELECT coalesce(sum(size), 0) FROM assets WHERE id IN ({})", marks(chunk.len())),
                params_from_iter(chunk),
                |r| r.get::<_, i64>(0),
            )?;
        }
        Ok(total)
    })
}

fn export_items(s: &AppState, ids: &[i64]) -> ApiResult<Vec<export::Item>> {
    let mut items = Vec::new();
    for &id in ids {
        let row = s.tx(|c| asset_row(c, id))?;
        let location = location(&row);
        if location.exists() {
            let source_name = row["source_name"].as_str().unwrap_or_default().to_string();
            items.push(export::Item { rel_path: export_rel(&row), source_name, location });
        }
    }
    if items.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "None of the selected files exist on disk"));
    }
    Ok(items)
}

async fn export_folder(
    State(s): St,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<ExportIn>,
) -> ApiResult {
    local_only(addr)?;
    let Some(dest) = body.dest.clone().filter(|d| !d.is_empty()) else {
        return Err(err(StatusCode::BAD_REQUEST, "Destination folder required"));
    };
    let items = export_items(&s, &export_ids(&s, &body)?)?;
    let written = tokio::task::block_in_place(|| export::copy_to_folder(&items, &expand_user(&dest), body.flatten))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "count": written.len(), "dest": dest })))
}

async fn export_zip(State(s): St, Json(body): Json<ExportIn>) -> ApiResult<Response> {
    let ids = export_ids(&s, &body)?;
    if total_size(&s, &ids)? > MAX_ZIP_DOWNLOAD {
        return Err(err(StatusCode::BAD_REQUEST, "Too big to download as one zip (over 1 GB); copy to a folder instead"));
    }
    let items = export_items(&s, &ids)?;
    let data = tokio::task::block_in_place(|| export::build_zip(&items, body.flatten))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"assets.zip\""),
        ],
        data,
    )
        .into_response())
}

// ---------------------------------------------------------------- desktop and settings

async fn get_settings(State(s): St) -> ApiResult {
    let mut v = serde_json::to_value(s.jobs.settings.get()).unwrap_or_default();
    v["max_threads"] = std::thread::available_parallelism().map_or(1, |n| n.get()).into();
    v["ai_locked"] = s.jobs.settings.ai_override.is_some().into();
    v["model_cached"] = s.jobs.analysis.clip.image_model_cached().into();
    Ok(Json(v))
}

/// Merge the given fields into the settings, save, and apply them.
async fn update_settings(State(s): St, Json(patch): Json<Map<String, Value>>) -> ApiResult {
    let store = &s.jobs.settings;
    let mut merged = serde_json::to_value(store.get()).unwrap_or_default();
    for (k, v) in patch {
        if merged.get(&k).is_some() {
            merged[k] = v;
        }
    }
    let new: Settings =
        serde_json::from_value(merged).map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let before = store.get();
    let saved = store.set(new).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    s.jobs.analysis.set_ai(saved.ai);
    if saved.ai && !before.ai {
        s.jobs.start(); // embed everything, downloading the model if needed
    }
    get_settings(State(s)).await
}

/// Open a native folder picker on this machine (desktop app only).
async fn pick_folder(State(s): St, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> ApiResult {
    local_only(addr)?;
    let Some(desktop) = s.desktop.clone() else { return Ok(Json(json!({ "path": "" }))) };
    let picked = tokio::task::spawn_blocking(move || desktop.pick_folder()).await.ok().flatten();
    Ok(Json(json!({ "path": picked.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default() })))
}

#[derive(Deserialize)]
struct DragIn {
    ids: Vec<i64>,
}

/// Start a native drag of the assets' original files (desktop app only).
async fn start_drag(State(s): St, ConnectInfo(addr): ConnectInfo<SocketAddr>, Json(body): Json<DragIn>) -> ApiResult {
    local_only(addr)?;
    let Some(desktop) = s.desktop.clone() else {
        return Err(err(StatusCode::BAD_REQUEST, "Native drag needs the desktop app"));
    };
    let mut files = Vec::new();
    let mut icon = None;
    for &id in &body.ids {
        let row = s.tx(|c| asset_row(c, id))?;
        let loc = location(&row);
        if loc.exists() {
            let hash = row["file_hash"].as_str().unwrap_or_default();
            if icon.is_none() {
                icon = drag_icon(&s.jobs.thumbs.path(hash)).ok();
            }
            // Pack entries are extracted first: other apps need a real file to receive.
            let file = tokio::task::block_in_place(|| loc.materialize(&extracted_dir(&s), hash))
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            files.push(file);
        }
    }
    if files.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "None of the selected files exist on disk"));
    }
    desktop
        .start_drag(files, icon.unwrap_or_default())
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    ok()
}

/// Open the original in the system's default app.
async fn open_file(State(s): St, ConnectInfo(addr): ConnectInfo<SocketAddr>, Path(id): Path<i64>) -> ApiResult {
    local_only(addr)?;
    let row = s.tx(|c| asset_row(c, id))?;
    let loc = location(&row);
    if !loc.exists() {
        return Err(err(StatusCode::NOT_FOUND, "File missing on disk"));
    }
    let hash = row["file_hash"].as_str().unwrap_or_default();
    let path = tokio::task::block_in_place(|| loc.materialize(&extracted_dir(&s), hash))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    open::that_detached(&path).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    ok()
}

/// Build the zip and save it where the user picks (desktop app only; webviews don't
/// reliably download blobs).
async fn export_zip_save(State(s): St, ConnectInfo(addr): ConnectInfo<SocketAddr>, Json(body): Json<ExportIn>) -> ApiResult {
    local_only(addr)?;
    let Some(desktop) = s.desktop.clone() else {
        return Err(err(StatusCode::BAD_REQUEST, "Saving needs the desktop app"));
    };
    let items = export_items(&s, &export_ids(&s, &body)?)?;
    let Some(dest) = tokio::task::spawn_blocking(move || desktop.pick_save_file("assets.zip")).await.ok().flatten() else {
        return Ok(Json(json!({ "path": "" })));
    };
    tokio::task::block_in_place(|| -> anyhow::Result<()> {
        let file = std::io::BufWriter::new(std::fs::File::create(&dest)?);
        export::write_zip(file, &items, body.flatten)?;
        Ok(())
    })
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "path": dest.to_string_lossy() })))
}

#[derive(Deserialize)]
struct ImportIn {
    path: String,
}

/// Import a library made by the Python app into this (empty) library.
async fn import_library(State(s): St, ConnectInfo(addr): ConnectInfo<SocketAddr>, Json(body): Json<ImportIn>) -> ApiResult {
    local_only(addr)?;
    let sources: i64 = s.tx(|c| Ok(c.query_row("SELECT count(*) FROM sources", [], |r| r.get(0))?))?;
    if sources > 0 {
        return Err(err(StatusCode::CONFLICT, "Import needs an empty library; remove its folders first"));
    }
    if s.scanner.status().running {
        return Err(err(StatusCode::CONFLICT, "A scan is running"));
    }
    let src = expand_user(body.path.trim());
    let r = tokio::task::block_in_place(|| {
        crate::import::import_python_library(&s.db, &src, &s.data_dir, &s.model_dir)
    })
    .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    s.jobs.start(); // recompute colours (and embeddings, if AI is on)
    Ok(Json(json!({ "assets": r.assets, "thumbnails": r.thumbnails, "copied_models": r.copied_models })))
}

// ---------------------------------------------------------------- properties

async fn list_fields(State(s): St) -> ApiResult {
    let fields = s.tx(|c| Ok(properties::list_fields(c)?))?;
    Ok(Json(serde_json::to_value(fields).unwrap_or_default()))
}

async fn create_field(State(s): St, Json(body): Json<NewField>) -> ApiResult {
    let f = s.tx(|c| Ok(properties::create_field(c, body)?))?;
    Ok(Json(serde_json::to_value(f).unwrap_or_default()))
}

async fn update_field(State(s): St, Path(id): Path<i64>, Json(body): Json<FieldPatch>) -> ApiResult {
    let f = s.tx(|c| Ok(properties::update_field(c, id, body)?))?;
    Ok(Json(serde_json::to_value(f).unwrap_or_default()))
}

async fn delete_field(State(s): St, Path(id): Path<i64>) -> ApiResult {
    s.tx(|c| Ok(properties::delete_field(c, id)?))?;
    ok()
}

async fn facets(State(s): St) -> ApiResult {
    let f = s.tx(|c| Ok(properties::facets(c)?))?;
    Ok(Json(serde_json::to_value(f).unwrap_or_default()))
}

#[derive(Deserialize)]
struct PromoteIn {
    field: String,
    value: Value,
}

/// Turn a tag into a property value on every asset that has it.
async fn promote_tag(State(s): St, Path(name): Path<String>, Json(body): Json<PromoteIn>) -> ApiResult {
    let r = s.tx(|c| Ok(properties::promote_tag(c, &name, &body.field, &body.value)?))?;
    Ok(Json(serde_json::to_value(r).unwrap_or_default()))
}
