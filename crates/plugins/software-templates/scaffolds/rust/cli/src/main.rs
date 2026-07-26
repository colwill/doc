//! {{ values.name }} — {{ values.description }}

mod command;
mod platform;

use anyhow::Result;
use clap::Parser;

use crate::platform::{Config, Flags, Telemetry};

#[derive(Parser)]
#[command(name = "{{ values.name }}", about = "{{ values.description }}", version)]
struct Arguments {
    #[command(subcommand)]
    command: command::Command,
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let config = Config::load();

    // A command still runs when the collector cannot be reached; it is only not measured.
    let telemetry = match Telemetry::start(&config) {
        Ok(telemetry) => Some(telemetry),
        Err(err) => {
            eprintln!("telemetry is not being exported: {err}");
            None
        }
    };
    let flags = Flags::start(&config).await;

    let outcome = arguments.command.run(&config, &flags).await;

    if let Some(telemetry) = telemetry {
        telemetry.shutdown();
    }
    outcome
}
