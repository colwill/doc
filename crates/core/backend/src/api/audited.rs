//! Endpoint tests that every call changing the platform's state leaves an entry in the audit log:
//! sign-in and out, tokens, service accounts, users, tasks, schedules and every plugin control.

use chrono::Utc;
use http::{Method, Request};
use serde_json::{Value, json};

use crate::testing::{
    ADMIN, Host, identity_provider, plugin_host, plugin_host_with, send, sign_in,
};

async fn host() -> Host {
    let mut config = plugin_host().state.config.as_ref().clone();
    config.plugins.ids.push("github".into());
    let capabilities = vec![doc_plugin_protocol::Capability::IdentityProvider];
    config.plugins.capabilities.insert("github".into(), capabilities);
    let host = plugin_host_with(config);
    identity_provider(&host, "github").await;
    let manifest = doc_plugin_protocol::Manifest {
        id: "hello".into(),
        version: "1.0.0".into(),
        classification: doc_plugin_protocol::Classification::Synchronous,
        ..Default::default()
    };
    let request = doc_plugin_protocol::RegisterRequest {
        manifest,
        address: "plugin-hello:4440".into(),
        binary_sha256: "a".repeat(64),
        started_at: None,
    };
    crate::plugins::register(&host.state, &host.as_plugin("hello"), request)
        .await
        .expect("registered");
    host.settle("hello").await;
    host
}

async fn entries(host: &Host) -> usize {
    host.identity.audit_entries().len()
}

/// Makes the call and returns its answer, failing unless it succeeded and was audited.
async fn audited(
    host: &Host,
    method: Method,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> Value {
    let before = entries(host).await;
    let mut request = Request::builder().method(method.clone()).uri(path);
    if let Some(token) = token {
        request = request.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = request
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .expect("request");
    let (status, answer, _) = send(&host.app, request).await;
    assert!(status.is_success(), "{method} {path}: {status} {answer}");
    assert!(entries(host).await > before, "{method} {path} was not audited");
    answer
}

#[tokio::test]
async fn every_call_that_changes_state_is_audited() {
    let host = host().await;
    let before = entries(&host).await;
    let signed = sign_in(&host, "github", "583231", "dev").await;
    let session = signed.session_token.expose().clone();
    assert!(entries(&host).await > before, "a sign-in was not audited");

    let token =
        audited(&host, Method::POST, "/api/v1/tokens", Some(&session), json!({ "name": "laptop" }))
            .await;
    let token = token["created"]["id"].as_str().expect("token id").to_string();
    audited(&host, Method::DELETE, &format!("/api/v1/tokens/{token}"), Some(&session), json!(null))
        .await;

    let account = audited(
        &host,
        Method::POST,
        "/api/v1/service-accounts",
        Some(ADMIN),
        json!({ "name": "deployer" }),
    )
    .await;
    let account = account["id"].as_str().expect("account id").to_string();
    let path = format!("/api/v1/service-accounts/{account}/tokens");
    let issued = audited(&host, Method::POST, &path, Some(ADMIN), json!({ "name": "ci" })).await;
    let issued = issued["created"]["id"].as_str().expect("token id").to_string();
    audited(&host, Method::DELETE, &format!("{path}/{issued}"), Some(ADMIN), json!(null)).await;
    let path = format!("/api/v1/service-accounts/{account}");
    audited(&host, Method::PATCH, &path, Some(ADMIN), json!({ "disabled": true })).await;

    let user = host.identity.add_user("mallory");
    let path = format!("/api/v1/users/{}", user.id);
    audited(&host, Method::PATCH, &path, Some(ADMIN), json!({ "disabled": true })).await;

    let task =
        audited(&host, Method::POST, "/api/v1/tasks", Some(ADMIN), json!({ "kind": "core.sleep" }))
            .await;
    let path = format!("/api/v1/tasks/{}/cancel", task["id"].as_str().expect("task id"));
    audited(&host, Method::POST, &path, Some(ADMIN), json!(null)).await;
    audited(
        &host,
        Method::PATCH,
        "/api/v1/task-kinds/core.sleep",
        Some(ADMIN),
        json!({ "paused": true }),
    )
    .await;
    let next = Utc::now() + chrono::Duration::hours(1);
    host.state
        .repos
        .cron
        .upsert("core.prune-status", "17 3 * * *", None, next)
        .await
        .expect("scheduled");
    audited(
        &host,
        Method::PATCH,
        "/api/v1/cron/core.prune-status",
        Some(ADMIN),
        json!({ "paused": true }),
    )
    .await;

    audited(
        &host,
        Method::POST,
        "/api/v1/plugins/hello/run",
        Some(ADMIN),
        json!({ "payload": {} }),
    )
    .await;
    let made = audited(
        &host,
        Method::POST,
        "/api/v1/plugins/hello/tokens",
        Some(ADMIN),
        json!({ "name": "spare" }),
    )
    .await;
    let path = format!(
        "/api/v1/plugins/hello/tokens/{}",
        made["created"]["id"].as_str().expect("token id")
    );
    audited(&host, Method::DELETE, &path, Some(ADMIN), json!(null)).await;
    audited(
        &host,
        Method::POST,
        "/api/v1/plugins/hello/state",
        Some(ADMIN),
        json!({ "state": "cancelled" }),
    )
    .await;
    audited(
        &host,
        Method::POST,
        "/api/v1/plugins/hello/state",
        Some(ADMIN),
        json!({ "state": "running" }),
    )
    .await;
    audited(&host, Method::POST, "/api/v1/plugins/hello/cancel", Some(ADMIN), json!(null)).await;
    audited(&host, Method::POST, "/api/v1/plugins/hello/reload", Some(ADMIN), json!(null)).await;
    host.settle("hello").await;
    audited(&host, Method::POST, "/api/v1/plugins/hello/unload", Some(ADMIN), json!(null)).await;
    audited(&host, Method::POST, "/api/v1/auth/logout", Some(&session), json!(null)).await;
}
