//! What the plugin is configured with: the vendor account in Secret Storage it reaches Grafana
//! through, which dashboards it shows and the services each is for, and the variable a service's
//! page sets. It holds no credential of its own: the account's is Secret Storage's.

use std::collections::BTreeMap;

use doc_plugin_sdk::{Setting, SettingKind, Settings};
use serde_json::json;

pub const ACCOUNT: &str = "account";
/// Where the Settings page asks which accounts can be chosen.
pub const ACCOUNT_CHOICES: &str = "settings/accounts";
pub const ADDRESS: &str = "address";
pub const DASHBOARDS: &str = "dashboards";
/// Where the Settings page asks which dashboards and services can be chosen.
pub const DASHBOARD_CHOICES: &str = "settings/dashboards";
pub const SERVICE_VARIABLE: &str = "service-variable";

/// The calls the plugin makes, as the rules an allowance needs to let them through.
pub const RULES: [&str; 5] = [
    "GET /api/search",
    "GET /api/dashboards/uid/*",
    "GET /api/library-elements/*",
    "GET /api/datasources",
    "POST /api/ds/query",
];

pub fn declared() -> Vec<Setting> {
    vec![
        Setting::new(ACCOUNT, "Vendor account", SettingKind::Choice)
            .choices_from(ACCOUNT_CHOICES)
            .required()
            .hinted(
                "The proxied account in Secret Storage that reaches Grafana. Only accounts with an \
                 allowance naming the grafana plugin are offered, and its rules need GET \
                 /api/search, GET /api/dashboards/uid/*, GET /api/library-elements/*, GET \
                 /api/datasources and POST /api/ds/query. Secret Storage records every call.",
            )
            .grouped("Connection"),
        Setting::new(ADDRESS, "Grafana's address for people", SettingKind::Url)
            .hinted(
                "Where people open Grafana in a browser, such as https://grafana.acme.example, for \
                 the links to a dashboard there. Empty uses the vendor account's own address.",
            )
            .grouped("Connection"),
        Setting::new(DASHBOARDS, "Dashboards shown", SettingKind::Map)
            .choices_from(DASHBOARD_CHOICES)
            .hinted(
                "The dashboards DOC shows, each with the services it is for, so it also shows on \
                 their pages in the Catalogue. With none chosen, every dashboard the account can \
                 see is listed, and none is on a service's page.",
            )
            .grouped("Dashboards"),
        Setting::new(SERVICE_VARIABLE, "Variable set to the service", SettingKind::Text)
            .defaulting(json!("service"))
            .matching("^[A-Za-z0-9_]{1,64}$")
            .hinted(
                "On a service's page, a dashboard variable with this name is set to the service's \
                 name, so one dashboard shows each service its own figures.",
            )
            .grouped("Dashboards"),
    ]
}

/// What the settings come to.
#[derive(Debug, Clone)]
pub struct Config {
    pub account: Option<String>,
    pub address: Option<String>,
    /// Each dashboard chosen, by its uid, with the services it is for.
    pub dashboards: BTreeMap<String, Vec<String>>,
    pub service_variable: String,
}

impl Config {
    pub fn read(settings: &Settings) -> Self {
        let tidy = |key: &str| {
            settings
                .some_text(key)
                .map(|text| text.trim().trim_end_matches('/').to_string())
                .filter(|text| !text.is_empty())
        };
        let mut dashboards: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (uid, services) in settings.map(DASHBOARDS) {
            let uid = uid.trim().to_string();
            if uid.is_empty() {
                continue;
            }
            let held = dashboards.entry(uid).or_default();
            for service in services.into_iter().map(|service| service.trim().to_string()) {
                if !service.is_empty() && !held.contains(&service) {
                    held.push(service);
                }
            }
        }
        Self {
            account: tidy(ACCOUNT),
            address: tidy(ADDRESS),
            dashboards,
            service_variable: tidy(SERVICE_VARIABLE).unwrap_or_else(|| "service".to_string()),
        }
    }

    /// The dashboards given a service, in the order they were chosen.
    pub fn for_service(&self, service: &str) -> Vec<String> {
        self.dashboards
            .iter()
            .filter(|(_, services)| services.iter().any(|named| named == service))
            .map(|(uid, _)| uid.clone())
            .collect()
    }
}
