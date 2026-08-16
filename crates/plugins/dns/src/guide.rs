//! How developers point their own machines at DOC's DNS, written from what this server is
//! configured with and kept in the Knowledge Base's `development-environment` space, where
//! developers look, rather than on this plugin's page, which only administrators see. It is
//! written again whenever the plugin loads — so whenever its settings change — and checked every
//! hour, so it always says what is true now; the Knowledge Base keeps it as this plugin's own page,
//! replacing nothing anybody else wrote there.

use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{Backend, Query};
use serde_json::{Value, json};

use crate::settings::Serving;

/// The space developers' machines are set up from.
pub const SPACE: &str = "development-environment";
const SPACE_TITLE: &str = "Development environment";
/// This plugin's page in it.
const PATH: &str = "dns.md";
/// The schedule that checks the page still says what is true.
pub const SCHEDULE: &str = "guide";
/// Who keeps the space when this plugin is the one to make it.
pub const OWNERS: &str = "guide-owners";
/// Where what was last written is remembered: once the Knowledge Base says it is there.
const WRITTEN: &str = "guide-written";
/// Written again at least this often, so a page somebody deleted comes back.
const AT_LEAST: i64 = 24;

/// The address a developer's machine asks: the name servers' own addresses when they are named,
/// else the address the server listens on, else the machine DOC runs on as configured.
fn address(serving: &Serving) -> (String, bool) {
    if let Some(first) = serving.nameserver_addresses.first() {
        return (first.to_string(), true);
    }
    match (serving.listen.ip(), serving.machine) {
        (ip, Some(machine)) if ip.is_unspecified() => (machine.to_string(), true),
        (ip, None) if ip.is_unspecified() => ("127.0.0.1".into(), false),
        (ip, _) => (ip.to_string(), true),
    }
}

fn listed(names: &[String]) -> String {
    names.iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ")
}

/// The page, as Markdown, from the server's settings now.
pub fn markdown(serving: &Serving) -> String {
    let zones = &serving.zones;
    let port = serving.listen.port();
    let (host, named) = address(serving);
    let doh = serving.over_https.then(|| crate::doh::url(serving, false));
    let open = serving.over_https.then(|| crate::doh::public_url(serving)).flatten();
    let example = serving
        .plugin_name("kb")
        .filter(|_| !serving.plugin_addresses.is_empty())
        .or_else(|| zones.first().cloned())
        .unwrap_or_else(|| "example.internal".into());
    let mut page = String::from(
        "---\ntitle: Using DOC's DNS on your machine\n---\n# Using DOC's DNS on your machine\n\n",
    );
    page.push_str(
        "> DOC's DNS plugin writes this page from its settings, and writes it again whenever they \
         change, so change it there rather than here: an edit here is replaced.\n\n",
    );
    if zones.is_empty() {
        page.push_str(
            "DOC answers for no domains yet, so there is nothing to point your machine at. An \
             administrator names the domains DOC owns on the DNS plugin's Settings page, and this \
             page says how to use them once they do.\n",
        );
        return page;
    }
    page.push_str(&format!(
        "DOC is the name server for {}. Point your machine at it **for those domains only**: every \
         other name keeps going to the resolver you use now, so nothing else changes.\n\n",
        listed(zones)
    ));
    page.push_str("## What to point at\n\n| | |\n|---|---|\n");
    page.push_str(&format!("| Domains | {} |\n", listed(zones)));
    match serving.on {
        true if named => page.push_str(&format!(
            "| Name server | `{host}`, port `{port}`, over UDP and TCP |\n"
        )),
        true => page.push_str(&format!(
            "| Name server | The machine DOC runs on, port `{port}`, over UDP and TCP: `{host}` when \
             it is your own `just dev`, which the steps below use. Put that machine's address in \
             their place when DOC runs somewhere else |\n"
        )),
        false => page.push_str(
            "| Name server | Not answering: an administrator turns on **Answer DNS questions** \
             under the DNS plugin's Features |\n",
        ),
    }
    if let Some(doh) = &doh {
        page.push_str(&format!(
            "| DNS over HTTPS | `{doh}`, sending a DOC personal access token as a bearer token |\n"
        ));
    }
    if let Some(open) = &open {
        page.push_str(&format!(
            "| DNS over HTTPS, no sign-in | `{open}`, for DOC's domains only, such as from a browser |\n"
        ));
    }
    if let (Some(domain), false) = (&serving.plugin_domain, serving.plugin_addresses.is_empty()) {
        page.push_str(&format!(
            "| A name for each plugin | `<plugin>.{domain}`, such as `https://{example}/` |\n"
        ));
    }
    page.push('\n');
    if !serving.on {
        page.push_str(
            "Until the name server answers, use DNS over HTTPS, if it is offered above, or ask \
             an administrator to turn it on.\n\n",
        );
    } else {
        let routed = zones.iter().map(|zone| format!("~{zone}")).collect::<Vec<_>>().join(" ");
        page.push_str(&format!(
            "## Linux\n\nWith systemd-resolved (Ubuntu, Fedora and most others), send only DOC's \
             domains to it:\n\n```shell\nsudo mkdir -p /etc/systemd/resolved.conf.d\nsudo tee \
             /etc/systemd/resolved.conf.d/doc.conf <<'EOF'\n[Resolve]\nDNS={host}:{port}\n\
             Domains={routed}\nEOF\nsudo systemctl restart systemd-resolved\n```\n\n\
             `resolvectl status` then lists `{host}:{port}` with {}.\n\n",
            listed(&zones.iter().map(|zone| format!("~{zone}")).collect::<Vec<_>>())
        ));
        page.push_str(
            "## macOS\n\nOne file for each domain, which macOS sends only that domain's \
             names to:\n\n```shell\nsudo mkdir -p /etc/resolver\n",
        );
        for zone in zones {
            page.push_str(&format!(
                "printf 'nameserver {host}\\nport {port}\\n' | sudo tee /etc/resolver/{zone}\n"
            ));
        }
        page.push_str("```\n\n`scutil --dns` then lists them, each as a resolver of its own.\n\n");
        page.push_str("## Windows\n\n");
        match port {
            53 => {
                page.push_str(
                    "In PowerShell as an administrator, a rule for each domain:\n\n```powershell\n",
                );
                for zone in zones {
                    page.push_str(&format!(
                        "Add-DnsClientNrptRule -Namespace \".{zone}\" -NameServers \"{host}\"\n"
                    ));
                }
                page.push_str("```\n\n`Get-DnsClientNrptRule` lists them; `Remove-DnsClientNrptRule` takes one away.\n\n");
            }
            other => page.push_str(&format!(
                "Windows asks a name server on port 53 only, and DOC's listens on port `{other}`. \
                 Ask an administrator to publish port 53 to it{}. Under WSL, follow the Linux \
                 steps inside it.\n\n",
                if doh.is_some() { ", or use DNS over HTTPS, above" } else { "" }
            )),
        }
    }
    if let Some(open) = &open {
        page.push_str(&format!(
            "## In a browser\n\nFirefox: **Settings**, **Privacy & Security**, **DNS over HTTPS**, \
             **Increased protection**, with the custom provider `{open}`. It answers for DOC's \
             domains only, and Firefox falls back to your own resolver for everything else.\n\n"
        ));
    }
    if !serving.on {
        return page;
    }
    page.push_str(&format!("## Check it works\n\n```shell\ndig @{host} -p {port} {example}\n"));
    page.push_str(&format!(
        "```\n\n`status: NOERROR` means DOC answered, with the address in its answer section if \
         the name has one. `NXDOMAIN` for a name under {} means DOC has no record for it: they \
         are kept on the DNS plugin's page, which administrators change. No answer at all means \
         your machine cannot reach DOC's name server.\n",
        listed(zones)
    ));
    page
}

/// Who keeps the space if this plugin makes it: the setting, or the one organisation there is.
async fn owners(backend: &Backend) -> Vec<String> {
    let set = backend.settings().list(OWNERS);
    if !set.is_empty() {
        return set;
    }
    let organisations: Vec<Value> = backend
        .query_all(Query::new("core.organisations").fields(&["name"]))
        .await
        .unwrap_or_default();
    match organisations.as_slice() {
        [only] => only["name"]
            .as_str()
            .map(|name| vec![format!("organisation:{name}")])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Writes the page into the Knowledge Base if it says something new, or has not been written for a
/// day. A Knowledge Base that does not let this plugin publish is asked to, once.
pub async fn publish(backend: &Backend) -> Result<String, String> {
    let serving = Serving::read(&backend.settings());
    let page = markdown(&serving);
    let held = backend.state_get(WRITTEN).await.ok().flatten().unwrap_or_default();
    let at = held["at"].as_str().and_then(|at| at.parse::<DateTime<Utc>>().ok());
    let fresh = at.is_some_and(|at| Utc::now() - at < Duration::hours(AT_LEAST));
    if held["page"] == json!(page) && fresh {
        return Ok("the page already says this".into());
    }
    let body = json!({
        "space": SPACE, "title": SPACE_TITLE, "owners": owners(backend).await,
        "pages": [{ "path": PATH, "content": &page }],
    });
    match backend.discovery("kb", "POST", "pages", None, Some(body)).await {
        Ok((status, _)) if (200..300).contains(&status) => {
            let _ = backend.state_set(WRITTEN, json!({ "page": page, "at": Utc::now() })).await;
            Ok(format!("written to {SPACE}"))
        }
        Ok((403, answer)) => {
            let reason = "DNS writes how developers point their machines at it into the Knowledge \
                          Base's development-environment space, from its own settings, and writes \
                          it again when they change.";
            let asked = backend.request_access("kb", "publisher-plugins", reason).await;
            Err(match asked {
                Ok(answer) if answer.state == "pending" => {
                    "the Knowledge Base does not let DNS publish yet: whoever administers it has \
                     been asked to approve"
                        .into()
                }
                _ => format!(
                    "the Knowledge Base refused: {}. Add dns to \"Plugins that publish pages\" on \
                     its Settings page",
                    answer["detail"].as_str().unwrap_or("no reason given")
                ),
            })
        }
        Ok((status, answer)) => Err(format!(
            "the Knowledge Base answered {status}: {}",
            answer["detail"].as_str().unwrap_or("no detail")
        )),
        Err(err) => Err(format!("the Knowledge Base could not be asked: {}", err.detail())),
    }
}
