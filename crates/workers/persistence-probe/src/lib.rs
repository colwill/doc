//! Persistence probe: whether Postgres answers a trivial query, and how long it took to do so.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use doc_backend::db::repositories::HealthRepository;
use doc_backend::status::{Component, Health, Probe};

pub const NAME: &str = "postgres";

pub struct PersistenceProbe {
    health: Arc<dyn HealthRepository>,
}

impl PersistenceProbe {
    pub fn new(health: Arc<dyn HealthRepository>) -> Self {
        Self { health }
    }
}

#[async_trait]
impl Probe for PersistenceProbe {
    fn name(&self) -> &str {
        NAME
    }

    async fn check(&self) -> Component {
        let started = Instant::now();
        let component = match self.health.ping().await {
            Ok(()) => Component::new("database", NAME, Health::Up).detail("answering queries"),
            Err(err) => Component::new("database", NAME, Health::Down).detail(err.to_string()),
        };
        component.took(started.elapsed())
    }
}
