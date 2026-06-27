//! The Service Bus service: a Raft node serving queues, leases, retries and dead letters.

use anyhow::Result;
use clap::Parser;
use doc_consensus::{BusArgs, init_tracing, run};
use doc_servicebus::server::{ServiceBusService, machine::ServiceMachine};

#[derive(Parser)]
#[command(name = "doc-servicebus", version, about)]
struct Cli {
    #[command(flatten)]
    bus: BusArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _telemetry = init_tracing("servicebus");
    let cli = Cli::parse();
    run("servicebus", cli.bus, true, ServiceMachine::default(), ServiceBusService::new).await
}
