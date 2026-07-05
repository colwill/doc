//! The checks an assignment passes before it is stored: a well-formed permission for a registered
//! plugin, of the holder's member kind, and never a group inside a group.

use doc_permissions::{Group, MemberKind, Permission, Subject};

use crate::model::{Holder, Refusal, parse};
use crate::store::{Known, Store};

fn noun(holder: &Holder) -> &'static str {
    match holder {
        Holder::User { .. } => "user",
        Holder::Service { .. } => "service account",
        Holder::Team { .. } => "team",
        Holder::Group { .. } => "group",
    }
}

fn members(kind: MemberKind) -> &'static str {
    match kind {
        MemberKind::User => "users",
        MemberKind::Service => "service accounts",
    }
}

/// The holder's name; a grant to someone who does not exist would sit there unseen.
pub async fn holder(store: &Store<'_>, holder: &Holder) -> Result<String, Refusal> {
    store
        .label(holder)
        .await?
        .ok_or_else(|| Refusal::missing(format!("no {} {}", noun(holder), holder.key())))
}

pub async fn permission(
    store: &Store<'_>,
    known: &Known,
    holder: &Holder,
    text: &str,
) -> Result<Permission, Refusal> {
    let permission = parse(text)?;
    known.check(&permission)?;
    if let Holder::Group { plugin, name } = holder {
        let record = store
            .group(plugin, name)
            .await?
            .ok_or_else(|| Refusal::missing(format!("no group {plugin}/{name}")))?;
        Group::new(plugin, name, record.kind)?.insert(permission.clone())?;
        return Ok(permission);
    }
    let kind = holder.member_kind();
    match &permission.subject {
        Subject::Group { name } => {
            let plugin = &permission.plugin;
            let record = store
                .group(plugin, name)
                .await?
                .ok_or_else(|| Refusal::bad(format!("{plugin} has no group called {name}")))?;
            if Some(record.kind) != kind {
                return Err(Refusal::bad(format!(
                    "{plugin}/{name} is a group of {}, so a {} cannot join it",
                    members(record.kind),
                    noun(holder)
                )));
            }
        }
        subject if subject.member_kind() != kind => {
            return Err(Refusal::bad(format!("a {} cannot hold {permission}", noun(holder))));
        }
        _ => {}
    }
    Ok(permission)
}
