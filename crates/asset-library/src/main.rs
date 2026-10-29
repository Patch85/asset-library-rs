//! Start the asset library: asset-library [--port 8765] [--lan] [--no-browser]

use std::net::Ipv4Addr;

use asset_library::config::Config;
use asset_library::server;
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
