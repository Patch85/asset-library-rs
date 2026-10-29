//! Start the asset library: asset-library [--port 8765] [--lan] [--no-browser]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use asset_library::analysis::Analysis;
use asset_library::api::{self, AppState};
use asset_library::config::Config;
use asset_library::db::Database;
use asset_library::jobs::Jobs;
use asset_library::scanner::Scanner;
use asset_library::thumbs::Thumbs;
use clap::Parser;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 8765)]
    port: u16,
    /// Listen on all interfaces so other devices on your network can browse.
    #[arg(long)]
    lan: bool,
    #[arg(long)]
    no_browser: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = Config::from_env();
    let db = Arc::new(Database::open(&cfg.db_path())?);
    let scanner = Arc::new(Scanner::new(Arc::clone(&db)));
    let thumbs = Arc::new(Thumbs::new(cfg.data_dir.join("thumbs")));
    let analysis = Arc::new(Analysis::new(Arc::clone(&db), Arc::clone(&thumbs)));
    let jobs = Arc::new(Jobs::new(Arc::clone(&db), thumbs, analysis));
    jobs.start(); // fill in any thumbnails/analysis not done yet
    let app = api::router(Arc::new(AppState { db, scanner, jobs }));

    let host = if args.lan { Ipv4Addr::UNSPECIFIED } else { Ipv4Addr::LOCALHOST };
    let listener = tokio::net::TcpListener::bind((host, args.port)).await?;
    let url = format!("http://127.0.0.1:{}", args.port);
    println!("Asset Library running at {url}  (Ctrl+C to stop)");
    if !args.no_browser {
        let _ = open::that_detached(&url);
    }
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}
