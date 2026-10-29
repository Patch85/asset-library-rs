//! Wire up the library and serve the API; shared by the CLI and the desktop app.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::analysis::Analysis;
use crate::api::{self, AppState};
use crate::clip::Clip;
use crate::config::Config;
use crate::db::Database;
use crate::desktop::Desktop;
use crate::jobs::Jobs;
use crate::scanner::Scanner;
use crate::settings::{Settings, SettingsStore};
use crate::thumbs::Thumbs;

#[derive(Default)]
pub struct Options {
    /// An extra place to look for ONNX Runtime (the desktop app's resources).
    pub runtime_dir: Option<PathBuf>,
    /// Settings to use when `settings.json` doesn't exist yet.
    pub defaults: Settings,
    pub desktop: Option<Arc<dyn Desktop>>,
}

/// Open the library and start the background pipeline.
pub fn open(cfg: &Config, opts: Options) -> anyhow::Result<Arc<AppState>> {
    let db = Arc::new(Database::open(&cfg.db_path())?);
    let mut settings = SettingsStore::load(cfg.data_dir.join("settings.json"), opts.defaults);
    settings.ai_override = cfg.ai_override;
    let settings = Arc::new(settings);
    let scanner = Arc::new(Scanner::new(Arc::clone(&db)));
    let thumbs = Arc::new(Thumbs::new(cfg.data_dir.join("thumbs")));
    let mut clip = Clip::new(cfg.model_dir.clone());
    if let Some(dir) = opts.runtime_dir {
        clip = clip.with_runtime_dir(dir);
    }
    let analysis = Arc::new(Analysis::new(Arc::clone(&db), Arc::clone(&thumbs), clip, settings.get().ai));
    let jobs = Arc::new(Jobs::new(Arc::clone(&db), thumbs, analysis, settings));
    jobs.start(); // fill in any thumbnails/analysis not done yet
    Ok(Arc::new(AppState {
        db,
        scanner,
        jobs,
        desktop: opts.desktop,
        data_dir: cfg.data_dir.clone(),
        model_dir: cfg.model_dir.clone(),
    }))
}

/// Serve the API. `ASSET_LIBRARY_LOG=1` logs each request to stderr.
pub async fn serve(listener: tokio::net::TcpListener, state: Arc<AppState>) -> std::io::Result<()> {
    let mut app = api::router(state);
    if std::env::var_os("ASSET_LIBRARY_LOG").is_some() {
        app = app.layer(axum::middleware::from_fn(log_request));
    }
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await
}

async fn log_request(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let (method, uri, start) = (req.method().clone(), req.uri().clone(), std::time::Instant::now());
    let resp = next.run(req).await;
    eprintln!("{method} {uri} {} {:.0?}", resp.status().as_u16(), start.elapsed());
    resp
}
