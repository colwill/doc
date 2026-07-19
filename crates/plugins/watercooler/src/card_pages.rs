//! The pages at `/p/water/cards/...` and `/p/water/kudos/...`: cards to sign and cards for you,
//! making and signing one, the kudos feed with its form, each person's and team's kudos, and a
//! panel of the latest for a team's or person's page.

use std::collections::BTreeMap;

use askama::Template;
use chrono::DateTime;
use chrono_tz::Tz;
use doc_plugin_sdk::{Backend, Request};

use crate::Refusal;
use crate::api::{self, Sight, query};
use crate::cards::{self, NewCard, NewKudos};
use crate::event_pages::{Choice, teams};
use crate::store::{Card, Kudos, Store};
use crate::ui::{Flash, Form, field, form, render};

pub struct CardLine {
    pub href: String,
    pub kind: &'static str,
    pub title: String,
    pub recipient: String,
    pub reveal: String,
    pub signatures: i64,
}

pub struct Signed {
    pub signer: String,
    pub when: String,
    pub html: String,
}

pub struct Given {
    pub id: String,
    pub from: String,
    pub to: String,
    pub href: String,
    pub team: String,
    pub when: String,
    pub html: String,
}

#[derive(Template)]
#[template(path = "cards.html")]
struct CardsPage {
    flash: Flash,
    writes: bool,
    for_me: Vec<CardLine>,
    to_sign: Vec<CardLine>,
    revealed: Vec<CardLine>,
}

#[derive(Template)]
#[template(path = "card_form.html")]
struct CardForm {
    flash: Flash,
    writes: bool,
    kinds: Vec<Choice>,
    teams: Vec<Choice>,
    fields: BTreeMap<String, String>,
}

#[derive(Template)]
#[template(path = "card.html")]
struct CardPage {
    flash: Flash,
    card: Card,
    kind: &'static str,
    reveal: String,
    revealed: bool,
    for_me: bool,
    signed: Vec<Signed>,
    can_sign: bool,
    can_delete: bool,
    mine: String,
}

#[derive(Template)]
#[template(path = "kudos.html")]
struct KudosPage {
    flash: Flash,
    writes: bool,
    heading: String,
    given: Vec<Given>,
    teams: Vec<Choice>,
    tell: Vec<Choice>,
    person: String,
}

#[derive(Template)]
#[template(path = "kudos_panel.html")]
struct KudosPanel {
    writes: bool,
    page: String,
    given: Vec<Given>,
}

/// The kudos a person was given, newest first, on their dashboard.
#[derive(Template)]
#[template(path = "kudos_dashboard.html")]
struct KudosDashboard {
    page: String,
    given: Vec<Given>,
}

/// A moment in `zone`, as a person reads it.
fn shown_in(text: &str, zone: &str) -> String {
    let zone: Tz = zone.parse().unwrap_or(Tz::UTC);
    DateTime::parse_from_rfc3339(text).map_or_else(
        |_| text.to_string(),
        |at| at.with_timezone(&zone).format("%a %-d %b %Y, %H:%M %Z").to_string(),
    )
}

fn line(card: &Card) -> CardLine {
    CardLine {
        href: format!("/p/water/cards/{}", card.id),
        kind: cards::card_kind(&card.kind),
        title: card.title.clone(),
        recipient: card.recipient.clone(),
        reveal: shown_in(&card.reveal_at, &card.timezone),
        signatures: card.signatures,
    }
}

fn given(kudos: &Kudos) -> Given {
    Given {
        id: kudos.id.to_string(),
        from: kudos.giver.clone(),
        to: match kudos.to_kind.as_str() {
            "team" => format!("the {} team", kudos.to_name),
            _ => kudos.to_name.clone(),
        },
        href: format!("/p/water/kudos/{}/{}", kudos.to_kind, kudos.to_name),
        team: kudos
            .team
            .clone()
            .filter(|team| kudos.to_kind != "team" || team != &kudos.to_name)
            .unwrap_or_default(),
        when: shown_in(&kudos.created_at, "UTC"),
        html: kudos.html.clone(),
    }
}

pub async fn cards(
    backend: &Backend,
    request: &Request,
    path: &[&str],
    moved: &mut Option<String>,
) -> Result<String, Refusal> {
    let mut sight = Sight::new(backend);
    match (request.method.as_str(), path) {
        ("GET", [] | [""]) => list(backend, Flash::default()).await,
        ("GET", ["new"]) => {
            let recipient = query(request, "for").unwrap_or_default();
            new_card(backend, vec![("recipient".into(), recipient)], Flash::default()).await
        }
        ("POST", [] | [""]) => {
            let asked = form(request);
            let text = |name: &str| field(&asked, name);
            let wanted = NewCard {
                kind: text("kind").unwrap_or_default(),
                title: text("title").unwrap_or_default(),
                recipient: text("recipient").unwrap_or_default(),
                team: text("team"),
                reveal_on: text("reveal_on").unwrap_or_default(),
                reveal_time: text("reveal_time"),
                timezone: text("timezone"),
                message: text("message"),
            };
            match cards::make_card(backend, &mut sight, wanted).await {
                Ok(card) => {
                    *moved = Some(format!("/p/water/cards/{}", card.id));
                    let notice = format!(
                        "Made. Others can sign it until it is revealed to {}.",
                        card.recipient
                    );
                    page(backend, &card.id.to_string(), Flash::done(notice)).await
                }
                Err(refusal) if refusal.status >= 500 => Err(refusal),
                Err(refusal) => new_card(backend, asked, Flash::refused(refusal.detail)).await,
            }
        }
        ("GET", [id]) => page(backend, id, Flash::default()).await,
        ("POST", [id, "sign"]) => {
            let text = field(&form(request), "message").unwrap_or_default();
            let flash = match cards::sign(backend, id, &text).await {
                Ok(_) => Flash::done("Signed."),
                Err(refusal) if refusal.status == 404 => return Err(refusal),
                Err(refusal) => Flash::refused(refusal.detail),
            };
            page(backend, id, flash).await
        }
        ("POST", [id, "delete"]) => match cards::delete_card(backend, id).await {
            Ok(card) => {
                *moved = Some("/p/water/cards".into());
                list(backend, Flash::done(format!("Deleted {}.", card.title))).await
            }
            Err(refusal) => page(backend, id, Flash::refused(refusal.detail)).await,
        },
        _ => Err(Refusal::missing("no such page")),
    }
}

async fn list(backend: &Backend, flash: Flash) -> Result<String, Refusal> {
    let me = cards::login(backend)?;
    let (mut for_me, mut to_sign, mut revealed) = (Vec::new(), Vec::new(), Vec::new());
    for card in Store(backend).cards().await? {
        match (card.recipient == me, cards::revealed(&card)) {
            (true, true) => for_me.push(line(&card)),
            (true, false) => {}
            (false, false) => to_sign.push(line(&card)),
            (false, true) => revealed.push(line(&card)),
        }
    }
    for_me.reverse();
    revealed.reverse();
    revealed.truncate(20);
    render(&CardsPage { flash, writes: backend.writes(), for_me, to_sign, revealed })
}

async fn new_card(backend: &Backend, chosen: Form, flash: Flash) -> Result<String, Refusal> {
    let mut fields: BTreeMap<String, String> = chosen.into_iter().collect();
    fields.entry("timezone".into()).or_insert_with(|| "UTC".into());
    fields.entry("reveal_time".into()).or_insert_with(|| "09:00".into());
    let pick = |name: &str| fields.get(name).cloned().unwrap_or_default();
    let (kind, team) = (pick("kind"), pick("team"));
    let kinds = cards::CARD_KINDS
        .iter()
        .map(|(value, shown)| Choice {
            value: (*value).to_string(),
            label: (*shown).to_string(),
            selected: *value == kind,
        })
        .collect();
    let mut teams = teams(backend).await;
    for choice in &mut teams {
        choice.selected = choice.value == team;
    }
    render(&CardForm { flash, writes: backend.writes(), kinds, teams, fields })
}

async fn page(backend: &Backend, id: &str, flash: Flash) -> Result<String, Refusal> {
    let me = cards::login(backend)?;
    let store = Store(backend);
    let card = cards::found_card(&store, &me, id).await?;
    let signatures = store.signatures(card.id).await?;
    let mine = signatures
        .iter()
        .find(|signature| signature.signer == me)
        .map(|signature| signature.message.clone())
        .unwrap_or_default();
    let signed = signatures
        .into_iter()
        .map(|signature| Signed {
            when: shown_in(&signature.signed_at, &card.timezone),
            signer: signature.signer,
            html: signature.html,
        })
        .collect();
    let revealed = cards::revealed(&card);
    let admin = backend.caller().is_some_and(|caller| caller.admin);
    render(&CardPage {
        flash,
        kind: cards::card_kind(&card.kind),
        reveal: shown_in(&card.reveal_at, &card.timezone),
        for_me: card.recipient == me,
        can_sign: backend.writes() && !revealed && card.recipient != me,
        can_delete: backend.writes() && (admin || card.creator == me),
        revealed,
        signed,
        mine,
        card,
    })
}

pub async fn kudos(backend: &Backend, request: &Request, path: &[&str]) -> Result<String, Refusal> {
    let mut sight = Sight::new(backend);
    let store = Store(backend);
    match (request.method.as_str(), path) {
        ("GET", [] | [""]) => feed(backend, None, Flash::default()).await,
        ("GET", [kind @ ("user" | "team"), name]) => {
            feed(backend, Some((kind, name)), Flash::default()).await
        }
        ("POST", [] | [""]) => {
            let asked = form(request);
            let to = match (field(&asked, "to_team"), field(&asked, "person")) {
                (Some(team), _) => format!("team:{team}"),
                (None, Some(person)) => person,
                (None, None) => {
                    let refused = Flash::refused("say who the kudos are for");
                    return feed(backend, None, refused).await;
                }
            };
            let wanted = NewKudos {
                to,
                team: field(&asked, "team"),
                message: field(&asked, "message").unwrap_or_default(),
            };
            let back = field(&asked, "back").filter(|back| back.contains('/'));
            let at = back.as_deref().and_then(|back| back.split_once('/'));
            match cards::give(backend, &mut sight, wanted).await {
                Ok(kudos) => {
                    let notice = format!("Kudos to {} given.", given(&kudos).to);
                    feed(backend, at, Flash::done(notice)).await
                }
                Err(refusal) if refusal.status >= 500 => Err(refusal),
                Err(refusal) => feed(backend, at, Flash::refused(refusal.detail)).await,
            }
        }
        ("GET", ["dashboard"]) => {
            let login =
                backend.caller().and_then(|caller| caller.label.clone()).unwrap_or_default();
            if login.is_empty() {
                return Err(Refusal::forbidden("a dashboard is a person's"));
            }
            let listed = store.kudos(Some(("user", &login)), 5).await?;
            render(&KudosDashboard {
                page: format!("/p/water/kudos/user/{login}"),
                given: listed.iter().map(given).collect(),
            })
        }
        ("GET", ["panel"]) => {
            let asked =
                query(request, "resource").ok_or_else(|| Refusal::bad("name the resource"))?;
            let tag = api::tag_in(&asked)?;
            let (kind, name) = tag.split_once(':').unwrap_or_default();
            let name = if kind == "user" { name.rsplit('/').next().unwrap_or(name) } else { name };
            let listed = store.kudos(Some((kind, name)), 5).await?;
            render(&KudosPanel {
                writes: backend.writes(),
                page: format!("/p/water/kudos/{kind}/{name}"),
                given: listed.iter().map(given).collect(),
            })
        }
        _ => Err(Refusal::missing("no such page")),
    }
}

async fn feed(
    backend: &Backend,
    to: Option<(&str, &str)>,
    flash: Flash,
) -> Result<String, Refusal> {
    let listed = Store(backend).kudos(to, 100).await?;
    let heading = match to {
        Some(("team", name)) => format!("Kudos for the {name} team"),
        Some((_, name)) => format!("Kudos for {name}"),
        None => "Kudos".to_string(),
    };
    let mut teams = teams(backend).await;
    let tell = teams
        .iter()
        .map(|choice| Choice {
            value: choice.value.clone(),
            label: choice.label.clone(),
            selected: false,
        })
        .collect();
    for choice in &mut teams {
        choice.selected = to == Some(("team", choice.value.as_str()));
    }
    let person = match to {
        Some(("user", name)) => name.to_string(),
        _ => String::new(),
    };
    render(&KudosPage {
        flash,
        writes: backend.writes(),
        heading,
        given: listed.iter().map(given).collect(),
        teams,
        tell,
        person,
    })
}
