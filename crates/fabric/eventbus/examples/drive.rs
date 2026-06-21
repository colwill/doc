//! T09 check: publishes a numbered stream of events while a consumer group reads it, then
//! reports anything missing or repeated. Delivery is at least once, so repeats are expected
//! and gaps are not.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use doc_consensus::ClusterClient;
use doc_eventbus::{
    ConsumerGroup, Event, EventBus, NetworkEventBus, Topic, TopicFilter, TopicSpec,
};
use parking_lot::Mutex;
use serde_json::json;

#[derive(Parser)]
struct Cli {
    #[arg(long)]
    nodes: String,
    #[arg(long)]
    secrets: PathBuf,
    #[arg(long, env = "DOC_BUS_TOKEN")]
    token: Option<String>,
    #[arg(long, default_value = "platform.drive.tick")]
    topic: String,
    #[arg(long, default_value = "platform.>")]
    filter: String,
    #[arg(long, default_value = "drive")]
    group: String,
    #[arg(long, default_value_t = 20)]
    seconds: u64,
    #[arg(long, default_value_t = 20)]
    per_second: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,drive=info".into()),
        )
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let nodes: Vec<String> = cli.nodes.split(',').map(str::to_string).collect();
    let token = match cli.token {
        Some(token) => token,
        None => std::fs::read_to_string(cli.secrets.join("tokens/buses/eventbus.token"))?
            .trim()
            .to_string(),
    };
    let client = ClusterClient::new("eventbus", nodes, token, cli.secrets)?;
    let bus = NetworkEventBus::new(client);

    bus.register_topic(TopicSpec::new(
        TopicFilter::new(cli.filter.clone())?,
        100_000,
        Duration::from_secs(3_600),
    ))
    .await?;

    let received: Arc<Mutex<Vec<u64>>> = Arc::default();
    let consumer = {
        let received = received.clone();
        let group = ConsumerGroup::new(cli.group.clone(), TopicFilter::new(cli.filter.clone())?);
        let mut subscription = bus.subscribe(group).await?;
        tokio::spawn(async move {
            while let Some(delivery) = subscription.next().await {
                if let Some(sequence) = delivery.event.payload["sequence"].as_u64() {
                    received.lock().push(sequence);
                }
                let _ = subscription.ack(delivery.id).await;
            }
        })
    };

    let topic = Topic::new(cli.topic.clone())?;
    let mut published = 0u64;
    let mut failed = 0u64;
    let interval = Duration::from_micros(1_000_000 / cli.per_second.max(1));
    let stop = tokio::time::Instant::now() + Duration::from_secs(cli.seconds);
    while tokio::time::Instant::now() < stop {
        let event = Event::new(topic.clone(), "drive", json!({"sequence": published}))
            .idempotent(format!("drive-{published}"));
        match bus.publish(event).await {
            Ok(_) => published += 1,
            Err(err) => {
                failed += 1;
                tracing::warn!(%err, "publish failed");
            }
        }
        tokio::time::sleep(interval).await;
    }

    // Give the consumer a moment to drain what is still in flight.
    tokio::time::sleep(Duration::from_secs(3)).await;
    consumer.abort();

    let sequences = received.lock().clone();
    let unique: BTreeSet<u64> = sequences.iter().copied().collect();
    let missing: Vec<u64> = (0..published).filter(|sequence| !unique.contains(sequence)).collect();
    println!(
        "{}",
        json!({
            "published": published,
            "publish_failures": failed,
            "delivered": sequences.len(),
            "unique": unique.len(),
            "repeated": sequences.len() - unique.len(),
            "missing": missing.len(),
            "first_missing": missing.first(),
        })
    );
    Ok(())
}
