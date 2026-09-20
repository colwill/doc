//! frontend probe: whether the frontend answers its health endpoint, and how quickly. It checks over
//! HTTP rather than in process, so what it reports is what a caller would actually get.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use doc_backend::status::{Component, Health, Probe};

pub const NAME: &str = "frontend";
const TIMEOUT: Duration = Duration::from_secs(5);

pub struct FrontendProbe {
    url: String,
    http: reqwest::Client,
}

impl FrontendProbe {
    /// `base` is the root the service is reached at, such as `http://frontend:8080`.
    pub fn new(base: &str) -> Result<Self, reqwest::Error> {
        Ok(Self {
            url: format!("{}/healthz", base.trim_end_matches('/')),
            http: reqwest::Client::builder().timeout(TIMEOUT).build()?,
        })
    }
}

#[async_trait]
impl Probe for FrontendProbe {
    fn name(&self) -> &str {
        NAME
    }

    async fn check(&self) -> Component {
        let started = Instant::now();
        let component = match self.http.get(&self.url).send().await {
            Ok(response) if response.status().is_success() => {
                Component::new("service", NAME, Health::Up).detail("answering /healthz")
            }
            Ok(response) => Component::new("service", NAME, Health::Degraded)
                .detail(format!("/healthz answered {}", response.status())),
            Err(err) => {
                Component::new("service", NAME, Health::Down).detail(err.without_url().to_string())
            }
        };
        component.took(started.elapsed())
    }
}
