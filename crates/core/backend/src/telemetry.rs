//! The backend's gauges: each plugin's state, and how many tasks wait on each queue or were
//! dead-lettered there. They are sampled in the background, since the collector cannot wait.

use std::sync::Arc;
use std::time::Duration;

use doc_background_tasks::{PLUGIN_RUNS, QUEUE};
use doc_servicebus::Address;
use opentelemetry::KeyValue;
use opentelemetry::metrics::ObservableGauge;
use parking_lot::Mutex;

use crate::api::AppState;

const SAMPLE_EVERY: Duration = Duration::from_secs(15);

#[derive(Default)]
struct Sample {
    plugins: Vec<(String, String, &'static str)>,
    queues: Vec<(&'static str, u64, u64)>,
}

pub fn start(state: &AppState) {
    let sample = Arc::new(Mutex::new(Sample::default()));
    let gauges = gauges(&sample);
    let state = state.clone();
    tokio::spawn(async move {
        let _gauges = gauges;
        loop {
            let plugins = state.plugins.list().await;
            let plugins = plugins
                .into_iter()
                .map(|entry| (entry.id, entry.manifest.version, entry.state.as_str()))
                .collect();
            let mut queues = Vec::new();
            for queue in [QUEUE, PLUGIN_RUNS] {
                let Ok(address) = Address::core(queue) else { continue };
                let services = state.buses.services.as_ref();
                let (Ok(waiting), Ok(dead)) =
                    (services.depth(&address).await, services.dead_letters(&address).await)
                else {
                    continue;
                };
                queues.push((queue, waiting as u64, dead.len() as u64));
            }
            *sample.lock() = Sample { plugins, queues };
            tokio::time::sleep(SAMPLE_EVERY).await;
        }
    });
}

fn gauges(sample: &Arc<Mutex<Sample>>) -> Vec<ObservableGauge<u64>> {
    let meter = doc_telemetry::meter();
    let states = {
        let sample = sample.clone();
        meter
            .u64_observable_gauge("doc.plugin.state")
            .with_description("1 for each plugin's current state and version")
            .with_callback(move |observer| {
                for (plugin, version, state) in &sample.lock().plugins {
                    let labels = [
                        KeyValue::new("plugin", plugin.clone()),
                        KeyValue::new("version", version.clone()),
                        KeyValue::new("state", *state),
                    ];
                    observer.observe(1, &labels);
                }
            })
            .build()
    };
    let queue = |name: &'static str, description: &'static str, read: fn(u64, u64) -> u64| {
        let sample = sample.clone();
        meter
            .u64_observable_gauge(name)
            .with_unit("{task}")
            .with_description(description)
            .with_callback(move |observer| {
                for (queue, waiting, dead) in &sample.lock().queues {
                    observer.observe(read(*waiting, *dead), &[KeyValue::new("queue", *queue)]);
                }
            })
            .build()
    };
    vec![
        states,
        queue("doc.tasks.waiting", "Tasks queued and not yet taken", |waiting, _| waiting),
        queue("doc.tasks.dead", "Task messages dead-lettered after their attempts", |_, dead| dead),
    ]
}
