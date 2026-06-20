use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use doc_plugin_sdk::{
    Aggregate, Backend, Classification, Collection, CustomPermission, Declaration, Event, Export,
    Feature, Field, Manifest, Measure, Nav, Order, Plugin, PluginError, Query, Request,
    ResourcePanel, Response, RunInput, RunOutput, Setting, SettingKind, Settings, SettingsVerdict,
};
use serde::Deserialize;
use serde_json::{Value, json};

// One classification per feature. `panic-on-load` and `hang-on-load` sit on top of whichever is
// chosen, so synchronous is what you get when no classification feature is named at all.
#[cfg(feature = "long-running")]
const CLASSIFICATION: Classification = Classification::LongRunning;
#[cfg(all(feature = "one-shot", not(feature = "long-running")))]
const CLASSIFICATION: Classification = Classification::OneShot;
#[cfg(all(feature = "task", not(any(feature = "long-running", feature = "one-shot"))))]
const CLASSIFICATION: Classification = Classification::Async;
#[cfg(not(any(feature = "long-running", feature = "one-shot", feature = "task")))]
const CLASSIFICATION: Classification = Classification::Synchronous;

/// The custom permission this plugin declares and checks for itself; core never checks it.
const GREETINGS: &str = "greetings";
/// What the platform configures this plugin with (ADR-0007). The page, the checking and the
/// storing are all core's; this is the whole of the plugin's side of it.
const WORDING: &str = "greeting";
const LOUD: &str = "loud";
/// A secret setting, which core keeps encrypted and hands back to this plugin alone. It opens
/// nothing — it is here so that the way DOC keeps a credential can be seen working.
const TOKEN: &str = "api-token";
/// A feature: a named switch the Features tab offers.
const PANEL: &str = "resource-panel";
const RECENT: u32 = 5;

/// One stored greeting. `mood` exists only in the builds that declare it.
#[derive(Debug, Deserialize)]
struct Greeting {
    id: String,
    name: String,
    at: String,
    #[serde(default)]
    mood: Option<String>,
}

/// The data this version keeps: its greetings, and what the Knowledge Base may read of them.
fn declaration() -> Declaration {
    let greetings = Collection::new()
        .field("id", Field::uuid().key())
        .field("name", Field::text().required().max(200.0))
        .field("by", Field::reference("core.users"))
        .field("at", Field::timestamp().required().default(json!("now")))
        .index(&["by", "at"])
        .index(&["at"])
        .search(&["name"])
        .export(Export::to(&["kb"]).fields(&["id", "name", "at"]));
    #[cfg(feature = "mood")]
    let greetings = greetings.field("mood", Field::text().max(40.0));
    #[cfg(all(feature = "mood-deprecated", not(feature = "mood")))]
    let greetings = greetings.field("mood", Field::text().max(40.0).deprecated());
    Declaration::default().collection("greetings", greetings)
}

#[derive(Default)]
struct Hello {
    greeted: AtomicU64,
    cancelled: AtomicBool,
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// What this plugin greets people with, as it is configured now. Reading a setting is free: the
/// SDK holds what core last gave it.
fn wording(backend: &Backend) -> String {
    let settings = backend.settings();
    let said = settings.text(WORDING);
    match settings.boolean(LOUD) {
        true => said.to_uppercase(),
        false => said,
    }
}

/// Stores a greeting from whoever the call is for, and returns it as stored.
async fn greet(backend: &Backend, name: &str, mood: Option<&str>) -> Result<Greeting, PluginError> {
    let by = backend
        .caller()
        .filter(|caller| caller.kind == "user")
        .and_then(|caller| caller.id.clone());
    let mut values = json!({ "name": name, "by": by });
    if cfg!(feature = "mood") {
        values["mood"] = json!(mood);
    }
    backend.insert("greetings", values).await
}

/// How many greetings are stored, and the most recent of them.
async fn stored(backend: &Backend) -> Result<(u64, Vec<Greeting>), PluginError> {
    let counted = Aggregate::new("greetings").measure("n", Measure::Count("*".into()));
    let groups = backend.aggregate(counted).await?;
    let count = groups.first().and_then(|group| group.get("n")?.as_u64()).unwrap_or(0);
    let recent = Query::new("greetings").order(Order::desc("at")).limit(RECENT);
    Ok((count, backend.query(recent).await?.records))
}

fn failed(err: &PluginError) -> Response {
    match err.problem() {
        Some((status, kind)) if status < 500 => Response::problem(status, &kind, &err.detail()),
        _ => Response::problem(503, "unavailable", &err.detail()),
    }
}

fn forbidden() -> Response {
    Response::problem(403, "forbidden", &format!("needs plugin:hello:pluginuser:{GREETINGS}:rw"))
}

#[async_trait]
impl Plugin for Hello {
    async fn load(
        &mut self,
        backend: &Backend,
        previous: Option<Value>,
    ) -> Result<(), PluginError> {
        #[cfg(feature = "panic-on-load")]
        panic!("this build of hello panics in load, on purpose");

        #[cfg(feature = "hang-on-load")]
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }

        #[cfg(not(feature = "hang-on-load"))]
        {
            // A hot reload hands over whatever the last `unload` returned.
            let carried = previous
                .as_ref()
                .and_then(|state| state.get("greeted"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.greeted.store(carried, Ordering::Release);
            self.cancelled.store(false, Ordering::Release);
            #[cfg(feature = "tour")]
            tour::run(backend).await;
            #[cfg(feature = "trespass")]
            trespass::run(backend).await;
            // Never the secret itself: only whether this plugin was given one.
            let holds_token = backend.settings().secret(TOKEN).is_some();
            tracing::info!(
                carried,
                version = backend.version(),
                greeting = %wording(backend),
                holds_token,
                "hello loaded"
            );
            Ok(())
        }
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        let greeted = self.greeted.load(Ordering::Acquire);
        tracing::info!(greeted, "hello unloading");
        Ok(Some(json!({ "greeted": greeted })))
    }

    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        let name = input.payload.get("name").and_then(Value::as_str).unwrap_or("world");
        if CLASSIFICATION == Classification::LongRunning {
            // The backend starts a fresh run after every load and every resume.
            self.cancelled.store(false, Ordering::Release);
            while !self.cancelled.load(Ordering::Acquire) {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            return Ok(RunOutput { payload: json!({ "stopped": true }) });
        }
        self.greeted.fetch_add(1, Ordering::AcqRel);
        greet(backend, name, None).await?;
        let _ = backend.publish("plugin.hello.greeted", json!({ "name": name })).await;
        Ok(RunOutput { payload: json!({ "greeting": format!("{}, {name}", wording(backend)) }) })
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        self.cancelled.store(true, Ordering::Release);
        Ok(())
    }

    /// What this plugin thinks of settings somebody has typed and nothing has stored yet. A real
    /// plugin tries a credential here; this one has none, so it objects to the one greeting it
    /// will not say, which is enough to show where the message lands.
    async fn settings_check(&self, _backend: &Backend, proposed: &Settings) -> SettingsVerdict {
        match proposed.text(WORDING).to_lowercase().contains("goodbye") {
            true => SettingsVerdict::wrong(WORDING, "hello does not say goodbye"),
            false => SettingsVerdict::saying("hello will greet people with that"),
        }
    }

    fn error(&self) -> Option<String> {
        None
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "ui" | "ui/") => match stored(backend).await {
                Ok((count, recent)) => card(&wording(backend), count, &recent, None),
                Err(err) => failed(&err),
            },
            // Core let the request in; greeting is the plugin's own permission to check.
            ("POST", "ui/greet") => {
                if !backend.allows(GREETINGS, true) {
                    return forbidden();
                }
                let who = backend.caller().and_then(|caller| caller.label.clone());
                let greeting = match greet(backend, who.as_deref().unwrap_or("world"), None).await {
                    Ok(greeting) => greeting,
                    Err(err) => return failed(&err),
                };
                self.greeted.fetch_add(1, Ordering::AcqRel);
                match stored(backend).await {
                    Ok((count, recent)) => {
                        card(&wording(backend), count, &recent, Some(&greeting.name))
                    }
                    Err(err) => failed(&err),
                }
            }
            // Its panel on each service's page in Resource Definitions, which loads it by HTMX.
            // A plugin checks its own features, which is what makes a switch mean anything.
            ("GET", "ui/panels/service") if !backend.feature(PANEL) => {
                Response::html(String::new())
            }
            ("GET", "ui/panels/service") => match stored(backend).await {
                Ok((count, _)) => Response::html(format!(
                    "<p>{count} greetings so far, across every service.</p>"
                )),
                Err(err) => failed(&err),
            },
            ("GET", "api/greetings") => {
                let search = request.query.split('&').find_map(|pair| pair.strip_prefix("q="));
                let listed = match search {
                    Some(text) => {
                        let found = Query::new("greetings").search(&text.replace('+', " ")).limit(20);
                        backend.query::<Value>(found).await.map(|page| json!({ "found": page.records }))
                    }
                    None => stored(backend).await.map(|(count, recent)| {
                        let recent: Vec<Value> = recent
                            .iter()
                            .map(|greeting| {
                                json!({ "id": greeting.id, "name": greeting.name, "at": greeting.at, "mood": greeting.mood })
                            })
                            .collect();
                        json!({ "stored": count, "recent": recent })
                    }),
                };
                match listed {
                    Ok(mut answer) => {
                        answer["greeted"] = json!(self.greeted.load(Ordering::Acquire));
                        Response::json(&answer)
                    }
                    Err(err) => failed(&err),
                }
            }
            ("POST", "api/greetings") => {
                // The plugin's own permission, checked by the plugin. A platform admin passes.
                if !backend.allows(GREETINGS, true) {
                    return forbidden();
                }
                let body = request.json::<Value>().unwrap_or_default();
                let name = body.get("name").and_then(Value::as_str).unwrap_or("world").to_string();
                let mood = body.get("mood").and_then(Value::as_str);
                let greeting = match greet(backend, &name, mood).await {
                    Ok(greeting) => greeting,
                    Err(err) => return failed(&err),
                };
                let greeted = self.greeted.fetch_add(1, Ordering::AcqRel) + 1;
                Response::json(&json!({
                    "greeting": format!("hello, {}", greeting.name),
                    "id": greeting.id,
                    "mood": greeting.mood,
                    "greeted": greeted,
                }))
            }
            // Fails this one request and nothing else: the plugin stays `running` (T22).
            ("GET", "api/panic") => panic!("hello panics in this route, on purpose"),
            _ => Response::not_found(),
        }
    }

    async fn on_event(&self, _backend: &Backend, event: Event) -> Result<(), PluginError> {
        tracing::info!(topic = %event.topic, id = %event.id, "hello saw an event");
        Ok(())
    }
}

/// The card the UI shows, with the Greet button that posts back through HTMX.
fn card(wording: &str, count: u64, recent: &[Greeting], greeted: Option<&str>) -> Response {
    let said = greeted
        .map(|name| format!("<p><strong>{}, {}</strong></p>", escape(wording), escape(name)))
        .unwrap_or_default();
    let items: String = recent
        .iter()
        .map(|greeting| {
            format!(
                "<li>{} <span class=\"doc-mono\">{}</span></li>",
                escape(&greeting.name),
                escape(&greeting.at)
            )
        })
        .collect();
    let list = match items.is_empty() {
        true => String::new(),
        false => format!("<ul class=\"doc-list\">{items}</ul>"),
    };
    Response::html(format!(
        "<div id=\"hello-card\" class=\"doc-card\">\
           <h3 class=\"doc-card__heading\">Hello</h3>\
           <div class=\"doc-card__content\">{said}<p>{count} greetings so far.</p>{list}\
             <form hx-post=\"/p/hello/greet\" hx-target=\"#hello-card\" hx-swap=\"outerHTML\">\
               <button class=\"doc-button doc-button--small\" type=\"submit\">Greet</button>\
             </form>\
           </div>\
         </div>"
    ))
}

#[cfg(feature = "tour")]
mod tour {
    //! Every call in §5's backend API, made from inside `load` with the context the backend gave
    //! it. Each answer is logged, so a run shows the whole API working against a real backend.

    use std::future::Future;
    use std::time::Duration;

    use doc_plugin_sdk::{Aggregate, Backend, Bucket, Measure, PluginError, Query};
    use serde_json::{Value, json};

    async fn step<F: Future<Output = Result<Value, PluginError>>>(name: &str, call: F) {
        match call.await {
            Ok(answer) => tracing::info!(step = name, %answer, "tour: answered"),
            Err(err) => tracing::error!(step = name, %err, "tour: FAILED"),
        }
    }

    pub async fn run(backend: &Backend) {
        step("insert, then search its own collection", async {
            let stored: Value = backend.insert("greetings", json!({ "name": "tour" })).await?;
            let search = Query::new("greetings").search("tour").limit(3);
            let found = backend.query::<Value>(search).await?;
            Ok(json!({ "stored": stored, "found": found.records }))
        })
        .await;
        step("aggregate greetings by day", async {
            let daily = Aggregate::new("greetings")
                .bucket("at", Bucket::Day)
                .measure("greetings", Measure::Count("*".into()));
            Ok(json!(backend.aggregate(daily).await?))
        })
        .await;
        step("read core.plugins and its own tasks", async {
            let plugin: Option<Value> = backend.get("core.plugins", "hello").await?;
            let tasks = backend.query::<Value>(Query::new("core.tasks").limit(5)).await?;
            Ok(json!({ "plugin": plugin, "tasks": tasks.records.len() }))
        })
        .await;
        step("publish to its own topic", async {
            Ok(json!(backend.publish("plugin.hello.toured", json!({ "at": "load" })).await?))
        })
        .await;
        step("service bus request as itself", async {
            backend.request("core.ping", "ping", json!({}), Duration::from_secs(5)).await
        })
        .await;
        step("cache set, get and compare-and-set", async {
            backend.cache_set("tour", json!({ "n": 1 }), Some(Duration::from_secs(600))).await?;
            let read = backend.cache_get("tour").await?;
            let first = backend.cache_compare_and_set("once", json!(true), None, None).await?;
            let again = backend.cache_compare_and_set("once", json!(true), None, None).await?;
            Ok(json!({ "read": read, "first_claim": first, "second_claim": again }))
        })
        .await;
        step("queue a background task", async {
            Ok(json!(backend.task(json!({ "name": "tour" })).await?))
        })
        .await;
        step("state set then get", async {
            backend.state_set("checkpoint", json!({ "loaded": true })).await?;
            Ok(json!(backend.state_get("checkpoint").await?))
        })
        .await;
        step("audit", async {
            backend.audit("toured", Some("hello"), json!({ "steps": 9 })).await?;
            Ok(json!("recorded"))
        })
        .await;
    }
}

#[cfg(feature = "trespass")]
mod trespass {
    //! Everything T21, T22 and T62 say a plugin must not be able to do, tried from inside `load`.
    //! Every line a run logs should say `refused`; one that says `ALLOWED` is a hole in the
    //! isolation.

    use std::future::Future;
    use std::time::Duration;

    use doc_plugin_sdk::protocol::calls::IdentityRequest;
    use doc_plugin_sdk::{Backend, PluginError, Query};
    use serde_json::{Value, json};

    async fn refused<T: std::fmt::Debug, F: Future<Output = Result<T, PluginError>>>(
        what: &str,
        call: F,
    ) {
        match call.await {
            Ok(answer) => tracing::error!(attempt = what, ?answer, "trespass: ALLOWED"),
            Err(err) => tracing::info!(attempt = what, %err, "trespass: refused"),
        }
    }

    pub async fn run(backend: &Backend) {
        refused(
            "write another plugin's collection",
            backend.insert::<Value>("kb.documents", json!({ "title": "planted" })),
        )
        .await;
        refused(
            "read a collection another plugin has not exported",
            backend.query::<Value>(Query::new("rbac.assignments")),
        )
        .await;
        refused(
            "write a core collection",
            backend.update::<Value>("core.plugins", "hello", json!({ "state": "running" }), None),
        )
        .await;
        match backend.query::<Value>(Query::new("core.tasks").limit(1000)).await {
            Ok(page) => {
                let payloads: Vec<&Value> =
                    page.records.iter().filter_map(|task| task.get("payload")).collect();
                tracing::info!(
                    tasks = page.records.len(),
                    ?payloads,
                    "trespass: core.tasks answered with what should be only hello's own tasks"
                );
            }
            Err(err) => tracing::error!(%err, "trespass: core.tasks could not be read"),
        }
        refused("publish as the platform", backend.publish("platform.backend.started", json!({})))
            .await;
        refused("publish as another plugin", backend.publish("plugin.kb.imported", json!({})))
            .await;
        refused(
            "reach a plugin's internal route over the Service Bus",
            backend.request("plugin.hello", "permissions", json!({}), Duration::from_secs(5)),
        )
        .await;
        refused(
            "sign a user in",
            backend.identity(IdentityRequest {
                provider: "hello".into(),
                external_id: "1".into(),
                login: "mallory".into(),
                ..IdentityRequest::default()
            }),
        )
        .await;
        // The context this `load` was given, kept past the end of the call it was issued for.
        let kept = backend.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            refused(
                "use load's context after load returned",
                kept.request("core.ping", "ping", json!({}), Duration::from_secs(5)),
            )
            .await;
        });
    }
}

doc_plugin_sdk::main!(
    Hello,
    Manifest {
        id: "hello".into(),
        classification: CLASSIFICATION,
        custom_permissions: vec![
            CustomPermission::user(GREETINGS)
                .describes("Greeting people, and storing the greeting"),
            CustomPermission::service(GREETINGS).describes("The same, for a service account"),
        ],
        settings: vec![
            Setting::text(WORDING, "Greeting")
                .hinted("What this plugin greets people with.")
                .defaulting(json!("hello"))
                .required(),
            Setting::new(LOUD, "Shout it", SettingKind::Boolean).hinted("Greets in capitals."),
            Setting::secret(TOKEN, "API token")
                .hinted("Opens nothing. It is here to show how DOC keeps a secret.")
                .grouped("Credentials"),
        ],
        features: vec![
            Feature::new(PANEL, "Service panel", "Shows greetings on each service's page").on(),
        ],
        nav: vec![
            Nav::new("Hello", "/").described("An example plugin that greets you").grouped("Help")
        ],
        resource_panels: vec![ResourcePanel::new("service", "Greetings", "/panels/service")],
        // Its own completed background runs too, which is how an async plugin hears how they went.
        subscriptions: vec!["platform.plugin.>".into(), "plugin.hello.run.completed".into()],
        data: declaration(),
        ..Manifest::default()
    }
);
