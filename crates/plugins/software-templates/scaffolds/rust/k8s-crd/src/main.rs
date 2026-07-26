//! {{ values.name }} — {{ values.description }}
//!
//! The custom resource lives in `types.rs`; this program is what writes its CRD out and what reads
//! the resources back, so the types are used by something from the first commit.
//!
//! ```sh
//! cargo run -- crd > config/crd/bases/crd.yaml   # the schema the cluster enforces
//! cargo run -- list                              # what is in the cluster now
//! ```

mod platform;
mod types;

use anyhow::{Context as _, Result};
use kube::api::{Api, ListParams, PostParams};
use kube::{Client, CustomResourceExt, ResourceExt};

use crate::platform::{Config, Flags, Telemetry};
use crate::types::{{ scaffold.kind }};
use crate::types::{{ scaffold.kind }}Spec;

#[tokio::main]
async fn main() -> Result<()> {
    let command = std::env::args().nth(1).unwrap_or_else(|| "list".to_string());
    if command == "crd" {
        println!("{}", serde_yaml::to_string(&{{ scaffold.kind }}::crd())?);
        return Ok(());
    }

    let config = Config::load();
    let telemetry = Telemetry::start(&config).ok();
    let flags = Flags::start(&config).await;
    let namespace = std::env::var("NAMESPACE").unwrap_or_else(|_| "default".to_string());

    let client = Client::try_default().await.context("reaching the cluster")?;
    let resources: Api<{{ scaffold.kind }}> = Api::namespaced(client, &namespace);

    match command.as_str() {
        "create" => {
            let name = std::env::args().nth(2).unwrap_or_else(|| "{{ values.name }}-sample".to_string());
            let owner = flags.string("default-owner", "{{ values.team | name | default('platform') }}");
            let mut wanted = {{ scaffold.kind }}::new(
                &name,
                {{ scaffold.kind }}Spec { owner, size: 1, retention: None, settings: Default::default() },
            );
            wanted.meta_mut().namespace = Some(namespace.clone());
            let made = resources.create(&PostParams::default(), &wanted).await?;
            println!("{{ scaffold.kind }}/{} created in {namespace}", made.name_any());
        }
        _ => {
            let held = resources.list(&ListParams::default()).await?;
            if held.items.is_empty() {
                println!("there are no {{ scaffold.plural }} in {namespace}");
            }
            for one in held.items {
                println!(
                    "{:<30} owner={:<20} size={} phase={:?}",
                    one.name_any(),
                    one.spec.owner,
                    one.spec.size,
                    one.status.map(|status| status.phase).unwrap_or_default()
                );
            }
        }
    }

    if let Some(telemetry) = telemetry {
        telemetry.shutdown();
    }
    Ok(())
}
