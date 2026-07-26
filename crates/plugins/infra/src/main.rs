//! Infra: self-service infrastructure. Templates say what may be asked for from each vendor, and
//! requests are checked, provisioned in the background, kept in Resource Definitions, warned about
//! and torn down when they expire.

mod api;
mod catalog;
mod store;
mod ui;
mod vendors;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, CustomPermission, DashboardItem, Manifest, Nav, Plugin, PluginError,
    Request, ResourcePanel, Response, RunInput, RunOutput, Schedule, Setting, SettingKind,
};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub detail: String,
}

impl Refusal {
    pub fn bad(detail: impl Into<String>) -> Self {
        Self { status: 400, detail: detail.into() }
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self { status: 403, detail: detail.into() }
    }

    pub fn missing(detail: impl Into<String>) -> Self {
        Self { status: 404, detail: detail.into() }
    }

    pub fn conflict(detail: impl Into<String>) -> Self {
        Self { status: 409, detail: detail.into() }
    }

    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self { status: 503, detail: detail.into() }
    }

    pub fn response(&self) -> Response {
        let kind = match self.status {
            400 => "bad-request",
            403 => "forbidden",
            404 => "not-found",
            409 => "conflict",
            _ => "unavailable",
        };
        Response::problem(self.status, kind, &self.detail)
    }
}

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        tracing::warn!(%err, "a call to the backend failed");
        Self::unavailable(format!("the backend could not do that: {err}"))
    }
}

impl From<Refusal> for PluginError {
    fn from(refusal: Refusal) -> Self {
        Self::Message(refusal.detail)
    }
}

#[derive(Default)]
struct Infra;

#[async_trait]
impl Plugin for Infra {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // The vendors' credentials are read wherever a call is made, so what core gave us is
        // kept where those calls can reach it (ADR-0007).
        vendors::remember(backend);
        tracing::info!(version = backend.version(), "infra loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    /// A `provision` or `teardown` task for one request, or the `lifecycle` schedule.
    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        let id = |key: &str| input.payload[key].as_str().and_then(|id| id.parse::<Uuid>().ok());
        let output = match (input.payload["schedule"].as_str(), id("provision"), id("teardown")) {
            (Some("lifecycle"), _, _) => api::lifecycle(backend).await?,
            (_, Some(request), _) => api::provision(backend, request).await?,
            (_, _, Some(request)) => api::teardown(backend, request).await?,
            _ => return Err(PluginError::from("a run provisions or tears down a request")),
        };
        Ok(RunOutput { payload: output })
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        api::handle(backend, request).await
    }
}

doc_plugin_sdk::main!(
    Infra,
    Manifest {
        id: "infra".into(),
        classification: Classification::Async,
        custom_permissions: [
            "templates",
            "selfservice",
            "selfservice-linode",
            "selfservice-aws",
            "selfservice-gcp",
            "selfservice-azure"
        ]
        .into_iter()
        .map(CustomPermission::user)
        .collect(),
        // Development is what people ask DOC for - sandboxes, agent harnesses, anything a team
        // builds against. Production is only shown: DOC does not make it.
        nav: vec![
            Nav::new("Development", "/development")
                .described("Ask for developer infrastructure: sandboxes and agent harnesses")
                .grouped("Platform"),
            Nav::new("Production", "/production")
                .described("What is running in production, to read rather than change")
                .grouped("Platform"),
        ],
        settings: vec![
            Setting::new("drift-minutes", "How often to look for drift", SettingKind::Number,)
                .hinted("A resource is checked against its vendor at most this often, in minutes.")
                .defaulting(json!(15))
                .between(1.0, 1440.0),
            Setting::secret("linode-token", "Linode API token").grouped("Linode"),
            Setting::new("linode-api", "Linode API URL", SettingKind::Url)
                .hinted("Leave it empty for Linode's own.")
                .grouped("Linode"),
            Setting::text("linode-image", "Linode machine image")
                .hinted("What a machine runs, such as linode/debian12. Empty uses Debian 12.")
                .grouped("Linode"),
            Setting::text("linode-ssh-key", "Linode SSH key")
                .hinted(
                    "A public key put on every machine DOC stands up here. Without one nobody \
                     can reach the machine over SSH: DOC sets a password it then forgets.",
                )
                .grouped("Linode"),
            Setting::text("aws-access-key-id", "AWS access key ID").grouped("AWS"),
            Setting::secret("aws-secret-access-key", "AWS secret access key").grouped("AWS"),
            Setting::new("aws-endpoint", "AWS endpoint", SettingKind::Url)
                .hinted("For an AWS-compatible service; empty uses AWS itself.")
                .grouped("AWS"),
            Setting::new("aws-ec2-endpoint", "AWS EC2 endpoint", SettingKind::Url)
                .hinted("For an EC2-compatible service; empty uses the region's own.")
                .grouped("AWS"),
            Setting::text("aws-image", "AWS machine image")
                .hinted(
                    "The AMI a machine runs. Empty uses Amazon Linux 2023, which EC2 looks up \
                     for each region itself.",
                )
                .grouped("AWS"),
            Setting::text("aws-key-pair", "AWS key pair")
                .hinted("The EC2 key pair put on every machine DOC stands up here.")
                .grouped("AWS"),
            Setting::text("aws-subnet", "AWS subnet")
                .hinted(
                    "Where a machine goes. Empty uses the account's default VPC, which gives \
                     each machine a public address; a subnet named here has to do the same for \
                     the machine to be reachable.",
                )
                .grouped("AWS"),
            Setting::text("aws-security-group", "AWS security group")
                .hinted("The security group every machine joins.")
                .grouped("AWS"),
            Setting::text("gcp-project", "Google Cloud project").grouped("Google Cloud"),
            Setting::secret("gcp-key", "Google Cloud service account key")
                .hinted("The JSON key, as the console gives it.")
                .grouped("Google Cloud"),
            Setting::new("gcp-api", "Google Cloud API URL", SettingKind::Url)
                .grouped("Google Cloud"),
            Setting::new("gcp-compute-api", "Compute Engine API URL", SettingKind::Url)
                .grouped("Google Cloud"),
            Setting::text("gcp-image", "Google Cloud machine image")
                .hinted("What a machine runs. Empty uses the Debian 12 image family.")
                .grouped("Google Cloud"),
            Setting::text("gcp-network", "Google Cloud network")
                .hinted("Which network a machine joins. Empty uses global/networks/default.")
                .grouped("Google Cloud"),
            Setting::text("gcp-ssh-key", "Google Cloud SSH key")
                .hinted("A key in Compute Engine's own form, user:ssh-ed25519 AAAA... user.")
                .grouped("Google Cloud"),
            Setting::text("azure-tenant", "Azure tenant").grouped("Azure"),
            Setting::text("azure-client-id", "Azure client ID").grouped("Azure"),
            Setting::secret("azure-client-secret", "Azure client secret").grouped("Azure"),
            Setting::text("azure-subscription", "Azure subscription").grouped("Azure"),
            Setting::text("azure-resource-group", "Azure resource group").grouped("Azure"),
            Setting::new("azure-login", "Azure login URL", SettingKind::Url).grouped("Azure"),
            Setting::new("azure-api", "Azure API URL", SettingKind::Url).grouped("Azure"),
            Setting::text("azure-image", "Azure machine image")
                .hinted("As publisher:offer:sku:version. Empty uses Ubuntu 24.04 LTS.",)
                .grouped("Azure"),
            Setting::text("azure-network", "Azure network")
                .hinted(
                    "The virtual network machines join, made on first use with one subnet. DOC \
                     never writes over a network it did not make.",
                )
                .grouped("Azure"),
            Setting::text("azure-admin-username", "Azure administrator")
                .hinted("The account made on every machine. Empty uses docadmin.")
                .grouped("Azure"),
            Setting::text("azure-ssh-key", "Azure SSH key")
                .hinted(
                    "A public key put on every machine DOC stands up here. Without one nobody \
                     can reach the machine over SSH: DOC sets a password it then forgets.",
                )
                .grouped("Azure"),
        ],
        dashboard: vec![
            DashboardItem::new("expiring", "Your cloud resources", "/dashboard").described(
                "What you and your teams asked DOC for, soonest to expire first, so nothing goes \
                 that is still needed.",
            ),
        ],
        resource_panels: ["team", "service"]
            .into_iter()
            .map(|kind| ResourcePanel::new(kind, "Cloud resources", "/panel"))
            .collect(),
        schedules: vec![Schedule::new(
            "lifecycle",
            "* * * * *",
            "Warns before resources expire, tears down those that have, and notices drift"
        )],
        data: store::declaration(),
        ..Manifest::default()
    }
);
