//! `doc-frontend`: the server-rendered UI over the backend API.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use doc_frontend::{config::Config, server};

#[derive(Parser)]
#[command(name = "doc-frontend", version, about)]
struct Cli {
    /// Configuration file; also read from DOC_CONFIG.
    #[arg(long, short)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _telemetry = doc_telemetry::init("doc-frontend", env!("CARGO_PKG_VERSION"), "info");
    let cli = Cli::parse();
    server::serve(Config::load(cli.config.as_deref())?).await
}
