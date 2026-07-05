//! What each command asks the backend, and how the answer is shown.

use std::time::{Duration, Instant};

use doc_secret::Secret;
use serde_json::{Value, json};
use url::form_urlencoded::byte_serialize;
use uuid::Uuid;

use crate::client::{Client, Failure};
use crate::credentials;
use crate::output::{Output, Table, cell, say};
use crate::{
    AccountTokens, Automation, Calendar, Cli, Command, Github, Groups, Holder, Infra, Kb,
    KbSources, Permissions, PluginTokens, Plugins, Processes, Resources, ServiceAccounts,
    SourceArgs, Tasks,
};

const DEFAULT_URL: &str = "http://127.0.0.1:8080";
const RBAC: &str = "plugins/rbac/api";
const RESOURCES: &str = "plugins/resources/api";

pub async fn run(cli: Cli) -> Result<(), Failure> {
    let saved = credentials::load();
    let url = cli.url.clone().or(saved.url).unwrap_or_else(|| DEFAULT_URL.into());
    let out = Output { json: cli.json };
    if let Command::Login = cli.command {
        return login(&url, cli.token, &out).await;
    }
    let client = Client::new(&url, cli.token.or(saved.token))?;
    match cli.command {
        Command::Login => Ok(()),
        Command::Logout => logout(&out),
        Command::Whoami => whoami(&client, &out).await,
        Command::Status => status(&client, &out).await,
        Command::Plugins(command) => plugins(&client, &out, command).await,
        Command::Sa(command) => accounts(&client, &out, command).await,
        Command::Permissions(command) => permissions(&client, &out, command).await,
        Command::Groups(command) => groups(&client, &out, command).await,
        Command::Tasks(command) => tasks(&client, &out, command).await,
        Command::Resources(command) => resources(&client, &out, command).await,
        Command::Kb(command) => kb(&client, &out, command).await,
        Command::Automation(command) => automation(&client, &out, command).await,
        Command::Process(command) => processes(&client, &out, command).await,
        Command::Infra(command) => infra(&client, &out, command).await,
        Command::Calendar(Calendar::Agenda { calendar, from, to }) => {
            agenda(&client, &out, calendar, from, to).await
        }
        Command::Github(Github::Sync { wait: waiting, timeout, plugin }) => {
            let started = client.post(&format!("plugins/{plugin}/api/sync"), json!({})).await?;
            let task = cell(&started, "task");
            match waiting {
                true => wait(&client, &out, &task, timeout, 2).await,
                false => {
                    out.done(
                        &started,
                        &format!("Syncing, as task {task}; follow it with `cli tasks wait {task}`"),
                    );
                    Ok(())
                }
            }
        }
    }
}

fn encode(value: &str) -> String {
    byte_serialize(value.as_bytes()).collect()
}

fn label(principal: &Value) -> String {
    let name = cell(principal, "login");
    if name != "-" { name } else { cell(principal, "name") }
}

fn pairs(value: &Value, fields: &[(&str, &str)]) {
    let width = fields.iter().map(|(title, _)| title.len()).max().unwrap_or(0);
    for (title, path) in fields {
        say!("{title:<width$}  {}", cell(value, path));
    }
}

async fn login(url: &str, token: Option<Secret<String>>, out: &Output) -> Result<(), Failure> {
    let token = token.ok_or_else(|| {
        Failure::Refused("give the token to save with --token or DOC_TOKEN".into())
    })?;
    let me = Client::new(url, Some(token.clone()))?.get("me").await?;
    let path = credentials::save(url, &token).map_err(Failure::Refused)?;
    out.done(&me, &format!("Signed in to {url} as {}; saved in {}", label(&me), path.display()));
    Ok(())
}

fn logout(out: &Output) -> Result<(), Failure> {
    let message = match credentials::forget().map_err(Failure::Refused)? {
        Some(path) => format!("Forgot the token saved in {}", path.display()),
        None => "No token was saved".into(),
    };
    out.done(&Value::Null, &message);
    Ok(())
}

async fn whoami(client: &Client, out: &Output) -> Result<(), Failure> {
    let me = client.get("me").await?;
    out.show(&me, |me| {
        pairs(
            me,
            &[
                ("Kind", "kind"),
                ("ID", "id"),
                ("Provider", "provider"),
                ("Login", "login"),
                ("Name", "name"),
            ],
        );
        say!("Backend   {}", client.base());
    });
    Ok(())
}

async fn status(client: &Client, out: &Output) -> Result<(), Failure> {
    let status = client.get("status").await?;
    out.show(&status, |status| {
        say!("Platform is {}, checked by {}\n", cell(status, "state"), cell(status, "source"));
        let components = status["components"].as_array().cloned().unwrap_or_default();
        Table::of(
            &["COMPONENT", "KIND", "STATE", "DETAIL"],
            &components,
            &["name", "kind", "state", "detail"],
        )
        .print();
        say!();
        let plugins = status["plugins"].as_array().cloned().unwrap_or_default();
        Table::of(
            &["PLUGIN", "STATE", "LIFECYCLE", "VERSION", "ERROR"],
            &plugins,
            &["id", "state", "lifecycle", "version", "error"],
        )
        .print();
    });
    Ok(())
}

async fn plugins(client: &Client, out: &Output, command: Plugins) -> Result<(), Failure> {
    match command {
        Plugins::List => {
            let listed = client.get("plugins").await?;
            out.show(&listed, |listed| {
                let plugins = listed["plugins"].as_array().cloned().unwrap_or_default();
                let mut table =
                    Table::new(&["PLUGIN", "VERSION", "CLASSIFICATION", "STATE", "ERROR"]);
                for plugin in &plugins {
                    let state = plugin["state"].as_str().unwrap_or("not running").to_string();
                    let columns =
                        ["id", "version", "classification"].map(|path| cell(plugin, path));
                    table.row(columns.into_iter().chain([state, cell(plugin, "error")]).collect());
                }
                table.print();
            });
        }
        Plugins::Show { id } => {
            let shown = client.get(&format!("plugins/{id}")).await?;
            out.show(&shown, |shown| {
                pairs(
                    shown,
                    &[
                        ("Plugin", "id"),
                        ("Version", "version"),
                        ("Classification", "classification"),
                        ("State", "state"),
                        ("Since", "since"),
                        ("Error", "error"),
                        ("Last error", "last_error"),
                        ("Registered", "registered_at"),
                        ("Address", "registration.address"),
                        ("Last liveness", "registration.last_seen"),
                    ],
                );
                say!();
                let history = shown["history"].as_array().cloned().unwrap_or_default();
                Table::of(
                    &["AT", "VERSION", "STATE", "ERROR", "FROM"],
                    &history,
                    &["at", "version", "state", "error", "source"],
                )
                .print();
            });
        }
        Plugins::Reload { id } => {
            let answer = client.post(&format!("plugins/{id}/reload"), json!({})).await?;
            out.done(&answer, &format!("Reloading {id}: {}", cell(&answer, "state")));
        }
        Plugins::Unload { id } => {
            let answer = client.post(&format!("plugins/{id}/unload"), json!({})).await?;
            out.done(&answer, &format!("Unloaded {id}"));
        }
        Plugins::Cancel { id } => {
            let answer = client.post(&format!("plugins/{id}/cancel"), json!({})).await?;
            out.done(&answer, &format!("Cancelled {id}'s work until it is resumed"));
        }
        Plugins::Token(command) => plugin_tokens(client, out, command).await?,
    }
    Ok(())
}

async fn plugin_tokens(
    client: &Client,
    out: &Output,
    command: PluginTokens,
) -> Result<(), Failure> {
    match command {
        PluginTokens::List { plugin } => {
            let tokens = client.get(&format!("plugins/{plugin}/tokens")).await?;
            out.show(&tokens, token_table);
        }
        PluginTokens::Create { plugin, name, days } => {
            let body = json!({ "name": name, "expires_in_days": days });
            let created = client.post(&format!("plugins/{plugin}/tokens"), body).await?;
            out.show(&created, secret);
        }
        PluginTokens::Revoke { plugin, token_id } => {
            let answer = client.delete(&format!("plugins/{plugin}/tokens/{token_id}")).await?;
            out.done(&answer, &format!("Revoked {plugin}'s token {token_id}"));
        }
    }
    Ok(())
}

fn token_table(tokens: &Value) {
    let tokens = tokens.as_array().cloned().unwrap_or_default();
    Table::of(
        &["ID", "NAME", "CREATED", "EXPIRES", "REVOKED"],
        &tokens,
        &["id", "name", "created_at", "expires_at", "revoked_at"],
    )
    .print();
}

fn secret(created: &Value) {
    say!("{}", cell(created, "token"));
    eprintln!("This secret is shown once; keep it now. Its ID is {}.", cell(created, "created.id"));
}

/// A service account by ID, or by a name among those the caller can see.
async fn account(client: &Client, account: &str) -> Result<String, Failure> {
    if Uuid::parse_str(account).is_ok() {
        return Ok(account.to_string());
    }
    let accounts = client.get("service-accounts").await?;
    accounts
        .as_array()
        .into_iter()
        .flatten()
        .find(|found| found["name"] == account)
        .and_then(|found| found["id"].as_str().map(str::to_string))
        .ok_or_else(|| {
            Failure::Refused(format!("no service account called {account} that you can see"))
        })
}

async fn accounts(client: &Client, out: &Output, command: ServiceAccounts) -> Result<(), Failure> {
    match command {
        ServiceAccounts::List => {
            let accounts = client.get("service-accounts").await?;
            out.show(&accounts, |accounts| {
                let accounts = accounts.as_array().cloned().unwrap_or_default();
                Table::of(
                    &["ID", "NAME", "OWNER", "DISABLED", "CREATED"],
                    &accounts,
                    &["id", "name", "owner_id", "disabled", "created_at"],
                )
                .print();
            });
        }
        ServiceAccounts::Create { name, description } => {
            let created = client
                .post("service-accounts", json!({ "name": name, "description": description }))
                .await?;
            out.done(
                &created,
                &format!("Created service account {name} ({})", cell(&created, "id")),
            );
        }
        ServiceAccounts::Disable { account: name } => {
            let id = account(client, &name).await?;
            let answer = client
                .patch(&format!("service-accounts/{id}"), json!({ "disabled": true }))
                .await?;
            out.done(&answer, &format!("Disabled {name}; its tokens stop working at once"));
        }
        ServiceAccounts::Token(AccountTokens::Create { account: name, name: token_name, days }) => {
            let id = account(client, &name).await?;
            let body = json!({ "name": token_name, "expires_in_days": days });
            let created = client.post(&format!("service-accounts/{id}/tokens"), body).await?;
            out.show(&created, secret);
        }
        ServiceAccounts::Token(AccountTokens::Revoke { account: name, token_id }) => {
            let id = account(client, &name).await?;
            let answer = client.delete(&format!("service-accounts/{id}/tokens/{token_id}")).await?;
            out.done(&answer, &format!("Revoked {name}'s token {token_id}"));
        }
    }
    Ok(())
}

/// A user by ID, or by login, written `provider/login` when several providers share it.
async fn user(client: &Client, user: &str) -> Result<String, Failure> {
    if Uuid::parse_str(user).is_ok() {
        return Ok(user.to_string());
    }
    let (provider, login) = match user.split_once('/') {
        Some((provider, login)) => (Some(provider), login),
        None => (None, user),
    };
    let found = client.get(&format!("{RBAC}/users?login={}", encode(login))).await?;
    let matching: Vec<&Value> = found
        .as_array()
        .into_iter()
        .flatten()
        .filter(|found| provider.is_none_or(|provider| found["provider"] == provider))
        .collect();
    match matching.as_slice() {
        [one] => Ok(cell(one, "id")),
        [] => Err(Failure::Refused(format!("no user signs in as {user}"))),
        several => {
            let providers: Vec<String> =
                several.iter().map(|found| cell(found, "provider")).collect();
            Err(Failure::Refused(format!(
                "{login} signs in through {}; write provider/login",
                providers.join(" and ")
            )))
        }
    }
}

fn group_holder(group: &str) -> Result<Value, Failure> {
    let (plugin, name) = group
        .split_once('/')
        .ok_or_else(|| Failure::Refused(format!("write the group as plugin/name, not {group}")))?;
    Ok(json!({ "kind": "group", "plugin": plugin, "name": name }))
}

fn show_principal(view: &Value) {
    say!("{} ({})\n", cell(view, "label"), cell(view, "holder.kind"));
    let assignments = view["assignments"].as_array().cloned().unwrap_or_default();
    Table::of(
        &["PERMISSION", "SOURCE", "GRANTED BY", "GRANTED"],
        &assignments,
        &["permission", "source", "granted_by", "granted_at"],
    )
    .print();
    if let Some(access) = view["access"].as_object() {
        say!();
        let mut table = Table::new(&["PLUGIN", "ACCESS", "CUSTOM"]);
        for (plugin, effective) in access {
            let custom = effective["custom"].as_object().into_iter().flatten();
            let custom: Vec<String> =
                custom.map(|(name, scope)| format!("{name}:{}", cell(scope, ""))).collect();
            table.row(vec![plugin.clone(), cell(effective, "scope"), custom.join(", ")]);
        }
        table.print();
    }
}

async fn permissions(client: &Client, out: &Output, command: Permissions) -> Result<(), Failure> {
    match command {
        Permissions::List { user: Some(name), .. } => {
            let id = user(client, &name).await?;
            let view = client.get(&format!("{RBAC}/principals/user/{id}")).await?;
            out.show(&view, show_principal);
        }
        Permissions::List { sa: Some(name), .. } => {
            let id = account(client, &name).await?;
            let view = client.get(&format!("service-accounts/{id}/permissions")).await?;
            out.show(&view, show_principal);
        }
        Permissions::List { plugin: Some(plugin), .. } => {
            let view = client.get(&format!("{RBAC}/plugins/{plugin}")).await?;
            out.show(&view, |view| {
                let assignments = view["assignments"].as_array().cloned().unwrap_or_default();
                Table::of(
                    &["HOLDER", "KIND", "PERMISSION", "SOURCE"],
                    &assignments,
                    &["label", "holder.kind", "permission", "source"],
                )
                .print();
                say!();
                let groups = view["groups"].as_array().cloned().unwrap_or_default();
                group_table(&groups);
            });
        }
        Permissions::List { .. } => {
            return Err(Failure::Refused("name --user, --sa or --plugin".into()));
        }
        Permissions::Grant { permission, holder } => {
            let answer = match holder_of(client, &holder).await? {
                Whose::Account(id) => {
                    let body = json!({ "permission": permission });
                    client.post(&format!("service-accounts/{id}/permissions"), body).await?
                }
                Whose::Rbac(holder) => {
                    let body = json!({ "holder": holder, "permission": permission });
                    client.post(&format!("{RBAC}/assignments"), body).await?
                }
            };
            out.done(&answer, &format!("Granted {}", cell(&answer, "permission")));
        }
        Permissions::Revoke { permission, holder } => {
            let answer = match holder_of(client, &holder).await? {
                Whose::Account(id) => {
                    let path = format!("service-accounts/{id}/permissions/{}", encode(&permission));
                    client.delete(&path).await?
                }
                Whose::Rbac(holder) => {
                    let body = json!({ "holder": holder, "permission": permission });
                    client.post(&format!("{RBAC}/assignments/revoke"), body).await?
                }
            };
            out.done(&answer, &format!("Revoked {permission}"));
        }
    }
    Ok(())
}

/// Service accounts go through core, so their owners can grant within their own access (rule 6).
enum Whose {
    Account(String),
    Rbac(Value),
}

async fn holder_of(client: &Client, holder: &Holder) -> Result<Whose, Failure> {
    match (&holder.user, &holder.sa, &holder.group) {
        (Some(name), _, _) => {
            Ok(Whose::Rbac(json!({ "kind": "user", "id": user(client, name).await? })))
        }
        (_, Some(name), _) => Ok(Whose::Account(account(client, name).await?)),
        (_, _, Some(group)) => Ok(Whose::Rbac(group_holder(group)?)),
        _ => Err(Failure::Refused("name --user, --sa or --group".into())),
    }
}

fn group_table(groups: &[Value]) {
    Table::of(
        &["GROUP", "KIND", "MEMBERS", "PERMISSIONS", "DESCRIPTION"],
        groups,
        &["name", "kind", "members", "permissions", "description"],
    )
    .print();
}

async fn groups(client: &Client, out: &Output, command: Groups) -> Result<(), Failure> {
    match command {
        Groups::List { plugin } => {
            let path = match plugin {
                Some(plugin) => format!("{RBAC}/groups?plugin={}", encode(&plugin)),
                None => format!("{RBAC}/groups"),
            };
            let groups = client.get(&path).await?;
            out.show(&groups, |groups| {
                let groups = groups.as_array().cloned().unwrap_or_default();
                let mut table = Table::new(&["GROUP", "KIND", "MEMBERS", "PERMISSIONS"]);
                for group in &groups {
                    let name = format!("{}/{}", cell(group, "plugin"), cell(group, "name"));
                    let rest = ["kind", "members", "permissions"].map(|path| cell(group, path));
                    table.row([name].into_iter().chain(rest).collect());
                }
                table.print();
            });
        }
        Groups::Show { group } => {
            let holder = group_holder(&group)?;
            let path =
                format!("{RBAC}/groups/{}/{}", cell(&holder, "plugin"), cell(&holder, "name"));
            let shown = client.get(&path).await?;
            out.show(&shown, |shown| {
                pairs(
                    shown,
                    &[
                        ("Group", "group.name"),
                        ("Plugin", "group.plugin"),
                        ("Members are", "group.kind"),
                        ("Description", "group.description"),
                        ("Permissions", "group.permissions"),
                    ],
                );
                say!();
                let members = shown["members"].as_array().cloned().unwrap_or_default();
                Table::of(
                    &["MEMBER", "KIND", "SOURCE", "GRANTED BY"],
                    &members,
                    &["label", "holder.kind", "source", "granted_by"],
                )
                .print();
                if let Some(attributes) =
                    shown["attributes"].as_object().filter(|found| !found.is_empty())
                {
                    say!();
                    let mut table = Table::new(&["ATTRIBUTE", "VALUE"]);
                    for (key, value) in attributes {
                        table.row(vec![key.clone(), cell(value, "")]);
                    }
                    table.print();
                }
            });
        }
    }
    Ok(())
}

async fn tasks(client: &Client, out: &Output, command: Tasks) -> Result<(), Failure> {
    match command {
        Tasks::List { state, kind, mine, limit } => {
            let mut query = vec![format!("mine={mine}")];
            query.extend(state.map(|state| format!("state={}", encode(&state))));
            query.extend(kind.map(|kind| format!("kind={}", encode(&kind))));
            query.extend(limit.map(|limit| format!("limit={limit}")));
            let listed = client.get(&format!("tasks?{}", query.join("&"))).await?;
            out.show(&listed, |listed| {
                let listed = listed.as_array().cloned().unwrap_or_default();
                Table::of(
                    &["ID", "KIND", "STATE", "ATTEMPTS", "STARTED BY", "CREATED"],
                    &listed,
                    &["id", "kind", "state", "attempts", "started_by.label", "created_at"],
                )
                .print();
            });
        }
        Tasks::Show { id } => {
            let task = client.get(&format!("tasks/{id}")).await?;
            out.show(&task, show_task);
        }
        Tasks::Wait { id, timeout, interval } => wait(client, out, &id, timeout, interval).await?,
    }
    Ok(())
}

fn show_task(task: &Value) {
    pairs(
        task,
        &[
            ("Task", "id"),
            ("Kind", "kind"),
            ("State", "state"),
            ("Attempts", "attempts"),
            ("Started by", "started_by.label"),
            ("Created", "created_at"),
            ("Finished", "finished_at"),
            ("Result", "result"),
            ("Error", "error"),
        ],
    );
    if task["chained"].is_object() {
        let whole = &task["chained"];
        say!(
            "All of it  {}: {} of {} tasks finished, {} failed",
            cell(whole, "state"),
            cell(whole, "finished"),
            cell(whole, "tasks"),
            cell(whole, "failed")
        );
    }
}

/// Polls until the task finishes; a backend that stops answering is waited out like a slow task.
async fn wait(
    client: &Client,
    out: &Output,
    id: &str,
    timeout: u64,
    interval: u64,
) -> Result<(), Failure> {
    let started = Instant::now();
    let deadline = Duration::from_secs(timeout);
    loop {
        match client.get(&format!("tasks/{id}")).await {
            // A task that queued more while it ran is done when all of it is.
            Ok(task) => match task["chained"]["state"].as_str().or(task["state"].as_str()) {
                Some("succeeded") => {
                    out.show(&task, show_task);
                    return Ok(());
                }
                Some("failed") => {
                    out.show(&task, show_task);
                    let error = match cell(&task, "chained.error").as_str() {
                        "-" => cell(&task, "error"),
                        error => error.to_string(),
                    };
                    return Err(Failure::Refused(format!("task {id} failed: {error}")));
                }
                Some("cancelled") => {
                    out.show(&task, show_task);
                    return Err(Failure::Refused(format!("task {id} was cancelled")));
                }
                state if started.elapsed() >= deadline => {
                    let state = state.unwrap_or("unknown");
                    return Err(Failure::TimedOut(format!(
                        "task {id} is still {state} after {timeout} s"
                    )));
                }
                _ => {}
            },
            Err(Failure::Unreachable(err)) if started.elapsed() < deadline => {
                eprintln!("still waiting: {err}");
            }
            Err(Failure::Unreachable(err)) => {
                return Err(Failure::TimedOut(format!(
                    "gave up on task {id} after {timeout} s: {err}"
                )));
            }
            Err(other) => return Err(other),
        }
        tokio::time::sleep(Duration::from_secs(interval.max(1))).await;
    }
}

async fn resources(client: &Client, out: &Output, command: Resources) -> Result<(), Failure> {
    match command {
        Resources::Apply { file, dry_run } => {
            let text = match file.as_str() {
                "-" => std::io::read_to_string(std::io::stdin()),
                path => std::fs::read_to_string(path),
            }
            .map_err(|err| Failure::Refused(format!("{file} could not be read: {err}")))?;
            let path = format!("{RESOURCES}/apply{}", if dry_run { "?dry_run=true" } else { "" });
            let report = client.post_body(&path, "application/yaml", text).await?;
            out.show(&report, applied);
        }
        Resources::Get { kind, name: None } => {
            let listed = client
                .get(&format!("{RESOURCES}/resources?kind={}&limit=500", encode(&kind)))
                .await?;
            out.show(&listed, |listed| {
                let listed = listed.as_array().cloned().unwrap_or_default();
                Table::of(&["NAME", "TITLE", "OWNER"], &listed, &["name", "title", "owner"])
                    .print();
            });
        }
        Resources::Get { kind, name: Some(name) } => {
            let shown =
                client.get(&format!("{RESOURCES}/resources/{}/{name}", encode(&kind))).await?;
            out.show(&shown, show_resource);
        }
        Resources::Assign { service_account, resource } => {
            let body = json!({ "service_account": service_account, "to": resource });
            let answer = client.post(&format!("{RESOURCES}/assign"), body).await?;
            let message = match answer["added"] == true {
                true => format!("Assigned {service_account} to {resource}"),
                false => format!("{service_account} was already assigned to {resource}"),
            };
            out.done(&answer, &message);
        }
    }
    Ok(())
}

fn applied(report: &Value) {
    let changed = report["changed"].as_u64().unwrap_or_default();
    let unchanged = cell(report, "unchanged");
    match report["dry_run"] == true {
        true => {
            say!("Would change {changed}; {unchanged} already as described. Nothing was changed.")
        }
        false => say!("Changed {changed}; {unchanged} already as described."),
    }
    let changes = report["changes"].as_array().cloned().unwrap_or_default();
    if changes.is_empty() {
        return;
    }
    say!();
    let mut table = Table::new(&["CHANGE", "RESOURCE", "DETAIL"]);
    for change in &changes {
        let what = match cell(change, "resource").as_str() {
            "-" => cell(change, "definition"),
            resource => resource.to_string(),
        };
        let detail = match (cell(change, "to").as_str(), cell(change, "fields").as_str()) {
            ("-", fields) => fields.to_string(),
            (to, _) => format!("to {to}"),
        };
        table.row(vec![cell(change, "action"), what, detail]);
    }
    table.print();
}

fn show_resource(shown: &Value) {
    let resource = &shown["resource"];
    pairs(
        resource,
        &[
            ("Kind", "kind"),
            ("Name", "name"),
            ("Title", "title"),
            ("Description", "description"),
            ("Owner", "owner"),
            ("Email", "email"),
            ("From", "source"),
            ("Updated", "updated_at"),
        ],
    );
    if let Some(metadata) = resource["metadata"].as_object().filter(|metadata| !metadata.is_empty())
    {
        say!();
        let mut table = Table::new(&["METADATA", "VALUE"]);
        for (key, value) in metadata {
            table.row(vec![key.clone(), cell(value, "")]);
        }
        table.print();
    }
    say!();
    let mut table = Table::new(&["CONNECTED", "KIND", "NAME", "TITLE"]);
    for link in shown["connections"].as_array().into_iter().flatten() {
        let direction = match (cell(link, "direction").as_str(), link["derived"] == true) {
            ("in", false) => "above",
            ("in", true) => "above (rbac)",
            (_, false) => "below",
            (_, true) => "below (rbac)",
        };
        table.row(vec![
            direction.into(),
            cell(link, "kind"),
            cell(link, "name"),
            cell(link, "title"),
        ]);
    }
    table.print();
    for reason in shown["hidden"].as_array().into_iter().flatten() {
        eprintln!("Some of it is read from rbac as you, and it said: {}", cell(reason, ""));
    }
}

/// The largest archive `kb import` sends, which is what the backend lets through to a plugin.
const MAX_UPLOAD: usize = 16 * 1024 * 1024;

/// A directory as a `.tar.gz`, leaving out hidden files and an MkDocs build in `site/`.
fn packed(dir: &std::path::Path) -> Result<Vec<u8>, Failure> {
    fn add(
        archive: &mut tar::Builder<flate2::write::GzEncoder<Vec<u8>>>,
        root: &std::path::Path,
        dir: &std::path::Path,
    ) -> std::io::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(&path);
            if name.starts_with('.') || relative == std::path::Path::new("site") {
                continue;
            }
            if entry.file_type()?.is_dir() {
                add(archive, root, &path)?;
            } else if entry.file_type()?.is_file() {
                archive.append_path_with_name(&path, relative)?;
            }
        }
        Ok(())
    }
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let unreadable = |err: std::io::Error| {
        Failure::Refused(format!("{} could not be read: {err}", dir.display()))
    };
    add(&mut archive, dir, dir).map_err(unreadable)?;
    let bytes =
        archive.into_inner().and_then(flate2::write::GzEncoder::finish).map_err(unreadable)?;
    if bytes.len() > MAX_UPLOAD {
        return Err(Failure::Refused(format!("{} packs to more than 16 MiB", dir.display())));
    }
    Ok(bytes)
}

async fn kb(client: &Client, out: &Output, command: Kb) -> Result<(), Failure> {
    match command {
        Kb::Import { dir, space, resource, owners, wait: waiting, timeout } => {
            let bytes = packed(std::path::Path::new(&dir))?;
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            if let Some(space) = &space {
                query.append_pair("space", space);
            }
            if let Some(resource) = &resource {
                query.append_pair("resource", resource);
            }
            for owner in &owners {
                query.append_pair("owner", owner);
            }
            let path = format!("plugins/kb/api/imports?{}", query.finish());
            let started = client.post_body(&path, "application/gzip", bytes).await?;
            let task = cell(&started, "task");
            match waiting {
                true => {
                    eprintln!("Importing {} pages into {}, as task {task}", cell(&started, "pages"), cell(&started, "space"));
                    wait(client, out, &task, timeout, 2).await?;
                }
                false => out.done(
                    &started,
                    &format!(
                        "Importing {} pages into {}, as task {task}; follow it with `cli tasks wait {task}`",
                        cell(&started, "pages"),
                        cell(&started, "space")
                    ),
                ),
            }
        }
        Kb::Sources(KbSources::List) => {
            let listed = client.get("plugins/kb/api/sources").await?;
            out.show(&listed, |listed| {
                let sources = listed.as_array().cloned().unwrap_or_default();
                Table::of(
                    &["ID", "KIND", "SPACE", "SCHEDULE", "LAST SYNC", "STATE", "ERROR"],
                    &sources,
                    &[
                        "id",
                        "kind",
                        "space",
                        "schedule",
                        "last_sync_at",
                        "last_state",
                        "last_error",
                    ],
                )
                .print();
            });
        }
        Kb::Sources(KbSources::Add(source)) => {
            let SourceArgs {
                kind,
                space,
                resource,
                owners,
                schedule,
                repository,
                reference,
                path,
                flavour,
                site,
                space_key,
                credential,
                folder,
            } = *source;
            let body = json!({
                "kind": kind, "space": space, "resource": resource, "schedule": schedule,
                "owners": owners,
                "repository": repository, "ref": reference, "path": path, "flavour": flavour,
                "url": site, "space_key": space_key, "credential": credential, "folder": folder,
            });
            let body: serde_json::Map<String, Value> = body
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            let made = client.post("plugins/kb/api/sources", Value::Object(body)).await?;
            out.done(
                &made,
                &format!(
                    "Added {} source {} for space {}",
                    cell(&made, "kind"),
                    cell(&made, "id"),
                    cell(&made, "space")
                ),
            );
        }
        Kb::Sources(KbSources::Sync { id, wait: waiting, timeout }) => {
            let started =
                client.post(&format!("plugins/kb/api/sources/{id}/sync"), json!({})).await?;
            let task = cell(&started, "task");
            match waiting {
                true => wait(client, out, &task, timeout, 2).await?,
                false => out.done(
                    &started,
                    &format!("Syncing, as task {task}; follow it with `cli tasks wait {task}`"),
                ),
            }
        }
        Kb::Search { query, space, limit } => {
            let mut asked = url::form_urlencoded::Serializer::new(String::new());
            asked.append_pair("q", &query);
            if let Some(space) = &space {
                asked.append_pair("space", space);
            }
            if let Some(limit) = limit {
                asked.append_pair("limit", &limit.to_string());
            }
            let found = client.get(&format!("plugins/kb/api/search?{}", asked.finish())).await?;
            out.show(&found, |found| {
                let results = found["results"].as_array().cloned().unwrap_or_default();
                if results.is_empty() {
                    say!("Nothing matches.");
                }
                for hit in &results {
                    let plain = cell(hit, "snippet").replace("<mark>", "*").replace("</mark>", "*");
                    say!(
                        "{}  ({})\n  {}\n  {}\n",
                        cell(hit, "title"),
                        cell(hit, "space"),
                        cell(hit, "url"),
                        plain
                    );
                }
            });
        }
    }
    Ok(())
}

/// A trigger as one line: its kind, and its schedule, topic or queue.
fn trigger_line(trigger: &Value) -> String {
    let kind = cell(trigger, "type");
    match trigger["cron"].as_str().or(trigger["topic"].as_str()).or(trigger["queue"].as_str()) {
        Some(key) => format!("{kind} {key}"),
        None => kind,
    }
}

async fn automation(client: &Client, out: &Output, command: Automation) -> Result<(), Failure> {
    const API: &str = "plugins/automation/api";
    match command {
        Automation::List { resource } => {
            let query = resource
                .map(|resource| {
                    format!("?resource={}", byte_serialize(resource.as_bytes()).collect::<String>())
                })
                .unwrap_or_default();
            let listed = client.get(&format!("{API}/automations{query}")).await?;
            out.show(&listed, |listed| {
                let mut table = Table::new(&["ID", "NAME", "RESOURCE", "TRIGGER", "OWNER", "ON"]);
                for automation in listed.as_array().into_iter().flatten() {
                    table.row(vec![
                        cell(automation, "id"),
                        cell(automation, "name"),
                        cell(automation, "resource"),
                        trigger_line(&automation["trigger"]),
                        cell(automation, "owner"),
                        cell(automation, "enabled"),
                    ]);
                }
                table.print();
            });
        }
        Automation::Show { id } => {
            let shown = client.get(&format!("{API}/automations/{id}")).await?;
            out.show(&shown, |shown| {
                say!("{}  ({})", cell(shown, "name"), cell(shown, "id"));
                say!("  Resource   {}", cell(shown, "resource"));
                say!("  Runs as    {}", cell(shown, "owner"));
                say!("  Trigger    {}", trigger_line(&shown["trigger"]));
                if let Some(webhook) = shown["webhook"].as_str() {
                    say!("  Webhook    {webhook}");
                }
                say!("  On         {}", cell(shown, "enabled"));
                for condition in shown["conditions"].as_array().into_iter().flatten() {
                    say!(
                        "  Only if    {} {} {}",
                        cell(condition, "field"),
                        cell(condition, "op"),
                        condition["value"]
                    );
                }
                for (index, action) in shown["actions"].as_array().into_iter().flatten().enumerate()
                {
                    let mut rest = action.clone();
                    if let Some(object) = rest.as_object_mut() {
                        object.remove("type");
                    }
                    say!("  Action {}   {} {}", index + 1, cell(action, "type"), rest);
                }
            });
        }
        Automation::Run { id, payload, force, wait: waiting, timeout } => {
            let asked = json!({ "payload": payload, "force": force });
            let started = client.post(&format!("{API}/automations/{id}/run"), asked).await?;
            if started["matched"] == false {
                return Err(Failure::Refused(format!(
                    "its conditions turned the payload away: {}; --force runs it anyway",
                    cell(&started, "reason")
                )));
            }
            let run = cell(&started, "run");
            match waiting {
                true => {
                    eprintln!("Running, as run {run}");
                    let waited =
                        wait(client, &Output { json: true }, &cell(&started, "task"), timeout, 1)
                            .await;
                    let finished = client.get(&format!("{API}/runs/{run}")).await?;
                    out.show(&finished, show_run);
                    waited?;
                }
                false => out.done(
                    &started,
                    &format!(
                        "Queued run {run}; see how it went with `cli automation history {id}`"
                    ),
                ),
            }
        }
        Automation::History { id, limit } => {
            let limit = limit.map(|limit| format!("?limit={limit}")).unwrap_or_default();
            let runs = client.get(&format!("{API}/automations/{id}/runs{limit}")).await?;
            out.show(&runs, |runs| {
                let mut table =
                    Table::new(&["RUN", "QUEUED", "TRIGGER", "STATE", "ATTEMPTS", "ERROR"]);
                for run in runs.as_array().into_iter().flatten() {
                    table.row(vec![
                        cell(run, "id"),
                        cell(run, "created_at"),
                        cell(run, "input.trigger"),
                        cell(run, "state"),
                        format!("{}/{}", cell(run, "attempts"), cell(run, "max_attempts")),
                        cell(run, "error"),
                    ]);
                }
                table.print();
            });
        }
    }
    Ok(())
}

fn show_run(run: &Value) {
    say!(
        "Run {}  {}  attempts {}/{}",
        cell(run, "id"),
        cell(run, "state"),
        cell(run, "attempts"),
        cell(run, "max_attempts")
    );
    for (index, step) in run["steps"].as_array().into_iter().flatten().enumerate() {
        let detail = match step["error"].as_str() {
            Some(error) => error.to_string(),
            None => step["output"].to_string(),
        };
        say!("  {}. {} {}: {detail}", index + 1, cell(step, "action"), cell(step, "state"));
    }
    if let Some(error) = run["error"].as_str() {
        say!("  Error: {error}");
    }
}

async fn agenda(
    client: &Client,
    out: &Output,
    calendar: Option<String>,
    from: Option<String>,
    to: Option<String>,
) -> Result<(), Failure> {
    const API: &str = "plugins/calendar/api";
    let mut range = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in [("from", &from), ("to", &to)] {
        if let Some(value) = value {
            range.append_pair(key, value);
        }
    }
    let range = range.finish();
    let path = match &calendar {
        Some(resource) => {
            let (kind, name) = resource.split_once(':').ok_or_else(|| {
                Failure::Refused(format!("write the calendar as kind:name, not `{resource}`"))
            })?;
            let found = client.get(&format!("{API}/calendars/for/{kind}/{name}")).await?;
            format!("{API}/calendars/{}/agenda?{range}", cell(&found, "id"))
        }
        None => format!("{API}/agenda?{range}"),
    };
    let agenda = client.get(&path).await?;
    out.show(&agenda, |agenda| {
        let occurrences = agenda["occurrences"].as_array().cloned().unwrap_or_default();
        if occurrences.is_empty() {
            say!("Nothing between {} and {}.", cell(agenda, "from"), cell(agenda, "to"));
            return;
        }
        let mut table = Table::new(&["WHEN", "UNTIL", "TITLE", "CALENDAR", "WHERE"]);
        for occurrence in &occurrences {
            let calendar = match cell(occurrence, "calendar_name").as_str() {
                "-" => cell(occurrence, "calendar"),
                name => name.to_string(),
            };
            table.row(vec![
                cell(occurrence, "start"),
                cell(occurrence, "end"),
                cell(occurrence, "title"),
                calendar,
                cell(occurrence, "location"),
            ]);
        }
        table.print();
    });
    Ok(())
}

async fn processes(client: &Client, out: &Output, command: Processes) -> Result<(), Failure> {
    const API: &str = "plugins/process/api";
    match command {
        Processes::List { resource, mine } => {
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            if let Some(resource) = &resource {
                query.append_pair("resource", resource);
            }
            if mine {
                query.append_pair("mine", "true");
            }
            let listed = client.get(&format!("{API}/processes?{}", query.finish())).await?;
            out.show(&listed, |listed| {
                let processes = listed.as_array().cloned().unwrap_or_default();
                if processes.is_empty() {
                    say!("No processes{}.", if mine { " of yours" } else { "" });
                    return;
                }
                let mut table =
                    Table::new(&["ID", "TITLE", "RESOURCE", "WHEN", "NEXT DUE", "NEXT", "MISSED"]);
                for process in &processes {
                    table.row(vec![
                        cell(process, "id"),
                        cell(process, "title"),
                        cell(process, "resource"),
                        cell(process, "schedule"),
                        cell(process, "next.due_at"),
                        cell(process, "next.id"),
                        cell(process, "missed"),
                    ]);
                }
                table.print();
            });
        }
        Processes::Show { id } => {
            let process = match client.get(&format!("{API}/processes/{id}")).await {
                Ok(process) => process,
                Err(Failure::Refused(_)) => {
                    let occurrence = client.get(&format!("{API}/occurrences/{id}")).await.map_err(
                        |failure| match failure {
                            Failure::Refused(_) => Failure::Refused(format!(
                                "there is no process or occurrence {id} that you can see"
                            )),
                            other => other,
                        },
                    )?;
                    out.show(&occurrence, show_occurrence);
                    return Ok(());
                }
                Err(other) => return Err(other),
            };
            out.show(&process, show_process);
        }
        Processes::Complete { id, all, note } => {
            let asked = json!({ "all": all, "note": note });
            let done = client.post(&format!("{API}/occurrences/{id}/complete"), asked).await?;
            out.done(
                &done,
                &format!("Done: {}, due {}", cell(&done, "title"), cell(&done, "due_at")),
            );
        }
    }
    Ok(())
}

fn show_process(process: &Value) {
    say!("{}  ({})", cell(process, "title"), cell(process, "id"));
    let reminder = match cell(process, "remind_minutes").as_str() {
        "-" => "none".to_string(),
        minutes => format!("{minutes} minutes before"),
    };
    say!("  Resource     {}", cell(process, "resource"));
    say!("  When         {}", cell(process, "schedule"));
    say!("  Owner        {}", cell(process, "owner"));
    let assigned: Vec<String> = process["assignees"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|assignee| match assignee["team"].as_str() {
            Some(team) => format!("team {team}"),
            None => cell(assignee, "user"),
        })
        .collect();
    let assigned = match assigned.is_empty() {
        true => cell(process, "owner"),
        false => assigned.join(", "),
    };
    say!("  Assigned to  {assigned}");
    say!("  Reminder     {reminder}");
    say!("  Missed       {} minutes after it is due", cell(process, "grace_minutes"));
    for (index, item) in process["checklist"].as_array().into_iter().flatten().enumerate() {
        say!(
            "  {:<11}  {}. {}",
            if index == 0 { "Checklist" } else { "" },
            index + 1,
            cell(item, "")
        );
    }
    let occurrences = process["occurrences"].as_array().cloned().unwrap_or_default();
    let (open, finished): (Vec<&Value>, Vec<&Value>) =
        occurrences.iter().partition(|occurrence| occurrence["status"] == "pending");
    let mut table = Table::new(&["OCCURRENCE", "DUE", "STATUS", "BY"]);
    for occurrence in finished.iter().rev().take(10).rev().chain(open.iter().take(5)) {
        table.row(vec![
            cell(occurrence, "id"),
            cell(occurrence, "due_at"),
            cell(occurrence, "status"),
            cell(occurrence, "finished_by"),
        ]);
    }
    say!("");
    table.print();
}

fn show_occurrence(occurrence: &Value) {
    say!(
        "{}, due {}  ({})",
        cell(occurrence, "title"),
        cell(occurrence, "due_at"),
        cell(occurrence, "id")
    );
    say!("  Resource     {}", cell(occurrence, "resource"));
    say!("  Status       {}", cell(occurrence, "status"));
    say!("  Assigned to  {}", cell(occurrence, "assignees"));
    if occurrence["finished_by"].is_string() {
        say!(
            "  Finished by  {} at {}",
            cell(occurrence, "finished_by"),
            cell(occurrence, "finished_at")
        );
    }
    if occurrence["note"].as_str().is_some_and(|note| !note.is_empty()) {
        say!("  Note         {}", cell(occurrence, "note"));
    }
    for (index, item) in occurrence["checklist"].as_array().into_iter().flatten().enumerate() {
        let (mark, by) = match item["done"] == true {
            true => ("[x]", format!("  ({}, {})", cell(item, "by"), cell(item, "at"))),
            false => ("[ ]", String::new()),
        };
        say!("  {mark} {}. {}{by}", index + 1, cell(item, "text"));
    }
    say!("  {}", cell(occurrence, "url"));
}

fn show_request(request: &Value) {
    say!("{}  ({})", cell(request, "name"), cell(request, "id"));
    pairs(
        request,
        &[
            ("  Status", "status"),
            ("  Vendor", "vendor"),
            ("  Type", "type"),
            ("  Region", "region"),
            ("  Size", "size"),
            ("  Team", "team"),
            ("  Service", "service"),
            ("  Expires", "expires_at"),
            ("  Resource", "resource"),
            ("  Error", "error"),
        ],
    );
}

async fn infra(client: &Client, out: &Output, command: Infra) -> Result<(), Failure> {
    const API: &str = "plugins/infra/api";
    match command {
        Infra::Templates => {
            let listed = client.get(&format!("{API}/templates")).await?;
            out.show(&listed, |listed| {
                let mut table = Table::new(&[
                    "NAME", "TITLE", "VENDOR", "TYPE", "REGIONS", "SIZES", "LIFETIME", "PER TEAM",
                ]);
                for template in listed.as_array().into_iter().flatten() {
                    table.row(vec![
                        cell(template, "name"),
                        cell(template, "title"),
                        cell(template, "vendor"),
                        cell(template, "type"),
                        cell(template, "regions"),
                        cell(template, "sizes"),
                        format!(
                            "{} (at most {})",
                            cell(template, "default_lifetime"),
                            cell(template, "max_lifetime")
                        ),
                        cell(template, "team_quota"),
                    ]);
                }
                table.print();
            });
        }
        Infra::Request {
            template,
            name,
            team,
            service,
            region,
            size,
            lifetime,
            wait: waiting,
            timeout,
        } => {
            let asked = json!({
                "template": template, "name": name, "team": team, "service": service,
                "region": region, "size": size, "lifetime": lifetime,
            });
            let made = client.post(&format!("{API}/requests"), asked).await?;
            let id = cell(&made, "id");
            if !waiting {
                out.done(&made, &format!("Requested {id}; follow it with `cli infra list`"));
                return Ok(());
            }
            let waited =
                wait(client, &Output { json: true }, &cell(&made, "task"), timeout, 1).await;
            let now = client.get(&format!("{API}/requests/{id}")).await?;
            out.show(&now, show_request);
            waited?;
            if cell(&now, "status") == "failed" {
                return Err(Failure::Refused(format!("it failed: {}", cell(&now, "error"))));
            }
        }
        Infra::List { team, all } => {
            let mut query = url::form_urlencoded::Serializer::new(String::new());
            if let Some(team) = &team {
                query.append_pair("team", team);
            }
            if all {
                query.append_pair("all", "true");
            }
            let listed = client.get(&format!("{API}/requests?{}", query.finish())).await?;
            out.show(&listed, |listed| {
                let requests = listed.as_array().cloned().unwrap_or_default();
                if requests.is_empty() {
                    say!("No resources.");
                    return;
                }
                let mut table = Table::new(&[
                    "ID", "NAME", "VENDOR", "TYPE", "REGION", "TEAM", "STATUS", "EXPIRES",
                ]);
                for request in &requests {
                    table.row(vec![
                        cell(request, "id"),
                        cell(request, "name"),
                        cell(request, "vendor"),
                        cell(request, "type"),
                        cell(request, "region"),
                        cell(request, "team"),
                        cell(request, "status"),
                        cell(request, "expires_at"),
                    ]);
                }
                table.print();
            });
        }
        Infra::Extend { id, by } => {
            let extended =
                client.post(&format!("{API}/requests/{id}/extend"), json!({ "by": by })).await?;
            out.done(&extended, &format!("It now expires {}", cell(&extended, "expires_at")));
        }
        Infra::Delete { id } => {
            let deleting = client.post(&format!("{API}/requests/{id}/delete"), json!({})).await?;
            out.done(
                &deleting,
                &format!(
                    "Deleting {}; follow it with `cli infra list --all`",
                    cell(&deleting, "name")
                ),
            );
        }
    }
    Ok(())
}
