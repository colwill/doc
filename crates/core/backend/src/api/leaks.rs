//! Endpoint tests that no answer carries a token's hash, a secret or a credential, looked for in
//! every read endpoint's answer by value and by field name, and that a plugin's registration token
//! and context tokens open nothing on the HTTP API beyond saying whose they are.

use http::StatusCode;
use serde_json::{Value, json};

use crate::identity::token_hash;
use crate::testing::{
    ADMIN, Host, get_as, identity_provider, plugin_host, plugin_host_with, post_json, sign_in,
};
const FIELDS: [&str; 5] = ["hash", "secret", "password", "credential", "private"];

fn names(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                found.push(key.to_ascii_lowercase());
                names(value, found);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| names(item, found)),
        _ => {}
    }
}

fn clean(path: &str, body: &Value, secrets: &[String]) {
    let text = body.to_string();
    for secret in secrets {
        assert!(!text.contains(secret.as_str()), "{path} carries a secret: {text}");
    }
    let mut found = Vec::new();
    names(body, &mut found);
    for name in found {
        assert!(!FIELDS.iter().any(|field| name.contains(field)), "{path} has a field {name}");
    }
}

async fn host() -> (Host, Vec<String>) {
    let mut config = plugin_host().state.config.as_ref().clone();
    config.plugins.ids.push("github".into());
    let capabilities = vec![doc_plugin_protocol::Capability::IdentityProvider];
    config.plugins.capabilities.insert("github".into(), capabilities);
    let host = plugin_host_with(config);
    identity_provider(&host, "github").await;
    let manifest = doc_plugin_protocol::Manifest {
        id: "hello".into(),
        version: "1.0.0".into(),
        ..Default::default()
    };
    let request = doc_plugin_protocol::RegisterRequest {
        manifest,
        address: "plugin-hello:4440".into(),
        binary_sha256: "a".repeat(64),
        started_at: None,
    };
    let registered = crate::plugins::register(&host.state, &host.as_plugin("hello"), request)
        .await
        .expect("registered");
    host.settle("hello").await;
    let instance = registered.secret.expose().clone();
    (host, vec![ADMIN.into(), "doc_reg_hello".into(), "doc_reg_rbac".into(), instance])
}

/// A creating call's answer: its secret once, under `token`, and nothing else secret.
fn created(path: &str, body: &Value, secrets: &mut Vec<String>) {
    let token = body["token"].as_str().unwrap_or_else(|| panic!("{path} gave no token: {body}"));
    let mut rest = body.clone();
    rest["token"] = json!("(the new token)");
    clean(path, &rest, secrets);
    secrets.push(token.to_string());
}

#[tokio::test]
async fn no_answer_carries_a_hash_a_secret_or_a_credential() {
    let (host, mut secrets) = host().await;
    let app = &host.app;

    let session = sign_in(&host, "github", "583231", "dev").await.session_token.expose().clone();
    secrets.push(session.clone());

    let (status, body, _) =
        post_json(app, "/api/v1/tokens", Some(&session), json!({ "name": "laptop" })).await;
    assert_eq!(status, StatusCode::CREATED);
    created("/api/v1/tokens", &body, &mut secrets);

    let (status, account, _) =
        post_json(app, "/api/v1/service-accounts", Some(ADMIN), json!({ "name": "deployer" }))
            .await;
    assert_eq!(status, StatusCode::CREATED, "{account}");
    let account = account["id"].as_str().expect("account id").to_string();
    let path = format!("/api/v1/service-accounts/{account}/tokens");
    let (status, body, _) = post_json(app, &path, Some(ADMIN), json!({ "name": "ci" })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    created(&path, &body, &mut secrets);

    let (status, body, _) =
        post_json(app, "/api/v1/plugins/hello/tokens", Some(ADMIN), json!({ "name": "spare" }))
            .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    created("/api/v1/plugins/hello/tokens", &body, &mut secrets);

    let hashes: Vec<String> =
        secrets.iter().map(|secret| hex::encode(token_hash(secret))).collect();
    secrets.extend(hashes);
    let reads = [
        "/api/v1/me".to_string(),
        "/api/v1/me/access".into(),
        "/api/v1/tokens".into(),
        "/api/v1/service-accounts".into(),
        format!("/api/v1/service-accounts/{account}"),
        format!("/api/v1/service-accounts/{account}/tokens"),
        "/api/v1/plugins".into(),
        "/api/v1/plugins/hello".into(),
        "/api/v1/plugins/hello/permissions".into(),
        "/api/v1/plugins/hello/tokens".into(),
        "/api/v1/audit?limit=500".into(),
        "/api/v1/status".into(),
        "/api/v1/status/history?hours=1".into(),
        "/api/v1/tasks".into(),
        "/api/v1/cron".into(),
        "/api/v1/auth/providers".into(),
    ];
    for token in [ADMIN, session.as_str()] {
        for path in &reads {
            let (status, body, _) = get_as(app, path, token).await;
            assert!(status.is_success() || token != ADMIN, "{path}: {status} {body}");
            if status.is_success() {
                clean(path, &body, &secrets);
            }
        }
    }
    let (status, audit, _) = get_as(app, "/api/v1/audit?limit=500", ADMIN).await;
    assert_eq!(status, StatusCode::OK);
    assert!(audit["entries"].as_array().is_some_and(|entries| entries.len() >= 4), "{audit}");
}

#[tokio::test]
async fn a_registration_token_opens_nothing_but_the_plugin_host() {
    let (host, _) = host().await;
    let app = &host.app;
    let (status, me, _) = get_as(app, "/api/v1/me", "doc_reg_hello").await;
    assert_eq!((status, me["kind"].as_str()), (StatusCode::OK, Some("plugin")));
    let refused = [
        ("/api/v1/tokens", json!({ "name": "spare key" })),
        ("/api/v1/service-accounts", json!({ "name": "hideout" })),
        ("/api/v1/tasks", json!({ "kind": "core.sleep" })),
        ("/api/v1/plugins/hello/run", json!({})),
        ("/api/v1/plugins/hello/tokens", json!({ "name": "spare" })),
    ];
    for (path, body) in refused {
        let (status, answer, _) = post_json(app, path, Some("doc_reg_hello"), body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {answer}");
    }
    for path in ["/api/v1/plugins", "/api/v1/audit", "/api/v1/plugins/hello/api/x"] {
        let (status, _, _) = get_as(app, path, "doc_reg_hello").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }
    let context = host.state.plugins.contexts.issue(
        "hello",
        host.as_plugin("hello"),
        std::time::Duration::from_secs(60),
    );
    let context = context.expect("issued");
    let (status, _, _) = get_as(app, "/api/v1/me", context.token()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a context token is no bearer token");
}
