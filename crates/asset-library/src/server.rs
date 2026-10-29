//! Wire up the library and serve the API; shared by the CLI and the desktop app.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use crate::analysis::Analysis;
use crate::api::{self, AppState};
use crate::clip::Clip;
use crate::config::Config;
use crate::db::Database;
use crate::jobs::Jobs;
use crate::scanner::Scanner;
use crate::thumbs::Thumbs;

/// Open the library and start the background pipeline. `runtime_dir` is an extra place to
/// look for ONNX Runtime.
pub fn open(cfg: &Config, runtime_dir: Option<PathBuf>) -> anyhow::Result<Arc<AppState>> {
    let db = Arc::new(Database::open(&cfg.db_path())?);
    let scanner = Arc::new(Scanner::new(Arc::clone(&db)));
    let thumbs = Arc::new(Thumbs::new(cfg.data_dir.join("thumbs")));
    let mut clip = Clip::new(cfg.model_dir.clone());
    if let Some(dir) = runtime_dir {
        clip = clip.with_runtime_dir(dir);
    }
    let analysis = Arc::new(Analysis::new(Arc::clone(&db), Arc::clone(&thumbs), clip, cfg.ai_enabled));
    let jobs = Arc::new(Jobs::new(Arc::clone(&db), thumbs, analysis));
    jobs.start(); // fill in any thumbnails/analysis not done yet
    Ok(Arc::new(AppState { db, scanner, jobs }))
}

pub async fn serve(listener: tokio::net::TcpListener, state: Arc<AppState>) -> std::io::Result<()> {
    let app = api::router(state);
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await
}
