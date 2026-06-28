//! `doc-backend`: serves the platform API and fills the secrets volume at bootstrap.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use doc_backend::{bootstrap, config::Config, server};

#[derive(Parser)]
#[command(name = "doc-backend", version, about)]
struct Cli {
    /// Configuration file; also read from DOC_CONFIG.
    #[arg(long, short, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the platform API.
    Serve,
    /// Fill the secrets volume with the certificate authority, certificates and tokens.
    Bootstrap {
        /// Secrets directory; defaults to the configured one.
        #[arg(long)]
        secrets: Option<PathBuf>,
    },
    /// Make a new settings key and encrypt every stored plugin secret under it (ADR-0007). The
    /// old key stays in the volume, and nothing is lost if this stops half way: each value says
    /// which key opens it.
    RotateSettingsKey {
        /// Secrets directory; defaults to the configured one.
        #[arg(long)]
        secrets: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let _telemetry = doc_telemetry::init("doc-backend", env!("CARGO_PKG_VERSION"), "info");
    let config = Config::load(cli.config.as_deref())?;
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => server::serve(config).await,
        Command::Bootstrap { secrets } => {
            let dir = secrets.unwrap_or_else(|| config.secrets.dir.clone());
            let report = bootstrap::run(&config, &dir)?;
            tracing::info!(
                dir = %dir.display(),
                created = report.created.len(),
                kept = report.kept.len(),
                "bootstrap finished"
            );
            for path in &report.created {
                tracing::info!(path, "created");
            }
            Ok(())
        }
        Command::RotateSettingsKey { secrets } => {
            let dir = secrets.unwrap_or_else(|| config.secrets.dir.clone());
            let rotated = server::rotate_settings_key(&config, &dir).await?;
            tracing::info!(
                dir = %dir.display(),
                key = %rotated.key_id,
                rotated = rotated.rotated,
                left = rotated.unreadable,
                "settings key rotated"
            );
            Ok(())
        }
    }
}
