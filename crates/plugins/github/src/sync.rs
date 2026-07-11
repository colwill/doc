//! The sync: each allowed organisation's teams and their members into core, which owns teams
//! (T68), and its repositories published for Resource Definitions, with what went since the last
//! sync published as removed. Somebody the organisation no longer lists is reported as gone, and
//! the offboarding rules decide what follows (T67). It also hands other plugins archive links,
//! read with the same access.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{Backend, PluginError, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::github::{GitHub, Member, OrgTeam, Repository};
use crate::settings::{Access, AppKey, Settings};
use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::protocol::calls::{
    DeprovisionRequest, TeamMembersRequest, TeamRemoveRequest, TeamRequest, UserRequest,
};
use uuid::Uuid;

/// Documents per event, so each stays well inside what one event carries.
const CHUNK: usize = 100;
/// GitHub's archive links last about five minutes; say less, so nobody is caught out.
const LINK_LIFETIME: i64 = 4;
/// App tokens last an hour; one is used for at most this much less.
const TOKEN_MARGIN: i64 = 5;

/// App installation tokens by organisation, kept until shortly before they expire.
/// An installation token and when it stops working.
type Held = (Secret<String>, DateTime<Utc>);

#[derive(Default)]
pub struct Tokens(Mutex<HashMap<String, Held>>);

fn url_safe(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The JWT a GitHub App signs for itself, good for nine minutes.
fn app_jwt(id: &str, key: &AppKey) -> Result<String, String> {
    use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
    let pair = match key.pkcs8 {
        true => RsaKeyPair::from_pkcs8(key.der.expose()),
        false => RsaKeyPair::from_der(key.der.expose()),
    }
    .map_err(|err| format!("the App's private key is not an RSA key: {err}"))?;
    let now = Utc::now().timestamp();
    let header = url_safe(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims =
        url_safe(json!({ "iat": now - 60, "exp": now + 540, "iss": id }).to_string().as_bytes());
    let signed = format!("{header}.{claims}");
    let mut signature = vec![0; pair.public().modulus_len()];
    pair.sign(
        &RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        signed.as_bytes(),
        &mut signature,
    )
    .map_err(|_| "the JWT could not be signed".to_string())?;
    Ok(format!("{signed}.{}", url_safe(&signature)))
}

impl Tokens {
    /// What to read `org` with: the token as configured, or the App's installation token for it.
    pub async fn for_org(
        &self,
        github: &GitHub,
        settings: &Settings,
        org: &str,
    ) -> Result<Secret<String>, String> {
        let access = settings.access.as_ref().ok_or(
            "nothing to read organisations with: set a token, or a GitHub App's ID and private key",
        )?;
        let (id, key) = match access {
            Access::Token(token) => return Ok(token.clone()),
            Access::App { id, key } => (id, key),
        };
        let mut held = self.0.lock().await;
        if let Some((token, expires)) = held.get(org)
            && *expires - Duration::minutes(TOKEN_MARGIN) > Utc::now()
        {
            return Ok(token.clone());
        }
        let issued = github.installation_token(settings, &app_jwt(id, key)?, org).await?;
        held.insert(org.to_string(), (issued.token.clone(), issued.expires_at));
        Ok(issued.token)
    }
}

/// What one sync left behind for an organisation, so the next can say what went. Version 0 is
/// what the sync kept before teams moved into core, when `teams` held slugs published to Resource
/// Definitions rather than core's keys.
#[derive(Default, Serialize, Deserialize)]
struct Published {
    #[serde(default)]
    version: u8,
    teams: BTreeSet<String>,
    repositories: BTreeSet<String>,
    /// Everybody the organisation listed, by GitHub's own number for them.
    #[serde(default)]
    members: BTreeSet<String>,
}

const STATE_VERSION: u8 = 1;

/// A team's key in core: the organisation and the slug, which GitHub never reuses.
fn key(org: &str, slug: &str) -> String {
    format!("{}/{slug}", org.to_ascii_lowercase())
}

/// Teams with their parents before them, so a sub-team's parent is already in core. A team whose
/// parent is not in the list (one the token cannot see) stands at the top rather than going
/// missing.
fn parents_first(teams: Vec<OrgTeam>) -> Vec<OrgTeam> {
    let known: BTreeSet<String> = teams.iter().map(|team| team.slug.clone()).collect();
    let mut waiting: Vec<OrgTeam> = teams
        .into_iter()
        .map(|mut team| {
            if !team.parent.as_ref().is_some_and(|parent| known.contains(&parent.slug)) {
                team.parent = None;
            }
            team
        })
        .collect();
    let mut placed: BTreeSet<String> = BTreeSet::new();
    let mut ordered: Vec<OrgTeam> = Vec::with_capacity(waiting.len());
    while !waiting.is_empty() {
        let (ready, rest): (Vec<OrgTeam>, Vec<OrgTeam>) = waiting.into_iter().partition(|team| {
            team.parent.as_ref().is_none_or(|parent| placed.contains(&parent.slug))
        });
        if ready.is_empty() {
            // A cycle, which GitHub does not make: take the rest as they are rather than loop.
            ordered.extend(rest);
            break;
        }
        placed.extend(ready.iter().map(|team| team.slug.clone()));
        ordered.extend(ready);
        waiting = rest;
    }
    ordered
}

fn state_key(org: &str) -> String {
    format!("sync/{}", org.to_ascii_lowercase())
}

/// A repository still names the teams that can reach it, although the teams themselves are kept in
/// core rather than the Catalogue now (T68): as metadata, which stands on its own, and as
/// connections, which find their teams again when the Catalogue reads core's (T69).
fn repository_document(repository: &Repository, teams: Option<&BTreeSet<String>>) -> Value {
    let teams: Vec<&String> = teams.map(|teams| teams.iter().collect()).unwrap_or_default();
    json!({
        "kind": "Repository",
        "name": repository.full_name,
        "title": repository.name,
        "description": repository.description.clone().unwrap_or_default(),
        "metadata": {
            "url": repository.html_url,
            "default_branch": repository.default_branch,
            "visibility": repository.visibility,
            "archived": repository.archived,
            "language": repository.language,
            "pushed_at": repository.pushed_at,
            "teams": teams,
        },
        "connections": { "Teams": teams },
    })
}

async fn publish(
    backend: &Backend,
    plugin: &str,
    action: &str,
    documents: Vec<Value>,
) -> Result<(), PluginError> {
    let topic = format!("plugin.{plugin}.resources.{action}");
    for chunk in documents.chunks(CHUNK) {
        backend.publish(&topic, json!({ "documents": chunk })).await?;
    }
    Ok(())
}

/// The DOC user one of GitHub's accounts belongs to, made with it if nobody has it, so somebody
/// can be in a team here before they have ever signed in.
async fn user_of(
    backend: &Backend,
    plugin: &str,
    settings: &Settings,
    member: &Member,
) -> Result<Uuid, PluginError> {
    let answer = backend
        .provide_user(UserRequest {
            provider: plugin.to_string(),
            external_id: member.id.to_string(),
            login: member.login.clone(),
            organisation: settings.doc_organisation.clone(),
            ..UserRequest::default()
        })
        .await?;
    Ok(answer.user_id)
}

/// The names teams here already answer to, by the key of the provider's team that holds each one.
/// A team made in DOC, or another organisation's synced team, holds its name with no key.
async fn names_here(backend: &Backend) -> Result<BTreeMap<String, String>, PluginError> {
    #[derive(Deserialize)]
    struct Known {
        name: String,
        #[serde(default)]
        external_id: Option<String>,
    }
    let known: Vec<Known> =
        backend.query_all(Query::new("core.teams").fields(&["name", "external_id"])).await?;
    Ok(known.into_iter().map(|team| (team.name, team.external_id.unwrap_or_default())).collect())
}

/// One of the provider's teams in core, by the name it has on GitHub — or, where another team here
/// already answers to that name, the organisation's name in front of it, rather than a sync that
/// stops at the clash.
async fn team_in_core(
    backend: &Backend,
    settings: &Settings,
    names: &BTreeMap<String, String>,
    org: &str,
    team: &OrgTeam,
) -> Result<(), PluginError> {
    let external_id = key(org, &team.slug);
    let taken = names.get(&team.slug).is_some_and(|holder| holder != &external_id);
    let name = match taken {
        true => {
            let with_org = format!("{}-{}", org.to_ascii_lowercase(), team.slug);
            tracing::info!(org, team = %team.slug, name = %with_org, "that team name is taken here");
            with_org
        }
        false => team.slug.clone(),
    };
    backend
        .provide_team(TeamRequest {
            organisation: settings.doc_organisation.clone().unwrap_or_default(),
            external_id,
            name,
            title: team.name.clone(),
            description: team.description.clone().unwrap_or_default(),
            parent: team.parent.as_ref().map(|parent| key(org, &parent.slug)),
        })
        .await?;
    Ok(())
}

/// One organisation: its teams into core, parents first, each with the members GitHub lists, then
/// its repositories for Resource Definitions.
async fn organisation(
    backend: &Backend,
    github: &GitHub,
    settings: &Settings,
    tokens: &Tokens,
    plugin: &str,
    org: &str,
) -> Result<Value, PluginError> {
    let token = tokens.for_org(github, settings, org).await.map_err(PluginError::from)?;
    let teams = parents_first(
        github.teams(settings, token.expose(), org).await.map_err(PluginError::from)?,
    );
    let mut reach: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut users: HashMap<u64, Uuid> = HashMap::new();
    let mut memberships = 0;
    let names = names_here(backend).await?;
    for team in &teams {
        team_in_core(backend, settings, &names, org, team).await?;
        let members = github
            .members(settings, token.expose(), org, &team.slug)
            .await
            .map_err(PluginError::from)?;
        let mut in_team = Vec::with_capacity(members.len());
        for member in &members {
            let user = match users.get(&member.id) {
                Some(user) => *user,
                None => {
                    let user = user_of(backend, plugin, settings, member).await?;
                    users.insert(member.id, user);
                    user
                }
            };
            in_team.push(user);
        }
        memberships += in_team.len();
        // Only the memberships this plugin made change, so people an admin added stay (§9.10).
        backend
            .provide_members(TeamMembersRequest {
                external_id: key(org, &team.slug),
                users: in_team,
            })
            .await?;
        for repository in github
            .team_repositories(settings, token.expose(), org, &team.slug)
            .await
            .map_err(PluginError::from)?
        {
            reach.entry(repository.full_name).or_default().insert(team.slug.clone());
        }
    }
    let repositories =
        github.repositories(settings, token.expose(), org).await.map_err(PluginError::from)?;
    let documents: Vec<Value> = repositories
        .iter()
        .map(|repository| repository_document(repository, reach.get(&repository.full_name)))
        .collect();
    publish(backend, plugin, "synced", documents).await?;

    let listed = github
        .organisation_members(settings, token.expose(), org)
        .await
        .map_err(PluginError::from)?;
    let now = Published {
        version: STATE_VERSION,
        teams: teams.iter().map(|team| key(org, &team.slug)).collect(),
        repositories: repositories.iter().map(|repository| repository.full_name.clone()).collect(),
        members: listed.iter().map(|member| member.id.to_string()).collect(),
    };
    let before: Published = backend
        .state_get(&state_key(org))
        .await?
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    // Teams the last sync left in Resource Definitions go from it once, since core keeps them now.
    let gone_documents: Vec<Value> = match before.version < STATE_VERSION {
        true => before.teams.iter().map(|name| json!({ "kind": "Team", "name": name })).collect(),
        false => Vec::new(),
    };
    let dropped = match before.version < STATE_VERSION {
        true => Vec::new(),
        false => before.teams.difference(&now.teams).cloned().collect(),
    };
    let mut removed = 0;
    let mut released = 0;
    for team in &dropped {
        let answer = backend.remove_team(TeamRemoveRequest { external_id: team.clone() }).await?;
        removed += usize::from(answer.removed);
        released += usize::from(answer.released);
    }
    let left = leavers(backend, plugin, settings, org, &before, &now).await?;
    let repositories_gone: Vec<Value> = before
        .repositories
        .difference(&now.repositories)
        .map(|name| json!({ "kind": "Repository", "name": name }))
        .collect();
    publish(backend, plugin, "removed", [gone_documents, repositories_gone].concat()).await?;
    backend.state_set(&state_key(org), json!(now)).await?;
    tracing::info!(
        org,
        teams = now.teams.len(),
        memberships,
        repositories = now.repositories.len(),
        removed,
        released,
        left,
        "synced"
    );
    Ok(json!({
        "organisation": org,
        "teams": now.teams.len(),
        "memberships": memberships,
        "repositories": now.repositories.len(),
        "removed": removed,
        "released": released,
        "left": left,
    }))
}

/// Everybody the organisation listed last time and does not list now has left it. Core is told,
/// and the offboarding rules decide what happens to them (T67). Saying so needs the
/// identity-provider capability, which this plugin has only when it also signs people in.
async fn leavers(
    backend: &Backend,
    plugin: &str,
    settings: &Settings,
    org: &str,
    before: &Published,
    now: &Published,
) -> Result<usize, PluginError> {
    let left: Vec<&String> = before.members.difference(&now.members).collect();
    if left.is_empty() {
        return Ok(0);
    }
    if settings.oauth.is_none() {
        tracing::warn!(
            org,
            left = left.len(),
            "somebody is no longer in the organisation, but {plugin} signs nobody in, \
             so it cannot say they are gone"
        );
        return Ok(0);
    }
    let mut said = 0;
    for external_id in left {
        let told = DeprovisionRequest {
            external_id: external_id.clone(),
            reason: Some(format!("no longer in the {org} organisation")),
        };
        match backend.deprovision(told).await {
            Ok(answer) => {
                if let Some(user) = answer.user_id {
                    tracing::info!(org, %user, "somebody has left the organisation");
                    said += 1;
                }
            }
            Err(err) => tracing::warn!(org, %err, "somebody who left could not be reported"),
        }
    }
    Ok(said)
}

pub async fn run(
    backend: &Backend,
    github: &GitHub,
    settings: &Settings,
    tokens: &Tokens,
    plugin: &str,
) -> Result<Value, PluginError> {
    let mut synced = Vec::new();
    for org in &settings.organisations {
        synced.push(organisation(backend, github, settings, tokens, plugin, org).await?);
    }
    Ok(json!({ "organisations": synced }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveRequest {
    pub repository: String,
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
}

/// A short-lived tarball link: for a repository in one of the plugin's organisations, read with its
/// token or App, and for any other, read without one, which works for public repositories.
pub async fn archive_link(
    github: &GitHub,
    settings: &Settings,
    tokens: &Tokens,
    asked: &ArchiveRequest,
) -> Result<Value, (u16, String)> {
    let (org, name) = asked
        .repository
        .split_once('/')
        .filter(|(org, name)| !org.is_empty() && !name.is_empty() && !name.contains('/'))
        .ok_or_else(|| (400, format!("`{}` is not written owner/name", asked.repository)))?;
    let ours = settings.organisations.iter().any(|allowed| allowed.eq_ignore_ascii_case(org));
    let reference = asked
        .reference
        .clone()
        .filter(|reference| !reference.is_empty())
        .unwrap_or_else(|| "HEAD".into());
    let token = match (ours, &settings.access) {
        (true, Some(_)) => {
            Some(tokens.for_org(github, settings, org).await.map_err(|err| (503, err))?)
        }
        _ => None,
    };
    let token = token.as_ref().map(|token| token.expose().as_str());
    let url = github.archive(settings, token, &format!("{org}/{name}"), &reference).await?;
    let expires_at = Utc::now() + Duration::minutes(LINK_LIFETIME);
    let web = settings.web.join(&format!("{org}/{name}/blob/{reference}/")).ok().map(String::from);
    Ok(json!({
        "repository": asked.repository,
        "ref": reference,
        "url": url,
        "web": web,
        "expires_at": expires_at,
    }))
}
