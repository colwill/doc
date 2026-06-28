use std::time::Duration;

use doc_eventbus::{ConsumerGroup, TopicFilter};
use doc_plugin_protocol::data::{
    Aggregate, Bucket, Collection, DataRequest, Declaration, Export, Field, ListOf, Measure,
    OnDelete, Query,
};
use doc_plugin_protocol::{Manifest, PluginState, RegisterRequest};
use serde_json::{Value, json};

use super::store::DataStore;
use crate::config::Config;
use crate::identity::Principal;
use crate::plugins::api::{Refusal, data, tasks};
use crate::plugins::{self, RegisterError};
use crate::testing::{Host, plugin_host_with};

fn host() -> Host {
    let mut config = Config::default();
    config.plugins.ids = vec!["hello".into(), "rbac".into(), "kb".into()];
    config.limits.registrations_per_minute = 100;
    plugin_host_with(config)
}

fn greetings() -> Collection {
    Collection::new()
        .field("id", Field::uuid().key())
        .field("name", Field::text().required().max(20.0))
        .field("mood", Field::text().one_of(&["happy", "grumpy"]))
        .field("by", Field::reference("core.users"))
        .field("at", Field::timestamp().required().default(json!("now")))
        .field("count", Field::integer().min(0.0))
        .field("source", Field::text())
        .field("external_id", Field::text())
        .field("tags", Field::list(ListOf::Text))
        .index(&["at"])
        .index(&["mood", "at"])
        .unique(&["source", "external_id"])
        .search(&["name"])
}

fn declaration() -> Declaration {
    Declaration::default()
        .collection("greetings", greetings())
        .collection(
            "replies",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("greeting", Field::reference("greetings").on_delete(OnDelete::Cascade))
                .field("text", Field::text()),
        )
        .collection(
            "pins",
            Collection::new()
                .field("id", Field::text().key())
                .field("greeting", Field::reference("greetings")),
        )
        .collection(
            "likes",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("greeting", Field::reference("greetings").on_delete(OnDelete::Null)),
        )
}

fn manifest(id: &str, version: &str, data: Declaration) -> Manifest {
    Manifest { id: id.into(), version: version.into(), data, ..Manifest::default() }
}

async fn register(host: &Host, manifest: Manifest) -> Result<(), RegisterError> {
    let id = manifest.id.clone();
    let version = manifest.version.clone();
    let request = RegisterRequest {
        manifest,
        address: format!("plugin-{id}:4440"),
        binary_sha256: "a".repeat(64),
        started_at: None,
    };
    plugins::register(&host.state, &Principal::Plugin { id: id.clone() }, request).await?;
    for _ in 0..500 {
        tokio::time::sleep(Duration::from_millis(2)).await;
        let current = host.state.plugins.get(&id).await;
        let done = current.is_some_and(|entry| {
            entry.manifest.version == version && entry.state != PluginState::Loading
        });
        if done && !host.state.plugins.handing_over(&id) {
            return Ok(());
        }
    }
    panic!("{id} {version} never settled");
}

async fn hello(host: &Host) {
    register(host, manifest("hello", "1.0.0", declaration())).await.expect("registered");
    assert_eq!(host.state_of("hello").await, Some(PluginState::Running));
}

async fn ask(host: &Host, plugin: &str, request: Value) -> Result<Value, Refusal> {
    data(&host.state, plugin, serde_json::from_value(request).expect("a data request")).await
}

async fn refused(host: &Host, plugin: &str, request: Value) -> (u16, &'static str, String) {
    let refusal = ask(host, plugin, request).await.expect_err("refused");
    (refusal.status, refusal.kind, refusal.detail)
}

async fn insert(host: &Host, values: Value) -> Value {
    let request = json!({ "op": "insert", "collection": "greetings", "values": values });
    ask(host, "hello", request).await.expect("inserted")["record"].clone()
}

async fn names(host: &Host, query: Value) -> (Vec<String>, Value) {
    let answer = ask(host, "hello", query).await.expect("answered");
    let names = answer["records"]
        .as_array()
        .expect("records")
        .iter()
        .map(|record| record["name"].as_str().unwrap_or_default().to_string())
        .collect();
    (names, answer["next"].clone())
}

#[tokio::test]
async fn a_declaration_that_is_not_well_formed_is_refused_as_bad_data() {
    let host = host();
    let keyed = || Collection::new().field("id", Field::uuid().key());
    let bad = [
        ("no key", Collection::new().field("name", Field::text())),
        ("two keys", keyed().field("other", Field::text().key())),
        ("a number key", Collection::new().field("id", Field::integer().key())),
        ("a field name", keyed().field("Name", Field::text())),
        ("an index on nothing", keyed().index(&["missing"])),
        ("search on a number", keyed().field("n", Field::integer()).search(&["n"])),
        ("a reference to nothing", keyed().field("to", Field::reference("nowhere"))),
        ("a default of the wrong type", keyed().field("n", Field::integer().default(json!("x")))),
        ("one_of on a number", keyed().field("n", Field::integer().one_of(&["1"]))),
        (
            "a list without `of`",
            keyed().field("l", Field::of_type(doc_plugin_protocol::data::FieldType::List)),
        ),
        ("an export of a missing field", keyed().export(Export::to(&["kb"]).fields(&["missing"]))),
    ];
    for (why, collection) in bad {
        let declared = Declaration::default().collection("things", collection);
        let err = register(&host, manifest("hello", "1.0.0", declared)).await.expect_err(why);
        assert_eq!((err.status(), err.kind()), (400, "bad-data"), "{why}: {err}");
    }
    let named = Declaration::default().collection("Things", keyed());
    let err = register(&host, manifest("hello", "1.0.0", named)).await.expect_err("a bad name");
    assert_eq!(err.kind(), "bad-data");
    assert!(host.state.plugins.get("hello").await.is_none(), "nothing was registered");
}

#[tokio::test]
async fn additive_changes_are_applied_before_load() {
    let host = host();
    hello(&host).await;
    insert(&host, json!({ "name": "Ada" })).await;
    let grown = declaration()
        .collection("notes", Collection::new().field("id", Field::uuid().key()))
        .collection(
            "greetings",
            greetings()
                .field("colour", Field::text().required().default(json!("blue")))
                .field("name", Field::text().required().max(40.0))
                .field("mood", Field::text().one_of(&["happy", "grumpy", "calm"]))
                .index(&["name"]),
        );
    register(&host, manifest("hello", "2.0.0", grown.clone())).await.expect("registered");
    assert_eq!(host.state_of("hello").await, Some(PluginState::Running));
    let declarations = host.data.declarations("hello").await.expect("read");
    assert_eq!(declarations.serving, Some(grown));
    let (names, _) = names(&host, json!({ "op": "query", "collection": "greetings" })).await;
    assert_eq!(names, vec!["Ada"]);
    let query = json!({ "op": "query", "collection": "greetings", "where": { "colour": "blue" } });
    assert_eq!(
        ask(&host, "hello", query).await.unwrap()["records"].as_array().unwrap().len(),
        1,
        "backfilled"
    );
    let calm = json!({ "name": "A name of thirty characters!!", "mood": "calm" });
    insert(&host, calm).await;
}

#[tokio::test]
async fn a_one_of_can_be_dropped_for_any_text() {
    let host = host();
    hello(&host).await;
    let loosened = declaration().collection("greetings", greetings().field("mood", Field::text()));
    register(&host, manifest("hello", "2.0.0", loosened)).await.expect("registered");
    assert_eq!(host.state_of("hello").await, Some(PluginState::Running));
    insert(&host, json!({ "name": "Ada", "mood": "wistful" })).await;
}

#[tokio::test]
async fn a_change_that_would_break_the_serving_version_is_refused_as_incompatible_data() {
    let host = host();
    hello(&host).await;
    let changed = |collection: Collection| declaration().collection("greetings", collection);
    let without = |field: &str| {
        let mut collection = greetings();
        collection.fields.remove(field);
        collection.unique.clear();
        collection.search.clear();
        collection.indexes.clear();
        collection
    };
    let incompatible = [
        ("a type change", changed(greetings().field("count", Field::number()))),
        ("a field removed without deprecation", changed(without("source"))),
        (
            "a new required field without a default",
            changed(greetings().field("colour", Field::text().required())),
        ),
        ("a lower max", changed(greetings().field("name", Field::text().required().max(10.0)))),
        ("a new one_of", changed(greetings().field("source", Field::text().one_of(&["a"])))),
        (
            "an optional field made required",
            changed(greetings().field("source", Field::text().required())),
        ),
        (
            "a new key",
            changed(greetings().field("id", Field::uuid()).field("code", Field::text().key())),
        ),
        ("a collection removed without deprecation", {
            let mut declared = declaration();
            declared.collections.remove("likes");
            declared
        }),
    ];
    for (why, declared) in incompatible {
        let err = register(&host, manifest("hello", "2.0.0", declared)).await.expect_err(why);
        assert_eq!((err.status(), err.kind()), (400, "incompatible-data"), "{why}: {err}");
    }
    assert_eq!(host.state.plugins.get("hello").await.unwrap().manifest.version, "1.0.0");
}

#[tokio::test]
async fn a_field_is_removed_in_the_version_after_the_one_that_deprecates_it() {
    let host = host();
    hello(&host).await;
    insert(&host, json!({ "name": "Ada", "source": "import" })).await;
    let mut deprecating = greetings().field("source", Field::text().deprecated());
    deprecating.unique.clear();
    let deprecated = declaration().collection("greetings", deprecating.clone());
    register(&host, manifest("hello", "2.0.0", deprecated)).await.expect("deprecated");
    let query =
        json!({ "op": "query", "collection": "greetings", "where": { "source": "import" } });
    assert_eq!(
        ask(&host, "hello", query.clone()).await.unwrap()["records"].as_array().unwrap().len(),
        1
    );

    let mut removed = deprecating;
    removed.fields.remove("source");
    register(&host, manifest("hello", "3.0.0", declaration().collection("greetings", removed)))
        .await
        .expect("removed");
    let (status, kind, _) = refused(&host, "hello", query).await;
    assert_eq!((status, kind), (400, "bad-query"), "the field is gone once the handover completed");
    let (names, _) = names(&host, json!({ "op": "query", "collection": "greetings" })).await;
    assert_eq!(names, vec!["Ada"], "and only the field");
}

#[tokio::test]
async fn each_operation_reads_and_writes_records() {
    let host = host();
    hello(&host).await;
    let ada = insert(
        &host,
        json!({ "name": "Ada", "mood": "happy", "count": 1, "at": "2026-09-03T00:00:00Z" }),
    )
    .await;
    let id = ada["id"].as_str().expect("a generated key").to_string();
    assert_eq!(uuid::Uuid::parse_str(&id).unwrap().get_version_num(), 7);
    assert_eq!(ada["_version"], 1);
    assert!(
        ada["at"].is_string() && ada["_created_at"].is_string(),
        "defaults and system fields: {ada}"
    );
    assert_eq!(ada["source"], Value::Null, "every field is there, if only as null");

    let got = ask(&host, "hello", json!({ "op": "get", "collection": "greetings", "key": id }))
        .await
        .unwrap();
    assert_eq!(got["record"], ada);
    let none = json!({ "op": "get", "collection": "greetings", "key": uuid::Uuid::now_v7() });
    assert_eq!(ask(&host, "hello", none).await.unwrap(), json!({ "record": null }));

    let set = json!({ "op": "update", "collection": "greetings", "key": id, "set": { "count": 5 }, "version": 1 });
    let updated = ask(&host, "hello", set).await.unwrap()["record"].clone();
    assert_eq!((updated["count"].clone(), updated["_version"].clone()), (json!(5), json!(2)));
    let same =
        json!({ "op": "update", "collection": "greetings", "key": id, "set": { "count": 5 } });
    assert_eq!(
        ask(&host, "hello", same).await.unwrap()["record"]["_version"],
        2,
        "no change, no new version"
    );

    for (name, mood, at) in [
        ("Bea", "grumpy", "2026-09-01T10:00:00Z"),
        ("Cy", "happy", "2026-09-02T10:00:00Z"),
        ("Di", "happy", "2026-09-02T11:30:00Z"),
    ] {
        insert(&host, json!({ "name": name, "mood": mood, "at": at, "count": 2 })).await;
    }
    let happy = json!({
        "op": "query", "collection": "greetings", "where": { "mood": "happy", "name": { "ne": "Ada" } },
        "order": [{ "field": "mood" }, { "field": "at", "dir": "desc" }], "limit": 1,
    });
    let (first, next) = names(&host, happy.clone()).await;
    assert_eq!(first, vec!["Di"]);
    let mut again = happy.clone();
    again["after"] = next;
    let (second, next) = names(&host, again).await;
    assert_eq!((second, next), (vec!["Cy".to_string()], Value::Null), "the last page says so");
    let either = json!({
        "op": "query", "collection": "greetings",
        "where": { "any": [{ "name": { "prefix": "B" } }, { "count": { "gte": 5 } }] },
        "order": [{ "field": "at" }], "fields": ["name"],
    });
    let answer = ask(&host, "hello", either).await.unwrap();
    assert_eq!(answer["records"], json!([{ "name": "Bea" }, { "name": "Ada" }]));

    let counted = Aggregate::new("greetings")
        .filter(json!({ "at": { "lt": "2026-09-03T00:00:00Z" } }))
        .bucket("at", Bucket::Day)
        .measure("n", Measure::Count("*".into()))
        .measure("total", Measure::Sum("count".into()));
    let groups =
        ask(&host, "hello", serde_json::to_value(DataRequest::Aggregate(counted)).unwrap())
            .await
            .unwrap();
    assert_eq!(
        groups["groups"],
        json!([{ "at": "2026-09-01", "n": 1, "total": 2 }, { "at": "2026-09-02", "n": 2, "total": 4 }])
    );

    let delete = json!({ "op": "delete", "collection": "greetings", "key": id, "version": 2 });
    assert_eq!(ask(&host, "hello", delete.clone()).await.unwrap(), json!({ "deleted": true }));
    let again = json!({ "op": "delete", "collection": "greetings", "key": id });
    assert_eq!(ask(&host, "hello", again).await.unwrap(), json!({ "deleted": false }));
}

#[tokio::test]
async fn a_search_finds_records_by_their_search_fields() {
    let host = host();
    hello(&host).await;
    for name in ["Ada Lovelace", "Grace Hopper", "Ada Palmer"] {
        insert(&host, json!({ "name": name })).await;
    }
    let search = json!({ "op": "query", "collection": "greetings", "search": "ada", "limit": 1 });
    let (first, next) = names(&host, search.clone()).await;
    let mut again = search.clone();
    again["after"] = next;
    let (second, next) = names(&host, again).await;
    let mut found = [first, second].concat();
    found.sort();
    assert_eq!(
        (found, next),
        (vec!["Ada Lovelace".to_string(), "Ada Palmer".to_string()], Value::Null)
    );
    let (status, kind, _) =
        refused(&host, "hello", json!({ "op": "query", "collection": "replies", "search": "x" }))
            .await;
    assert_eq!((status, kind), (400, "bad-query"), "replies has no search fields");
}

#[tokio::test]
async fn a_stale_version_is_a_conflict_and_changes_nothing() {
    let host = host();
    hello(&host).await;
    let id = insert(&host, json!({ "name": "Ada" })).await["id"].clone();
    let set = |version: i64| json!({ "op": "update", "collection": "greetings", "key": id, "set": { "name": "Bea" }, "version": version });
    ask(&host, "hello", set(1)).await.expect("the first writer wins");
    assert_eq!(refused(&host, "hello", set(1)).await.1, "version-conflict");
    let delete = json!({ "op": "delete", "collection": "greetings", "key": id, "version": 1 });
    let (status, kind, detail) = refused(&host, "hello", delete).await;
    assert_eq!((status, kind), (409, "version-conflict"));
    assert!(detail.contains("version 2"), "{detail}");
    let got = ask(&host, "hello", json!({ "op": "get", "collection": "greetings", "key": id }))
        .await
        .unwrap();
    assert_eq!(
        (got["record"]["name"].clone(), got["record"]["_version"].clone()),
        (json!("Bea"), json!(2))
    );
}

#[tokio::test]
async fn upsert_inserts_once_then_updates_the_record_its_unique_fields_find() {
    let host = host();
    hello(&host).await;
    let upsert = |name: &str| {
        json!({ "op": "upsert", "collection": "greetings", "on": ["external_id", "source"],
                "values": { "source": "hr", "external_id": "7", "name": name } })
    };
    let first = ask(&host, "hello", upsert("Ada")).await.unwrap();
    assert_eq!(first["created"], true);
    let second = ask(&host, "hello", upsert("Ada L.")).await.unwrap();
    assert_eq!(second["created"], false);
    assert_eq!(second["record"]["id"], first["record"]["id"]);
    assert_eq!(
        (second["record"]["name"].clone(), second["record"]["_version"].clone()),
        (json!("Ada L."), json!(2))
    );
    let again = ask(&host, "hello", upsert("Ada L.")).await.unwrap();
    assert_eq!(again["record"]["_version"], 2, "repeating it changes nothing");

    let unconstrained = json!({ "op": "upsert", "collection": "greetings", "on": ["name"], "values": { "name": "x" } });
    assert_eq!(refused(&host, "hello", unconstrained).await.0, 400);
    let pin = json!({ "op": "upsert", "collection": "pins", "on": ["id"],
                      "values": { "id": "top", "greeting": first["record"]["id"] } });
    assert_eq!(
        ask(&host, "hello", pin.clone()).await.unwrap()["created"],
        true,
        "the key is unique too"
    );
    assert_eq!(ask(&host, "hello", pin).await.unwrap()["created"], false);
    let missing = json!({ "op": "upsert", "collection": "greetings", "on": ["source", "external_id"],
                          "values": { "source": "hr", "name": "x" } });
    assert_eq!(refused(&host, "hello", missing).await.1, "invalid-record");
}

#[tokio::test]
async fn a_batch_is_all_or_nothing() {
    let host = host();
    hello(&host).await;
    let insert = |name: &str| DataRequest::insert("greetings", json!({ "name": name }));
    let batch =
        |writes: Vec<DataRequest>| serde_json::to_value(DataRequest::Batch { writes }).unwrap();
    let too_long = "x".repeat(21);
    let (status, kind, detail) =
        refused(&host, "hello", batch(vec![insert("Ada"), insert("Bea"), insert(&too_long)])).await;
    assert_eq!((status, kind), (400, "invalid-record"));
    assert!(detail.contains("`name`"), "{detail}");
    let (none, _) = names(&host, json!({ "op": "query", "collection": "greetings" })).await;
    assert!(none.is_empty(), "the first two were not written either");

    let answer = ask(&host, "hello", batch(vec![insert("Ada"), insert("Bea")])).await.unwrap();
    assert_eq!(answer["results"].as_array().unwrap().len(), 2);
    let many = (0..101).map(|n| insert(&n.to_string())).collect();
    assert_eq!(refused(&host, "hello", batch(many)).await.0, 400);
    let read = serde_json::to_value(DataRequest::Query(Query::new("greetings"))).unwrap();
    let mixed = json!({ "op": "batch", "writes": [read] });
    assert_eq!(refused(&host, "hello", mixed).await.0, 400);
}

#[tokio::test]
async fn a_write_that_breaks_a_declared_rule_is_refused_naming_the_field() {
    let host = host();
    hello(&host).await;
    insert(&host, json!({ "name": "Ada", "source": "hr", "external_id": "1" })).await;
    let ada = host.identity.add_user("ada");
    insert(&host, json!({ "name": "Bea", "by": ada.id })).await;
    for (values, field) in [
        (json!({ "mood": "happy" }), "`name`"),
        (json!({ "name": 7 }), "`name`"),
        (json!({ "name": "x".repeat(21) }), "`name`"),
        (json!({ "name": "Cy", "mood": "sad" }), "`mood`"),
        (json!({ "name": "Cy", "count": -1 }), "`count`"),
        (json!({ "name": "Cy", "at": "yesterday" }), "`at`"),
        (json!({ "name": "Cy", "tags": ["a", 1] }), "`tags`"),
        (json!({ "name": "Cy", "by": uuid::Uuid::now_v7() }), "`by`"),
        (json!({ "name": "Cy", "colour": "red" }), "`colour`"),
        (json!({ "name": "Cy", "_version": 9 }), "`_version`"),
    ] {
        let request = json!({ "op": "insert", "collection": "greetings", "values": values });
        let (status, kind, detail) = refused(&host, "hello", request).await;
        assert_eq!((status, kind), (400, "invalid-record"), "{values}: {detail}");
        assert!(detail.contains(field), "{values}: {detail}");
    }
    let dangling = json!({ "op": "insert", "collection": "replies", "values": { "greeting": uuid::Uuid::now_v7() } });
    assert!(refused(&host, "hello", dangling).await.2.contains("`greeting`"));
}

#[tokio::test]
async fn a_taken_key_or_unique_values_are_a_duplicate_not_an_invalid_record() {
    let host = host();
    hello(&host).await;
    let ada = insert(&host, json!({ "name": "Ada", "source": "hr", "external_id": "1" })).await;
    let bea = insert(&host, json!({ "name": "Bea", "source": "hr", "external_id": "2" })).await;
    let same_key = json!({ "op": "insert", "collection": "greetings",
                           "values": { "id": ada["id"], "name": "Cy" } });
    let (status, kind, _) = refused(&host, "hello", same_key).await;
    assert_eq!((status, kind), (409, "duplicate-record"));
    let same_values = json!({ "op": "insert", "collection": "greetings",
                              "values": { "name": "Cy", "source": "hr", "external_id": "1" } });
    let (status, kind, detail) = refused(&host, "hello", same_values).await;
    assert_eq!((status, kind), (409, "duplicate-record"));
    assert!(detail.contains("`source`"), "{detail}");
    let onto_taken = json!({ "op": "update", "collection": "greetings", "key": bea["id"],
                             "set": { "external_id": "1" } });
    assert_eq!(refused(&host, "hello", onto_taken).await.1, "duplicate-record");
}

#[tokio::test]
async fn deleting_a_record_restricts_cascades_or_clears_what_refers_to_it() {
    let host = host();
    hello(&host).await;
    let id = insert(&host, json!({ "name": "Ada" })).await["id"].clone();
    let write = |collection: &str, values: Value| json!({ "op": "insert", "collection": collection, "values": values });
    ask(&host, "hello", write("replies", json!({ "greeting": id, "text": "hi" }))).await.unwrap();
    let like = ask(&host, "hello", write("likes", json!({ "greeting": id }))).await.unwrap()["record"]["id"].clone();
    ask(&host, "hello", write("pins", json!({ "id": "top", "greeting": id }))).await.unwrap();

    let delete = json!({ "op": "delete", "collection": "greetings", "key": id });
    let (status, _, detail) = refused(&host, "hello", delete.clone()).await;
    assert_eq!(status, 400);
    assert!(detail.contains("pins top"), "a pin restricts it: {detail}");
    ask(&host, "hello", json!({ "op": "delete", "collection": "pins", "key": "top" }))
        .await
        .unwrap();

    ask(&host, "hello", delete).await.expect("deleted");
    let (replies, _) = names(&host, json!({ "op": "query", "collection": "replies" })).await;
    assert!(replies.is_empty(), "replies went with it");
    let liked = ask(&host, "hello", json!({ "op": "get", "collection": "likes", "key": like }))
        .await
        .unwrap();
    assert_eq!(
        (liked["record"]["greeting"].clone(), liked["record"]["_version"].clone()),
        (Value::Null, json!(2))
    );
}

#[tokio::test]
async fn a_query_sorts_only_by_the_key_system_fields_or_an_index() {
    let host = host();
    hello(&host).await;
    let query = |extra: Value| {
        let mut query = json!({ "op": "query", "collection": "greetings" });
        query.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        query
    };
    for fine in [
        json!({ "order": [{ "field": "id", "dir": "desc" }] }),
        json!({ "order": [{ "field": "_updated_at" }] }),
        json!({ "order": [{ "field": "mood" }] }),
        json!({ "order": [{ "field": "source" }, { "field": "external_id" }] }),
    ] {
        ask(&host, "hello", query(fine.clone()))
            .await
            .unwrap_or_else(|err| panic!("{fine}: {err:?}"));
    }
    for bad in [
        json!({ "order": [{ "field": "name" }] }),
        json!({ "order": [{ "field": "at" }, { "field": "mood" }] }),
        json!({ "where": { "missing": 1 } }),
        json!({ "where": { "count": { "like": 1 } } }),
        json!({ "where": { "count": "many" } }),
        json!({ "where": { "tags": { "prefix": "a" } } }),
        json!({ "limit": 1001 }),
        json!({ "after": "not-a-cursor" }),
        json!({ "fields": ["missing"] }),
    ] {
        let (status, kind, _) = refused(&host, "hello", query(bad.clone())).await;
        assert_eq!((status, kind), (400, "bad-query"), "{bad}");
    }
    let unknown = json!({ "op": "query", "collection": "nothing" });
    assert_eq!(refused(&host, "hello", unknown).await.1, "no-collection");
}

#[tokio::test]
async fn another_plugins_collection_is_read_only_through_its_export() {
    let host = host();
    hello(&host).await;
    let documents = Collection::new()
        .field("id", Field::text().key())
        .field("title", Field::text())
        .field("owner_email", Field::text())
        .index(&["title"])
        .export(Export::to(&["hello"]).fields(&["id", "title"]));
    let kb = Declaration::default()
        .collection("documents", documents)
        .collection("secrets", Collection::new().field("id", Field::text().key()));
    register(&host, manifest("kb", "1.0.0", kb)).await.expect("registered");
    let values = json!({ "id": "intro", "title": "Welcome", "owner_email": "ada@example.com" });
    ask(&host, "kb", json!({ "op": "insert", "collection": "documents", "values": values }))
        .await
        .unwrap();

    let read =
        json!({ "op": "query", "collection": "kb.documents", "order": [{ "field": "title" }] });
    let answer = ask(&host, "hello", read).await.expect("exported to hello");
    let record = answer["records"][0].as_object().unwrap();
    assert_eq!(record["title"], "Welcome");
    assert!(!record.contains_key("owner_email"), "only the exported fields: {record:?}");
    let got =
        ask(&host, "hello", json!({ "op": "get", "collection": "kb.documents", "key": "intro" }))
            .await
            .unwrap();
    assert!(got["record"].get("owner_email").is_none());

    let hidden = json!({ "op": "query", "collection": "kb.documents", "where": { "owner_email": "ada@example.com" } });
    assert_eq!(
        refused(&host, "hello", hidden).await.1,
        "bad-query",
        "hidden fields cannot be filtered on either"
    );
    let private = json!({ "op": "query", "collection": "kb.secrets" });
    assert_eq!(refused(&host, "hello", private).await.0, 403);
    let not_listed = json!({ "op": "query", "collection": "kb.documents" });
    assert_eq!(refused(&host, "rbac", not_listed).await.0, 403, "exported to hello, not rbac");
    let write = json!({ "op": "insert", "collection": "kb.documents", "values": { "id": "x" } });
    assert_eq!(refused(&host, "hello", write).await.0, 403);
    let missing = json!({ "op": "query", "collection": "kb.nothing" });
    assert_eq!(refused(&host, "hello", missing).await.0, 404);
}

#[tokio::test]
async fn core_collections_are_read_only_and_tasks_are_the_readers_own() {
    let host = host();
    hello(&host).await;
    let ada = host.identity.add_user("ada");
    let users = json!({ "op": "query", "collection": "core.users", "where": { "login": "ada" } });
    let answer = ask(&host, "hello", users).await.unwrap();
    assert_eq!(answer["records"][0]["id"], json!(ada.id));
    let plugins = json!({ "op": "get", "collection": "core.plugins", "key": "hello" });
    assert_eq!(ask(&host, "hello", plugins).await.unwrap()["record"]["state"], "running");
    let declared = json!({ "op": "query", "collection": "core.plugin-permissions", "where": { "plugin": "hello" } });
    assert_eq!(
        ask(&host, "hello", declared).await.unwrap()["records"].as_array().unwrap().len(),
        2
    );

    for (plugin, payload) in
        [("hello", json!({ "mine": true })), ("rbac", json!({ "theirs": true }))]
    {
        let request = doc_plugin_protocol::calls::TaskRequest {
            payload,
            max_attempts: None,
            delegation: None,
        };
        tasks(&host.state, plugin, None, request).await.expect("queued");
    }
    let own =
        ask(&host, "hello", json!({ "op": "query", "collection": "core.tasks" })).await.unwrap();
    assert_eq!(own["records"].as_array().unwrap().len(), 1);
    assert_eq!(own["records"][0]["payload"], json!({ "mine": true }));

    let write = json!({ "op": "update", "collection": "core.users", "key": ada.id, "set": { "login": "eve" } });
    assert_eq!(refused(&host, "hello", write).await.0, 403);
}

#[tokio::test]
async fn the_status_history_is_read_a_bounded_window_at_a_time() {
    use crate::status::{Component, Health};
    let host = host();
    hello(&host).await;
    let now = chrono::Utc::now();
    for (hours_ago, state) in [(30, Health::Up), (3, Health::Down), (2, Health::Up)] {
        let mut check = Component::new("database", "postgres", state);
        check.checked_at = now - chrono::Duration::hours(hours_ago);
        host.state.repos.status_history.record(&check).await.unwrap();
    }
    let since = (now - chrono::Duration::hours(6)).to_rfc3339();
    let read = json!({ "op": "query", "collection": "core.status-history",
        "where": { "at": { "gte": since } }, "order": [{ "field": "at" }] });
    let answer = ask(&host, "hello", read).await.unwrap();
    let states: Vec<&str> = answer["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["down", "up"], "only the checks in the day after the lower bound");

    let unbounded = json!({ "op": "query", "collection": "core.status-history" });
    assert_eq!(refused(&host, "hello", unbounded).await.1, "bad-query");
    let week = (now - chrono::Duration::days(7)).to_rfc3339();
    let too_long = json!({ "op": "query", "collection": "core.status-history",
        "where": { "at": { "gte": week, "lt": now.to_rfc3339() } } });
    assert_eq!(refused(&host, "hello", too_long).await.1, "bad-query", "a day at a time");
    let change = crate::status::plugins::PluginChange {
        plugin: "hello".into(),
        version: "1.0.0".into(),
        classification: doc_plugin_protocol::Classification::Synchronous,
        instance: uuid::Uuid::new_v4(),
        state: Some(PluginState::Error),
        error: Some("it fell over".into()),
        since: now - chrono::Duration::days(3),
        at: now - chrono::Duration::days(3),
        registered_at: now - chrono::Duration::days(4),
    };
    let source = crate::status::plugins::Source::Event;
    host.state.repos.plugin_status.record(&change, source).await.unwrap();
    let changes = json!({ "op": "query", "collection": "core.plugin-status-history",
        "where": { "at": { "gte": week } } });
    let answer = ask(&host, "hello", changes).await.unwrap();
    assert!(
        answer["records"].as_array().unwrap().iter().any(|r| r["state"] == "error"),
        "a month of plugin changes is one read: {answer}"
    );
}

#[tokio::test]
async fn each_committed_write_announces_its_changes_without_values() {
    let host = host();
    hello(&host).await;
    let filter = TopicFilter::new("plugin.hello.data.>").unwrap();
    let mut changes =
        host.state.buses.events.subscribe(ConsumerGroup::new("t", filter)).await.unwrap();
    let id = insert(&host, json!({ "name": "Ada" })).await["id"].clone();
    let batch = json!({ "op": "batch", "writes": [
        { "op": "update", "collection": "greetings", "key": id, "set": { "name": "Bea" } },
        { "op": "insert", "collection": "replies", "values": { "greeting": id } },
        { "op": "delete", "collection": "greetings", "key": id },
    ] });
    ask(&host, "hello", batch).await.expect("written");
    let failed = json!({ "op": "insert", "collection": "greetings", "values": {} });
    refused(&host, "hello", failed).await;

    let mut seen = Vec::new();
    for _ in 0..4 {
        let delivery = tokio::time::timeout(Duration::from_secs(1), changes.next()).await;
        let Ok(Some(delivery)) = delivery else { break };
        seen.push((delivery.event.topic.as_str().to_string(), delivery.event.payload));
    }
    let greetings = "plugin.hello.data.greetings.changed";
    let replies = "plugin.hello.data.replies.changed";
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert_eq!(
        seen[0],
        (
            greetings.into(),
            json!({ "collection": "greetings", "changes": [{ "op": "insert", "key": id, "version": 1 }] })
        )
    );
    let (topics, payloads): (Vec<_>, Vec<_>) = seen[1..].iter().cloned().unzip();
    assert!(topics.contains(&greetings.to_string()) && topics.contains(&replies.to_string()));
    let batched = payloads.iter().find(|payload| payload["collection"] == "greetings").unwrap();
    assert_eq!(
        batched["changes"],
        json!([{ "op": "update", "key": id, "version": 2 }, { "op": "delete", "key": id, "version": 2 }])
    );
    let reply = payloads.iter().find(|payload| payload["collection"] == "replies").unwrap();
    let ops: Vec<&str> =
        reply["changes"].as_array().unwrap().iter().map(|c| c["op"].as_str().unwrap()).collect();
    assert_eq!(ops, vec!["insert", "delete"], "the reply went with its greeting");
}
