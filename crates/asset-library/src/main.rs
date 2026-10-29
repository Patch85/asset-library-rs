//! Start the asset library: asset-library [--port 8765] [--lan] [--no-browser]
//! Benchmark this machine: asset-library bench <folder>

use std::net::Ipv4Addr;
use std::path::PathBuf;

use asset_library::config::Config;
use asset_library::thumbs::default_threads;
use asset_library::{bench, server};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about = "A local, searchable, taggable library for TTRPG assets")]
struct Args {
    #[arg(long, default_value_t = 8765)]
    port: u16,
    /// Listen on all interfaces so other devices on your network can browse.
    #[arg(long)]
    lan: bool,
    #[arg(long)]
    no_browser: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Measure thumbnail, colour and CLIP speed and peak memory on a folder of images.
    Bench {
        folder: PathBuf,
        /// How many images to use.
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long, default_value_t = 64)]
        batch: usize,
        #[arg(long, default_value_t = default_threads())]
        threads: usize,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = Config::from_env();
    if let Some(Command::Bench { folder, limit, batch, threads }) = args.command {
        let opts = bench::Options { limit, batch, threads };
        return tokio::task::block_in_place(|| bench::run(&folder, &cfg.model_dir, &opts));
    }

    let state = server::open(&cfg, None)?;
    let host = if args.lan { Ipv4Addr::UNSPECIFIED } else { Ipv4Addr::LOCALHOST };
    let listener = tokio::net::TcpListener::bind((host, args.port)).await?;
    let url = format!("http://127.0.0.1:{}", args.port);
    println!("Asset Library running at {url}  (Ctrl+C to stop)");
    if !args.no_browser {
        let _ = open::that_detached(&url);
    }
    server::serve(listener, state).await?;
    Ok(())
}
