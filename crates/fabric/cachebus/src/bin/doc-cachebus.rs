//! The Cache Bus service: a Raft node serving get, set, delete and compare-and-set.
//! It runs with relaxed durability, because a cache may lose its last few milliseconds
//! after a whole-cluster crash (ADR-0002).

use anyhow::Result;
use clap::Parser;
use doc_cachebus::server::{CacheBusService, machine::CacheMachine};
use doc_consensus::{BusArgs, init_tracing, run};

#[derive(Parser)]
#[command(name = "doc-cachebus", version, about)]
struct Cli {
    #[command(flatten)]
    bus: BusArgs,
    /// Turns on fsync for every append; off by default for the cache.
    #[arg(long, env = "DOC_DURABLE", default_value_t = false)]
    durable: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _telemetry = init_tracing("cachebus");
    let cli = Cli::parse();
    run("cachebus", cli.bus, cli.durable, CacheMachine::default(), CacheBusService::new).await
}
