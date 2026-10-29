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
use crate::db::{Database, get_or_create_tags, marks, reindex_fts};
use crate::jobs::Jobs;
use crate::scanner::{Scanner, join_rel};
use crate::search;

pub struct AppState {
    pub db: Arc<Database>,
    pub scanner: Arc<Scanner>,
    pub jobs: Arc<Jobs>,
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
        .route("/api/assets/{id}", get(get_asset).patch(update_asset))
        .route("/api/assets/{id}/thumb", get(thumb))
        .route("/api/assets/{id}/file", get(original))
        .route("/api/assets/{id}/reveal", post(reveal))
        .route("/api/tags", get(list_tags))
        .with_state(state)
}

impl AppState {
    /// Run a database transaction from an async handler.
    fn tx<T>(&self, f: impl FnOnce(&Connection) -> Result<T, ApiError>) -> Result<T, ApiError> {
        tokio::task::block_in_place(|| self.db.tx(|c| f(c)))
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
        |r| row_json(r),
    )
    .optional()?
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "Asset not found"))
}

fn full_path(row: &Map<String, Value>) -> PathBuf {
    let s = |k: &str| row[k].as_str().unwrap_or_default().to_string();
    join_rel(FsPath::new(&s("source_path")), &s("rel_path"))
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

async fn meta() -> Json<Value> {
    let colors: Map<String, Value> =
        COLOR_HEX.iter().map(|(k, v)| (k.to_string(), (*v).into())).collect();
    Json(json!({
        "asset_types": ASSET_TYPES,
        "sorts": search::SORTS.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        "colors": colors,
        "ai": false, // TODO(M5): true once CLIP is ported and ASSET_LIBRARY_AI != 0
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
    // TODO(M4): real analysis progress.
    v["analysis"] =
        json!({ "running": false, "total": 0, "done": 0, "ai_available": false, "error": "" });
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
}

async fn list_assets(State(s): St, Query(p): Query<ListParams>) -> ApiResult {
    let parsed = search::parse(&p.q);
    if parsed.ranked() {
        // TODO(M5): rank by CLIP similarity.
        return Err(err(StatusCode::BAD_REQUEST, "Image search needs the AI extra (see README)"));
    }
    let (wh, params) = search::build_sql(&parsed, false);
    let order = search::order_by(&p.sort);
    let limit = p.limit.clamp(1, 1000);
    let (total, items) = s.tx(|c| {
        let total: i64 = c.query_row(
            &format!("SELECT count(*) FROM assets a WHERE {wh}"),
            params_from_iter(&params),
            |r| r.get(0),
        )?;
        let mut stmt = c.prepare(&format!(
            "SELECT a.id, a.name, a.rel_path, a.asset_type, a.width, a.height, a.favorite,
                    a.has_alpha, a.missing, a.file_hash
             FROM assets a WHERE {wh} ORDER BY {order} LIMIT ? OFFSET ?"
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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tags = tags_for(c, &items.iter().map(|i| i.id).collect::<Vec<_>>())?;
        for item in &mut items {
            item.tags = tags.remove(&item.id).unwrap_or_default();
        }
        Ok((total, items))
    })?;
    Ok(Json(json!({ "total": total, "items": items })))
}

fn asset_json(c: &Connection, id: i64) -> ApiResult {
    let mut d = asset_row(c, id)?;
    let path = full_path(&d);
    let extra = d.get("extra").and_then(Value::as_str).unwrap_or("{}");
    d["extra"] = serde_json::from_str(if extra.is_empty() { "{}" } else { extra }).unwrap_or_default();
    for key in ["favorite", "has_alpha", "missing"] {
        to_bool(&mut d, key);
    }
    d.insert("tags".into(), tags_for(c, &[id])?.remove(&id).unwrap_or_default().into());
    d.insert("full_path".into(), path.to_string_lossy().into());
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
}

async fn update_asset(State(s): St, Path(id): Path<i64>, Json(body): Json<AssetPatch>) -> ApiResult {
    s.tx(|c| {
        asset_row(c, id)?;
        let mut sets: Vec<&str> = Vec::new();
        let mut vals: Vec<SqlValue> = Vec::new();
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
    let source = full_path(&row);
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
    let path = full_path(&s.tx(|c| asset_row(c, id))?);
    let data = tokio::fs::read(&path)
        .await
        .map_err(|_| err(StatusCode::NOT_FOUND, "File missing on disk"))?;
    let mut resp = ([(header::CONTENT_TYPE, mime_type(&path))], data).into_response();
    if q.download {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Ok(v) = content_disposition(&name).parse() {
            resp.headers_mut().insert(header::CONTENT_DISPOSITION, v);
        }
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
    let path = full_path(&s.tx(|c| asset_row(c, id))?);
    reveal_in_file_manager(&path).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    ok()
}
