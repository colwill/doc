//! `cli`: the command line for the DOC backend API. Output is a table, or the API's own JSON with
//! `--json`; the URL and token come from flags, `DOC_URL` and `DOC_TOKEN`, or `cli login`.

mod client;
mod commands;
mod credentials;
mod output;

use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use doc_secret::Secret;

const EXIT_CODES: &str = "Exit codes: 0 success; 1 refused by the backend, or the task waited for \
failed or was cancelled; 2 wrong usage; 3 the wait timed out; 4 the backend could not be reached \
or refused the token.";

fn secret(value: &str) -> Result<Secret<String>, String> {
    Ok(Secret::new(value.to_string()))
}

#[derive(Parser)]
#[command(name = "cli", version, about = "Command line for the DOC platform", after_help = EXIT_CODES)]
pub struct Cli {
    /// The backend's URL [default: the one `cli login` saved, else http://127.0.0.1:8080]
    #[arg(long, env = "DOC_URL", global = true)]
    url: Option<String>,
    /// A personal access or service account token [default: the one `cli login` saved]
    #[arg(long, env = "DOC_TOKEN", global = true, hide_env_values = true, value_parser = secret)]
    token: Option<Secret<String>>,
    /// Print the API's JSON instead of a table
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check --token against the backend, then save it and the URL for later commands
    Login,
    /// Forget the saved token
    Logout,
    /// Show who the token belongs to
    Whoami,
    /// Show the platform's health: each component and each plugin
    Status,
    /// List, reload, unload and cancel plugins, and manage their registration tokens
    #[command(subcommand)]
    Plugins(Plugins),
    /// Create, list and disable service accounts, and issue and revoke their tokens
    #[command(subcommand)]
    Sa(ServiceAccounts),
    /// List, grant and revoke permissions (the RBAC plugin's, or a service account you own)
    #[command(subcommand)]
    Permissions(Permissions),
    /// List and show the RBAC plugin's groups, which are its roles
    #[command(subcommand)]
    Groups(Groups),
    /// List and show background tasks, or wait for one to finish
    #[command(subcommand)]
    Tasks(Tasks),
    /// Apply, show and assign resources in Resource Definitions
    #[command(subcommand)]
    Resources(Resources),
    /// Sync GitHub's repositories, teams and members into Resource Definitions
    #[command(subcommand)]
    Github(Github),
    /// Import documents into the Knowledge Base, and search it
    #[command(subcommand)]
    Kb(Kb),
    /// List, show and run automations, and see how their runs went
    #[command(subcommand)]
    Automation(Automation),
    /// What is coming up, on your calendars or on one resource's
    #[command(subcommand)]
    Calendar(Calendar),
    /// List and show recurring processes, and mark their occurrences done
    #[command(subcommand)]
    Process(Processes),
    /// Self-service cloud resources: templates, requests, and extending or deleting what you have
    #[command(subcommand)]
    Infra(Infra),
}

#[derive(Subcommand)]
enum Infra {
    /// The templates, with what each allows
    Templates,
    /// Ask for a resource from a template; exits 1 when it is refused
    Request {
        /// The template's name, as `infra templates` shows it
        template: String,
        /// The resource's name: 3 to 30 lowercase letters, digits and hyphens
        #[arg(long)]
        name: String,
        /// The owning team, one you are in
        #[arg(long)]
        team: String,
        /// The service it is for
        #[arg(long)]
        service: Option<String>,
        #[arg(long)]
        region: Option<String>,
        #[arg(long)]
        size: Option<String>,
        /// How long it lives, such as 7d or 12h
        #[arg(long)]
        lifetime: Option<String>,
        /// Wait until it is made; exits 1 if it fails, 3 on timeout
        #[arg(long)]
        wait: bool,
        /// Seconds to wait before giving up
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Your teams' resources, or one team's
    List {
        #[arg(long)]
        team: Option<String>,
        /// Deleted ones too
        #[arg(long)]
        all: bool,
    },
    /// Keep a resource longer, up to its template's longest lifetime
    Extend {
        /// The request's ID, as `infra list` shows it
        id: String,
        /// How much longer, such as 7d or 12h
        #[arg(long)]
        by: String,
    },
    /// Delete a resource now rather than when it expires
    Delete {
        /// The request's ID, as `infra list` shows it
        id: String,
    },
}

#[derive(Subcommand)]
enum Processes {
    /// Processes you can see, on one resource, or only your own
    List {
        /// A resource, written kind:name, such as service:card-gateway
        #[arg(long)]
        resource: Option<String>,
        /// Only those you own or are assigned to, yourself or through a team
        #[arg(long)]
        mine: bool,
    },
    /// A process with its occurrences, or one occurrence with its checklist
    Show {
        /// A process's or an occurrence's ID, as `process list` and `process show` show them
        id: String,
    },
    /// Mark an occurrence done; exits 1 while its checklist has items left, unless --all
    Complete {
        /// The occurrence's ID, as `process list` and `process show` show them
        id: String,
        /// Tick off whatever is left on its checklist
        #[arg(long)]
        all: bool,
        /// A note for whoever does it next
        #[arg(long)]
        note: Option<String>,
    },
}

#[derive(Subcommand)]
enum Calendar {
    /// Occurrences, soonest first: your own calendar and those you subscribe to, or one resource's
    Agenda {
        /// A resource's calendar, written kind:name, such as team:payments-core
        #[arg(long)]
        calendar: Option<String>,
        /// From this date or time [default: now]
        #[arg(long)]
        from: Option<String>,
        /// Until this date or time [default: four weeks after --from]
        #[arg(long)]
        to: Option<String>,
    },
}

#[derive(Subcommand)]
enum Automation {
    /// Every automation, or only those on one resource
    List {
        /// A resource, written kind:name, such as service:card-gateway
        #[arg(long)]
        resource: Option<String>,
    },
    /// One automation: its trigger, conditions and actions
    Show {
        /// The automation's ID, as `automation list` shows it
        id: String,
    },
    /// Run one now, as its owner; exits 1 if its conditions turn the payload away
    Run {
        /// The automation's ID, as `automation list` shows it
        id: String,
        /// JSON, as its trigger would bring it; its conditions read it as `payload`
        #[arg(long, default_value = "{}", value_parser = json_arg)]
        payload: serde_json::Value,
        /// Run it whatever its conditions say
        #[arg(long)]
        force: bool,
        /// Wait for the run; exits 0 once it succeeded, 1 if it failed, 3 on timeout
        #[arg(long)]
        wait: bool,
        /// Seconds to wait before giving up
        #[arg(long, default_value_t = 120)]
        timeout: u64,
    },
    /// Its latest runs, newest first, with how each went
    History {
        /// The automation's ID, as `automation list` shows it
        id: String,
        #[arg(long)]
        limit: Option<u32>,
    },
}

#[derive(Subcommand)]
enum Kb {
    /// Import a directory of Markdown, or an MkDocs project, into a space
    Import {
        /// The directory, such as examples/mkdocs
        dir: String,
        /// The space to import into [default: the project's site name, or `docs`]
        #[arg(long)]
        space: Option<String>,
        /// The resource its pages document, written kind:name, such as service:card-gateway
        #[arg(long)]
        resource: Option<String>,
        /// Who looks after the space, written team:<name> or organisation:<name>; give it more
        /// than once for several teams. Needed only when the space is new.
        #[arg(long = "owner")]
        owners: Vec<String>,
        /// Wait for all of it; exits 0 once it has finished, 1 if it failed, 3 on timeout
        #[arg(long)]
        wait: bool,
        /// Seconds to wait before giving up
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },
    /// Search every space, best matches first
    Search {
        query: String,
        #[arg(long)]
        space: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// List, add and sync the sources that fill spaces on their own
    #[command(subcommand)]
    Sources(KbSources),
}

#[derive(Subcommand)]
enum KbSources {
    /// Every source, with its schedule and how its last sync went
    List,
    /// Add a GitHub repository, a Confluence space or a Google Drive folder as a source
    Add(Box<SourceArgs>),
    /// Sync a source now
    Sync {
        /// The source's ID, as `kb sources list` shows it
        id: String,
        /// Wait for all of it; exits 0 once it has finished, 1 if it failed, 3 on timeout
        #[arg(long)]
        wait: bool,
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },
}

#[derive(Subcommand)]
enum Github {
    /// Start a sync of every allowed organisation now, as its schedule would
    Sync {
        /// Wait for it to finish; exits 0 if it succeeded, 1 if it failed, 3 on timeout
        #[arg(long)]
        wait: bool,
        /// Seconds to wait before giving up
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// The plugin to ask, `github` or `ghe`
        #[arg(long, default_value = "github")]
        plugin: String,
    },
}

#[derive(Subcommand)]
enum Resources {
    /// Create or update resources, connections and definitions from YAML or JSON documents
    Apply {
        /// The file to apply, or - for standard input
        #[arg(short = 'f', long = "file")]
        file: String,
        /// Show what would change, and change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Every resource of a kind, or one with its connections
    Get {
        /// A kind, such as organisation, service, team, repository, user or role
        kind: String,
        /// The resource's name; leave it out to list the kind
        name: Option<String>,
    },
    /// Assign a service account to a resource, as its owner or an RBAC admin
    Assign {
        /// The service account's name
        service_account: String,
        /// The resource, written kind:name, such as service:card-gateway
        resource: String,
    },
}

#[derive(Subcommand)]
enum Plugins {
    /// Every known plugin with its version, classification, state and last error
    List,
    /// One plugin, its registration and its state history
    Show {
        /// The plugin's ID, such as `kb`
        id: String,
    },
    /// Unload and load a plugin again in the same process, keeping what it hands over
    Reload {
        /// The plugin's ID, such as `kb`
        id: String,
    },
    /// Unload a plugin, saving what it hands over, and end its process
    Unload {
        /// The plugin's ID, such as `kb`
        id: String,
    },
    /// Stop a plugin's work until it is resumed
    Cancel {
        /// The plugin's ID, such as `kb`
        id: String,
    },
    /// Manage a plugin's registration tokens
    #[command(subcommand)]
    Token(PluginTokens),
}

#[derive(Subcommand)]
enum PluginTokens {
    /// A plugin's registration tokens, without their secrets
    List { plugin: String },
    /// Issue a registration token; its secret is shown once
    Create {
        plugin: String,
        #[arg(long)]
        name: Option<String>,
        /// Days until it expires (1 to 365); never, if left out
        #[arg(long)]
        days: Option<i64>,
    },
    /// Revoke a registration token by its ID
    Revoke { plugin: String, token_id: String },
}

#[derive(Subcommand)]
enum ServiceAccounts {
    /// Your service accounts, or every one with plugin:rbac:user:rw
    List,
    /// Create a service account, which you own
    Create {
        name: String,
        #[arg(long)]
        description: Option<String>,
    },
    /// Disable a service account, by name or ID
    Disable { account: String },
    /// Issue and revoke a service account's tokens
    #[command(subcommand)]
    Token(AccountTokens),
}

#[derive(Subcommand)]
enum AccountTokens {
    /// Issue a token; its secret is shown once
    Create {
        account: String,
        #[arg(long)]
        name: Option<String>,
        /// Days until it expires (1 to 365); never, if left out
        #[arg(long)]
        days: Option<i64>,
    },
    /// Revoke one of the account's tokens by its ID
    Revoke { account: String, token_id: String },
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct Holder {
    /// A user, by ID, login or provider/login
    #[arg(long)]
    user: Option<String>,
    /// A service account, by name or ID
    #[arg(long)]
    sa: Option<String>,
    /// A group, written plugin/name
    #[arg(long)]
    group: Option<String>,
}

#[derive(Subcommand)]
enum Permissions {
    /// A principal's permissions, or everything granted in one plugin
    #[command(group = clap::ArgGroup::new("whose").required(true))]
    List {
        /// A user, by ID, login or provider/login
        #[arg(long, group = "whose")]
        user: Option<String>,
        /// A service account, by name or ID
        #[arg(long, group = "whose")]
        sa: Option<String>,
        /// Everything granted in this plugin, including `core`
        #[arg(long, group = "whose")]
        plugin: Option<String>,
    },
    /// Grant a permission such as plugin:kb:user:rw, or group membership as plugin:kb:group:editors
    Grant {
        /// plugin:<plugin>:user|service[:ro|rw|wo], plugin:<plugin>:group:<name>, or a custom one
        permission: String,
        #[command(flatten)]
        holder: Holder,
    },
    /// Revoke a permission or group membership
    Revoke {
        /// The permission as it was granted
        permission: String,
        #[command(flatten)]
        holder: Holder,
    },
}

#[derive(Subcommand)]
enum Groups {
    /// Every group, or one plugin's
    List {
        #[arg(long)]
        plugin: Option<String>,
    },
    /// A group's permissions, members and attributes, written plugin/name
    Show { group: String },
}

#[derive(Subcommand)]
enum Tasks {
    /// Recent tasks, newest first
    List {
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        /// Only tasks you started
        #[arg(long)]
        mine: bool,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// One task, with its result or error
    Show {
        /// The task's ID, as `tasks list` or the call that started it gave it
        id: String,
    },
    /// Wait for a task to finish; exits 0 if it succeeded, 1 if it failed or was cancelled, 3 on timeout
    Wait {
        /// The task's ID, as `tasks list` or the call that started it gave it
        id: String,
        /// Seconds to wait before giving up
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Seconds between checks
        #[arg(long, default_value_t = 2)]
        interval: u64,
    },
}

fn json_arg(text: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(text).map_err(|err| format!("not JSON: {err}"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let _ = rustls::crypto::ring::default_provider().install_default();
    match commands::run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("error: {}", failure.message());
            failure.exit_code()
        }
    }
}

#[derive(Args)]
pub struct SourceArgs {
    /// github, confluence or drive
    kind: String,
    /// The Knowledge Base space it fills
    #[arg(long)]
    space: String,
    /// The resource its pages document, written kind:name
    #[arg(long)]
    resource: Option<String>,
    /// Who looks after the space, written team:<name> or organisation:<name>; give it more than
    /// once for several teams. Needed only when the space is new.
    #[arg(long = "owner")]
    owners: Vec<String>,
    /// When to sync it on its own: cron, in UTC, such as "0 * * * *"
    #[arg(long)]
    schedule: Option<String>,
    /// GitHub: the repository, written owner/name
    #[arg(long)]
    repository: Option<String>,
    /// GitHub: the branch, tag or commit [default: HEAD]
    #[arg(long = "ref")]
    reference: Option<String>,
    /// GitHub: the directory within the repository
    #[arg(long)]
    path: Option<String>,
    /// Confluence: cloud or datacenter
    #[arg(long)]
    flavour: Option<String>,
    /// Confluence: the site's URL
    #[arg(long)]
    site: Option<String>,
    /// Confluence: the space's key
    #[arg(long)]
    space_key: Option<String>,
    /// Confluence and Drive: the credential's name, set as DOC_KB_CREDENTIAL_<NAME> for kb
    #[arg(long)]
    credential: Option<String>,
    /// Drive: the folder or shared drive's ID
    #[arg(long)]
    folder: Option<String>,
}
