//! What the pages and the API share: a dashboard read and parsed, one of its panels with any
//! library panel put in place, and a panel's queries run. The data sources are listed only when
//! a panel cannot be resolved without them.

use doc_plugin_sdk::Backend;

use crate::Refusal;
use crate::dashboard::{Built, Dashboard, Panel, Scope};
use crate::frames::Answer;
use crate::grafana::Grafana;
use crate::settings::Config;

/// The account Grafana is reached through, or why there is none.
pub fn connected(config: &Config) -> Result<&str, Refusal> {
    config.account.as_deref().ok_or_else(|| Refusal {
        status: 503,
        detail: "Grafana is not connected yet: the vendor account it is reached through is chosen \
                 on this plugin's Settings page."
            .into(),
    })
}

/// A dashboard's uid, refused unless it is letters, digits, `-` and `_`, as Grafana's are.
pub fn uid(text: &str) -> Result<&str, Refusal> {
    let fine = !text.is_empty()
        && text.len() <= 64
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    match fine {
        true => Ok(text),
        false => Err(Refusal::bad(format!("{text} is not a dashboard's uid"))),
    }
}

pub async fn board(grafana: &Grafana<'_>, uid: &str) -> Result<Dashboard, Refusal> {
    let answer = grafana.dashboard(uid).await?;
    Dashboard::read(uid, &answer).map_err(Refusal::unavailable)
}

/// One panel of a dashboard, by its id, with its library panel's model in place of a reference.
pub async fn panel(grafana: &Grafana<'_>, board: &Dashboard, id: &str) -> Result<Panel, Refusal> {
    let id: i64 = id.parse().map_err(|_| Refusal::bad(format!("{id} is not a panel's id")))?;
    let panel = board
        .panel(id)
        .ok_or_else(|| Refusal::missing(format!("{} has no panel {id}", board.title)))?;
    match &panel.library {
        Some(library) => Ok(panel.with_library(&grafana.library_panel(library).await?)),
        None => Ok(panel.clone()),
    }
}

/// Runs a panel's queries. A refusal here is a sentence for the panel's card, not the page's.
pub async fn answer(
    grafana: &Grafana<'_>,
    scope: &Scope<'_>,
    panel: &Panel,
) -> Result<Answer, String> {
    let built = match scope.build(panel, None) {
        // An account that may not list the data sources leaves names to be taken on trust.
        Built::Unknown => {
            let list = grafana.datasources().await.unwrap_or_default();
            scope.build(panel, Some(&list))
        }
        built => built,
    };
    let asking = match built {
        Built::Ready(asking) => asking,
        Built::Refused(why) => return Err(format!("DOC cannot run this panel's queries: {why}.")),
        Built::Unknown => {
            return Err("DOC could not work out which data source this panel reads.".into());
        }
    };
    let answered = grafana.query(&asking.body).await.map_err(|refusal| refusal.detail)?;
    Ok(Answer::read(&answered, &asking.hidden))
}

/// Who is looking may change the plugin's settings, so a page can send them there.
pub fn settles(backend: &Backend) -> bool {
    backend.caller().is_some_and(|caller| caller.admin)
}
