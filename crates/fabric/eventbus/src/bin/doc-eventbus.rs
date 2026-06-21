//! The Event Bus service: a Raft node serving publish, topic registration and subscriptions.

use anyhow::Result;
use clap::Parser;
use doc_consensus::{BusArgs, init_tracing, run};
use doc_eventbus::server::{EventBusService, machine::EventMachine};

#[derive(Parser)]
#[command(name = "doc-eventbus", version, about)]
struct Cli {
    #[command(flatten)]
    bus: BusArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _telemetry = init_tracing("eventbus");
    let cli = Cli::parse();
    run("eventbus", cli.bus, true, EventMachine::default(), EventBusService::new).await
}
