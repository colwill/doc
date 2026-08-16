//! The Knowledge Base's faux data: for each service, a space holding the docs of its stand-in
//! repository `faux/<service>` — a README, its architecture, its runbook and the decisions made
//! about it — read by a GitHub source that syncs well or badly as the service does, with a chart
//! and a table attached; and an engineering handbook uploaded by hand, linked to every service.
//! What the pages say follows the service's profile, so ledger's runbook has the incidents its
//! reliability shows. The KB renders it all with its own code, so it looks as real docs do.

use base64::Engine;
use chrono::{Duration, Utc};
use serde_json::{Value, json};

use crate::dice::Dice;
use crate::estate::{self, Profile};
use crate::png;
use crate::settings::Config;

/// A steady ID for something made up, as a UUID, from the estate's seed and what it is.
fn id(config: &Config, key: &str) -> String {
    let mut dice = Dice::seeded(config.seed, key, -7);
    let (a, b) = (dice.next(), dice.next());
    let hex = format!("{a:016x}{b:016x}");
    format!("{}-{}-7{}-8{}-{}", &hex[..8], &hex[8..12], &hex[13..16], &hex[17..20], &hex[20..32])
}

/// What a service does, as its README opens.
fn purpose(service: &str) -> &'static str {
    match service {
        "card-gateway" => "authorises, captures and refunds card payments for every checkout",
        "developer-portal" => {
            "is where engineers find services, their docs and the templates that make new ones"
        }
        "chargebacks" => {
            "handles the disputes customers raise with their card issuer, from notice to outcome"
        }
        "ledger" => "keeps the double-entry record of every payment, refund and payout",
        _ => "is one of the platform's services",
    }
}

fn team(service: &str) -> &'static str {
    match service {
        "card-gateway" | "chargebacks" | "ledger" => "team:payments",
        "developer-portal" => "team:developer-experience",
        _ => "team:platform",
    }
}

fn status(profile: Profile) -> &'static str {
    match profile {
        Profile::Thriving => {
            "It deploys several times a day behind feature flags, and on call has been quiet for \
             weeks."
        }
        Profile::Steady => {
            "It deploys most days. Alerts are rare and are answered within the hour."
        }
        Profile::Struggling => {
            "**Known issues:** deployments are held while a flaky integration suite is fixed, and \
             latency rises at the end of each month."
        }
        Profile::Failing => {
            "**Known issues:** it is running past the end of life of its runtime, misses its \
             availability objective, and its last two releases were rolled back. See the runbook."
        }
    }
}

fn page(path: &str, title: &str, markdown: &str, source: &str, repository: Option<&str>) -> Value {
    let url =
        repository.map(|repository| format!("https://github.com/{repository}/blob/main/{path}"));
    json!({ "path": path, "title": title, "markdown": markdown, "source": source, "source_url": url })
}

fn file(page: &str, name: &str, content_type: &str, bytes: &[u8]) -> Value {
    json!({
        "page": page,
        "name": name,
        "content_type": content_type,
        "base64": base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

/// One service's space: its repository's docs, the GitHub source that reads them, and the files.
fn service_space(config: &Config, service: &str, title: &str) -> Value {
    let profile = estate::profile(config, service);
    let repository = estate::repository(service);
    let key = format!("faux-{}", service.to_ascii_lowercase());
    let source = id(config, &format!("source:{key}"));
    let now = Utc::now();
    let (state, error, synced) = match profile {
        Profile::Thriving | Profile::Steady => ("succeeded", None, now - Duration::minutes(12)),
        Profile::Struggling => ("waiting", None, now - Duration::hours(7)),
        Profile::Failing => (
            "failed",
            Some("GitHub answered 404: the repository was renamed; update the source"),
            now - Duration::days(3),
        ),
    };
    let products = estate::products(profile);
    let runs = products.join(", ");
    // The README says which service it is the docs of, as a real one's front matter would.
    let readme = format!(
        "---\nresources: [service:{service}]\n---\n# {title}\n\n{title} {}.\n\n{}\n\n![Requests over the last week](docs/traffic.png)\n\n\
         ## Where to go next\n\n- [How it is built](docs/architecture.md)\n- [When it goes \
         wrong](docs/runbook.md)\n- [Decisions made about it](docs/adr/0001-record-decisions.md)\n\n\
         It runs {runs}, and belongs to {}.\n",
        purpose(service),
        status(profile),
        team(service).trim_start_matches("team:"),
    );
    let architecture = format!(
        "# How {title} is built\n\n{title} is a stateless service in front of a {} database, \
         deployed as three replicas behind the internal load balancer. It calls the services in \
         [its dependencies](dependencies.csv).\n\n| Part | Runs on | Scales with |\n|---|---|---|\n\
         | API | {} | Requests |\n| Workers | {} | Queue depth |\n| Database | {} | Storage |\n\n\
         See the [runbook](runbook.md) for what to do when a part is unhealthy.\n",
        products.iter().find(|p| p.starts_with("postgresql")).unwrap_or(&"PostgreSQL"),
        products.first().unwrap_or(&"a container"),
        products.first().unwrap_or(&"a container"),
        products.iter().find(|p| p.starts_with("postgresql")).unwrap_or(&"PostgreSQL"),
    );
    let incidents = match profile {
        Profile::Thriving | Profile::Steady => "No incidents in the last 90 days.".to_string(),
        Profile::Struggling | Profile::Failing => {
            let mut dice = Dice::seeded(config.seed, &format!("incidents:{service}"), -2);
            let count = if profile == Profile::Failing { 4 } else { 2 };
            (0..count)
                .map(|n| {
                    let days = 5 + n * 11 + (dice.next() % 5) as i64;
                    let minutes = 20 + dice.next() % 160;
                    format!(
                        "- {}: down for {minutes} minutes after a deployment; rolled back.",
                        (now - Duration::days(days)).format("%-d %B %Y")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    };
    let runbook = format!(
        "# When {title} goes wrong\n\n## Alerts\n\n| Alert | Means | First step |\n|---|---|---|\n\
         | High error rate | More than 2% of requests fail | Check the last deployment, and roll it \
         back |\n| Slow responses | p95 over 800 ms | Look at the database's slow query log |\n\
         | Queue backing up | Workers are behind | Scale the workers up |\n\n## Recent incidents\n\n\
         {incidents}\n\nBack to the [README](../README.md).\n"
    );
    let adr = format!(
        "# 1. Record the decisions made about {title}\n\nWe keep a record of each decision that \
         shapes {title}, so the next person knows why it is the way it is. Each is numbered, \
         says what was decided and what follows from it, and is never rewritten: a later one \
         replaces it.\n\nThe next is [2. Choose the database](0002-choose-the-database.md).\n"
    );
    let database = format!(
        "# 2. Choose the database\n\n{title} keeps its data in {}, which the platform already \
         runs and backs up, rather than a store of its own.\n",
        products.iter().find(|p| p.starts_with("postgresql")).unwrap_or(&"PostgreSQL"),
    );
    let pages = vec![
        page("README.md", title, &readme, &source, Some(&repository)),
        page(
            "docs/architecture.md",
            &format!("How {title} is built"),
            &architecture,
            &source,
            Some(&repository),
        ),
        page(
            "docs/runbook.md",
            &format!("When {title} goes wrong"),
            &runbook,
            &source,
            Some(&repository),
        ),
        page(
            "docs/adr/0001-record-decisions.md",
            "1. Record decisions",
            &adr,
            &source,
            Some(&repository),
        ),
        page(
            "docs/adr/0002-choose-the-database.md",
            "2. Choose the database",
            &database,
            &source,
            Some(&repository),
        ),
    ];
    // A week of requests, as a share of the busiest day, falling away for a failing service.
    let mut dice = Dice::seeded(config.seed, &format!("traffic:{service}"), -3);
    let traffic: Vec<f64> = (0..7)
        .map(|day| {
            let base = 0.55 + 0.35 * dice.unit();
            match profile {
                Profile::Failing if day >= 5 => base * 0.3,
                Profile::Struggling if day == 4 => base * 0.6,
                _ => base,
            }
        })
        .collect();
    let ink = match profile {
        Profile::Thriving | Profile::Steady => (0, 94, 184),
        Profile::Struggling => (237, 139, 0),
        Profile::Failing => (212, 53, 28),
    };
    let dependencies = format!(
        "service,why\nledger,records every movement of money\nidentity,signs requests in\n{},\
         what it depends on most\n",
        match service {
            "ledger" => "card-gateway",
            _ => "ledger",
        }
    );
    let files = vec![
        file("README.md", "traffic.png", "image/png", &png::bars(&traffic, ink)),
        file("docs/architecture.md", "dependencies.csv", "text/csv", dependencies.as_bytes()),
    ];
    let source = json!({
        "id": source,
        "kind": "github",
        "space": key,
        "settings": { "repository": repository, "ref": "main", "path": "" },
        "schedule": null,
        "last_sync_at": synced,
        "last_state": state,
        "last_error": error,
        "managed": true,
    });
    json!({
        "key": key,
        "name": repository,
        "resource": format!("repository:{repository}"),
        "owners": [team(service)],
        "sources": [source],
        "pages": pages,
        "files": files,
    })
}

const HANDBOOK: &str = "faux-handbook";

/// The engineering handbook: uploaded rather than synced, and linked to every service.
fn handbook(config: &Config, services: &[(String, String)]) -> Value {
    let source = id(config, "source:faux-handbook");
    let named: Vec<String> = services.iter().map(|(name, _)| format!("service:{name}")).collect();
    let index = "# Engineering handbook\n\nHow we build and run services. Start with \
                 [being on call](on-call.md), then [incident reviews](incident-reviews.md) and \
                 [writing docs](writing-docs.md).\n"
        .to_string();
    let on_call = format!(
        "---\nresources: [{}]\n---\n# Being on call\n\nEach service has one person on call at a \
         time, for a week. Keep your laptop and the pager app with you, and answer within 15 \
         minutes. Page the service's team lead when an incident lasts longer than an hour.\n\n\
         The rota is in [the on-call sheet](on-call.csv).\n",
        named.join(", ")
    );
    let reviews = "# Incident reviews\n\nEvery incident that paged someone gets a review within \
                   a week: what happened, why, and what changes so it does not happen again. \
                   Reviews are blameless.\n"
        .to_string();
    let writing = "# Writing docs\n\nEvery repository has a README saying what the service does, \
                   how to run it and where its runbook is. Docs live beside the code they \
                   describe, and DOC reads them from there.\n"
        .to_string();
    let rota: String = std::iter::once("week,service,on call".to_string())
        .chain(services.iter().enumerate().map(|(n, (name, _))| {
            let week = (Utc::now() + Duration::weeks(n as i64)).format("%G-W%V");
            format!("{week},{name},engineer-{}", n + 1)
        }))
        .collect::<Vec<_>>()
        .join("\n");
    json!({
        "key": HANDBOOK,
        "name": "Engineering handbook",
        "resource": null,
        "owners": ["organisation:engineering"],
        "sources": [{
            "id": source,
            "kind": "upload",
            "space": HANDBOOK,
            "settings": {},
            "schedule": null,
            "last_sync_at": Utc::now() - Duration::days(9),
            "last_state": "succeeded",
            "last_error": null,
            "managed": false,
        }],
        "pages": [
            page("index.md", "Engineering handbook", &index, &source, None),
            page("on-call.md", "Being on call", &on_call, &source, None),
            page("incident-reviews.md", "Incident reviews", &reviews, &source, None),
            page("writing-docs.md", "Writing docs", &writing, &source, None),
        ],
        "files": [file("on-call.md", "on-call.csv", "text/csv", rota.as_bytes())],
    })
}

/// `docs?service=…`: a space for each service asked about, or for the made-up services where none
/// is, the handbook, and each service's linked spaces.
pub fn answer(config: &Config, asked: &[(String, String)]) -> Value {
    let services: Vec<(String, String)> = match asked.is_empty() {
        true => config.services.clone(),
        false => asked.to_vec(),
    };
    let mut spaces: Vec<Value> =
        services.iter().map(|(name, title)| service_space(config, name, title)).collect();
    spaces.push(handbook(config, &services));
    let links: serde_json::Map<String, Value> =
        services.iter().map(|(name, _)| (name.clone(), json!([HANDBOOK]))).collect();
    let listed: Vec<Value> =
        services.iter().map(|(name, title)| json!({ "name": name, "title": title })).collect();
    json!({ "services": listed, "spaces": spaces, "links": links })
}
