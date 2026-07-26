//! {{ values.name }}'s gRPC service: the generated contract, and what answers it.

use tonic::{Request, Response, Status};
use tracing::instrument;

use crate::platform::{Config, Flags};

/// What `build.rs` generated from `proto/service.proto`.
pub mod proto {
    tonic::include_proto!("{{ scaffold.package }}.v1");
}

/// The descriptor set the reflection service serves.
pub const DESCRIPTOR: &[u8] = tonic::include_file_descriptor_set!("descriptor");

pub struct Service {
    config: Config,
    flags: Flags,
}

impl Service {
    pub fn new(config: Config, flags: Flags) -> Self {
        Self { config, flags }
    }
}

#[tonic::async_trait]
impl proto::{{ scaffold.package }}_service_server::{{ scaffold.pascal }}Service for Service {
    #[instrument(skip_all)]
    async fn greet(
        &self,
        request: Request<proto::GreetRequest>,
    ) -> Result<Response<proto::GreetResponse>, Status> {
        let who = request.into_inner().who;
        let who = if who.is_empty() { "world".to_string() } else { who };

        // What it says is a flag in DOC, with the value this service falls back to beside it.
        let greeting = self.flags.string("greeting", "Hello");
        let greeting = if self.flags.bool("shout", false) { greeting.to_uppercase() } else { greeting };

        Ok(Response::new(proto::GreetResponse {
            message: format!("{greeting}, {who}"),
            service: self.config.service.clone(),
            environment: self.config.environment.clone(),
        }))
    }
}
