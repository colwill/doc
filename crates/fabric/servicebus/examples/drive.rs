//! T10 checks: messages survive a node restart, leases that are not acknowledged are handed
//! out again, retries end in the dead-letter queue, and request/reply works.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use async_trait::async_trait;
use clap::{Parser, ValueEnum};
use doc_consensus::ClusterClient;
use doc_servicebus::{
    Address, Message, NetworkServiceBus, QueueSpec, Request, ServiceBus, ServiceHandler,
};
use serde_json::json;

#[derive(Clone, Copy, ValueEnum, Debug)]
enum Mode {
    /// Sends messages, then receives and acknowledges them all.
    Messages,
    /// Leaves a message unacknowledged and waits for its lease to expire.
    Lease,
    /// Nacks a message until it is dead-lettered.
    DeadLetter,
    /// Calls a served address and waits for the reply.
    Request,
}

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    nodes: String,
    #[arg(long)]
    secrets: PathBuf,
    #[arg(long)]
    mode: Mode,
    #[arg(long, default_value_t = 200)]
    count: u64,
    #[arg(long, default_value = "core.drive")]
    address: String,
}

struct Echo;

#[async_trait]
impl ServiceHandler for Echo {
    async fn handle(&self, request: Request) -> Result<serde_json::Value, String> {
        match request.subject.as_str() {
            "fail" => Err("refused on purpose".into()),
            _ => Ok(json!({"echo": request.payload, "subject": request.subject})),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let nodes: Vec<String> = cli.nodes.split(',').map(str::to_string).collect();
    let token = std::fs::read_to_string(cli.secrets.join("tokens/buses/servicebus.token"))?
        .trim()
        .to_string();
    let client = ClusterClient::new("servicebus", nodes, token, cli.secrets)?;
    let bus = NetworkServiceBus::connect(client, "drive").await?;
    let address = Address::new(cli.address)?;

    match cli.mode {
        Mode::Messages => {
            bus.register_queue(QueueSpec::new(address.clone())).await?;
            let mut sent = 0;
            for sequence in 0..cli.count {
                let message = Message::new(address.clone(), "work", json!({"sequence": sequence}));
                if bus.send(message).await.is_ok() {
                    sent += 1;
                }
            }
            let mut received = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(60);
            while (received.len() as u64) < sent && Instant::now() < deadline {
                match bus.receive(&address).await? {
                    Some(lease) => {
                        if let Some(sequence) = lease.message.payload["sequence"].as_u64() {
                            received.push(sequence);
                        }
                        bus.ack(lease.id).await?;
                    }
                    None => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            }
            let unique: std::collections::BTreeSet<u64> = received.iter().copied().collect();
            println!(
                "{}",
                json!({
                    "sent": sent,
                    "received": received.len(),
                    "unique": unique.len(),
                    "missing": sent as usize - unique.len(),
                })
            );
        }
        Mode::Lease => {
            let queue = QueueSpec::new(address.clone());
            bus.register_queue(QueueSpec { lease: Duration::from_secs(2), ..queue }).await?;
            bus.send(Message::new(address.clone(), "work", json!({"sequence": 1}))).await?;
            let first =
                bus.receive(&address).await?.ok_or_else(|| anyhow::anyhow!("no message"))?;
            let immediately = bus.receive(&address).await?;
            tokio::time::sleep(Duration::from_secs(4)).await;
            let again = bus.receive(&address).await?;
            println!(
                "{}",
                json!({
                    "first_attempt": first.message.attempts,
                    "while_leased": immediately.is_some(),
                    "after_lease_expired": again.as_ref().map(|lease| lease.message.attempts),
                    "same_message": again
                        .as_ref()
                        .map(|lease| lease.message.payload == first.message.payload),
                })
            );
            if let Some(lease) = again {
                bus.ack(lease.id).await?;
            }
        }
        Mode::DeadLetter => {
            let queue = QueueSpec::new(address.clone()).with_attempts(3);
            bus.register_queue(queue).await?;
            bus.send(Message::new(address.clone(), "work", json!({"poison": true}))).await?;
            let mut attempts = 0;
            let deadline = Instant::now() + Duration::from_secs(30);
            while Instant::now() < deadline {
                match bus.receive(&address).await? {
                    Some(lease) => {
                        attempts += 1;
                        println!(
                            "  attempt {attempts}: message attempts={} lease={}",
                            lease.message.attempts, lease.id
                        );
                        bus.nack(lease.id, "cannot handle this").await?;
                    }
                    None => tokio::time::sleep(Duration::from_millis(100)).await,
                }
                if !bus.dead_letters(&address).await?.is_empty() {
                    break;
                }
            }
            let dead = bus.dead_letters(&address).await?;
            let depth = bus.depth(&address).await?;
            println!(
                "{}",
                json!({
                    "attempts": attempts,
                    "ready_depth": depth,
                    "dead_letters": dead.len(),
                    "reason": dead.first().map(|letter| letter.reason.clone()),
                })
            );
        }
        Mode::Request => {
            let served = Address::new("core.echo")?;
            bus.serve(served.clone(), Arc::new(Echo)).await?;
            let started = Instant::now();
            let reply =
                bus.request(&served, "hello", json!({"n": 1}), Duration::from_secs(10)).await?;
            let failure = bus
                .request(&served, "fail", json!({}), Duration::from_secs(10))
                .await
                .err()
                .map(|err| err.to_string());
            println!(
                "{}",
                json!({
                    "reply": reply,
                    "round_trip_ms": started.elapsed().as_millis() as u64,
                    "handler_error": failure,
                })
            );
            if reply["echo"]["n"] != 1 {
                bail!("unexpected reply: {reply}");
            }
        }
    }
    Ok(())
}
