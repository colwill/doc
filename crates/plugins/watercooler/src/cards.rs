//! Cards and kudos. A card is for one person: others sign it until it is revealed, it stays hidden
//! from that person until then, and its delivery is announced. Kudos go to a person or a team with
//! a message, and are announced as they are given.

use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use doc_plugin_sdk::{Backend, Caller};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::api::{self, Sight};
use crate::markdown;
use crate::store::{Card, Kudos, Store};

pub const CARD_KINDS: [(&str, &str); 5] = [
    ("farewell", "Farewell"),
    ("congratulations", "Congratulations"),
    ("birthday", "Birthday"),
    ("welcome", "Welcome"),
    ("thank-you", "Thank you"),
];

const MAX_MESSAGE: usize = 2_000;

pub fn card_kind(kind: &str) -> &'static str {
    CARD_KINDS.iter().find(|(name, _)| *name == kind).map_or("Card", |(_, shown)| shown)
}

pub fn login(backend: &Backend) -> Result<String, Refusal> {
    match backend.caller() {
        Some(Caller { kind, id: Some(id), label, .. }) if kind == "user" || kind == "service" => {
            Ok(label.clone().unwrap_or_else(|| id.clone()))
        }
        _ => Err(Refusal::forbidden("cards and kudos are for people and service accounts")),
    }
}

fn instant(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text).ok().map(|at| at.with_timezone(&Utc))
}

pub fn revealed(card: &Card) -> bool {
    instant(&card.reveal_at).is_some_and(|at| at <= Utc::now())
}

/// Whether `me` is the card's recipient and must not know of it yet.
pub fn hidden_from(card: &Card, me: &str) -> bool {
    card.recipient == me && !revealed(card)
}

pub fn card_page(card: &Card) -> String {
    api::page(&format!("cards/{}", card.id))
}

/// A person, named by login or as provider/login, answered by login.
async fn user_of(sight: &mut Sight<'_>, name: &str) -> Result<String, Refusal> {
    let name = name.trim().trim_start_matches('@').trim_start_matches("user:");
    let login = name.rsplit('/').next().unwrap_or(name).to_string();
    let fine = !login.is_empty() && !login.chars().any(char::is_whitespace);
    match sight.resource(&format!("user:{name}")).await {
        Ok(_) | Err(409) if fine => Ok(login),
        _ => Err(Refusal::bad(format!("there is no one called {name}"))),
    }
}

async fn team_of(sight: &mut Sight<'_>, name: &str) -> Result<String, Refusal> {
    let team = name.trim().trim_start_matches("team:").to_string();
    match sight.resource(&format!("team:{team}")).await {
        Ok(_) if !team.is_empty() => Ok(team),
        _ => Err(Refusal::bad(format!("there is no team called {team}, or you cannot see it"))),
    }
}

fn message(text: &str) -> Result<(String, String), Refusal> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > MAX_MESSAGE {
        return Err(Refusal::bad(format!("a message is 1 to {MAX_MESSAGE} characters")));
    }
    Ok((text.to_string(), markdown::render(text).html))
}

async fn announce(backend: &Backend, topic: &str, payload: Value, key: &str) -> bool {
    match backend.publish_once(topic, payload, key).await {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(%err, %topic, "a card or kudos was not announced");
            false
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewCard {
    pub kind: String,
    pub title: String,
    pub recipient: String,
    #[serde(default)]
    pub team: Option<String>,
    /// The date it is revealed on, and the time, 09:00 unless given.
    pub reveal_on: String,
    #[serde(default)]
    pub reveal_time: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

pub async fn found_card(store: &Store<'_>, me: &str, id: &str) -> Result<Card, Refusal> {
    let id: Uuid = id.parse().map_err(|_| Refusal::missing("there is no such card"))?;
    match store.card(id).await? {
        Some(card) if !hidden_from(&card, me) => Ok(card),
        _ => Err(Refusal::missing("there is no such card")),
    }
}

/// Makes a card, signed first by whoever makes it if they write something.
pub async fn make_card(
    backend: &Backend,
    sight: &mut Sight<'_>,
    asked: NewCard,
) -> Result<Card, Refusal> {
    let me = login(backend)?;
    let kind = CARD_KINDS
        .iter()
        .map(|(name, _)| *name)
        .find(|name| *name == asked.kind.trim())
        .ok_or_else(|| {
            Refusal::bad("a card is a farewell, congratulations, birthday, welcome or thank-you")
        })?;
    let title = asked.title.trim().to_string();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(Refusal::bad("a card's title is 1 to 200 characters"));
    }
    let recipient = user_of(sight, &asked.recipient).await?;
    if recipient == me {
        return Err(Refusal::bad("a card is for someone else"));
    }
    let team = match asked.team.as_deref().map(str::trim).filter(|team| !team.is_empty()) {
        Some(team) => Some(team_of(sight, team).await?),
        None => None,
    };
    let zone =
        asked.timezone.as_deref().map(str::trim).filter(|zone| !zone.is_empty()).unwrap_or("UTC");
    let tz: Tz = zone
        .parse()
        .map_err(|_| Refusal::bad(format!("`{zone}` is not a time zone, such as Europe/London")))?;
    let day = NaiveDate::parse_from_str(asked.reveal_on.trim(), "%Y-%m-%d")
        .map_err(|_| Refusal::bad("say the date it is revealed, such as 2026-10-30"))?;
    let time = match asked.reveal_time.as_deref().map(str::trim).filter(|time| !time.is_empty()) {
        Some(time) => NaiveTime::parse_from_str(time, "%H:%M")
            .map_err(|_| Refusal::bad(format!("`{time}` is not a time of day, such as 09:00")))?,
        None => NaiveTime::from_hms_opt(9, 0, 0).unwrap_or_default(),
    };
    let reveal_at = tz
        .from_local_datetime(&day.and_time(time))
        .earliest()
        .ok_or_else(|| Refusal::bad("that time does not happen there, because of daylight saving"))?
        .with_timezone(&Utc);
    if reveal_at <= Utc::now() {
        return Err(Refusal::bad("a card is revealed later than now, so others can sign it"));
    }
    let first =
        asked.message.as_deref().filter(|text| !text.trim().is_empty()).map(message).transpose()?;
    let card = Card {
        id: Uuid::now_v7(),
        kind: kind.to_string(),
        title,
        recipient,
        team,
        creator: me.clone(),
        reveal_at: reveal_at.to_rfc3339(),
        timezone: zone.to_string(),
        delivered_at: None,
        created_at: String::new(),
        signatures: 0,
    };
    let store = Store(backend);
    store.make_card(&card).await?;
    if let Some((text, html)) = first {
        store.sign(card.id, &me, &text, &html).await?;
    }
    store.card(card.id).await?.ok_or_else(|| Refusal::missing("the card has gone"))
}

/// Signs a card, or changes what the caller wrote on it, until it is revealed.
pub async fn sign(backend: &Backend, id: &str, text: &str) -> Result<Card, Refusal> {
    let me = login(backend)?;
    let store = Store(backend);
    let card = found_card(&store, &me, id).await?;
    if card.recipient == me {
        return Err(Refusal::bad("this card is for you, so you read it rather than sign it"));
    }
    let (text, html) = message(text)?;
    if !store.sign(card.id, &me, &text, &html).await? {
        return Err(Refusal::bad("it has been revealed, so it can no longer be signed"));
    }
    store.card(card.id).await?.ok_or_else(|| Refusal::missing("the card has gone"))
}

pub async fn delete_card(backend: &Backend, id: &str) -> Result<Card, Refusal> {
    let me = login(backend)?;
    let store = Store(backend);
    let card = found_card(&store, &me, id).await?;
    let admin = backend.caller().is_some_and(|caller| caller.admin);
    if !admin && card.creator != me {
        return Err(Refusal::forbidden(format!(
            "only {}, who made it, or an admin deletes it",
            card.creator
        )));
    }
    store.delete_card(card.id).await?;
    Ok(card)
}

/// Announces each card whose time has come, once.
pub async fn deliver(backend: &Backend) -> Result<Value, Refusal> {
    let store = Store(backend);
    let mut delivered = Vec::new();
    for card in store.due_cards().await? {
        let payload = json!({
            "card": card.id,
            "kind": card.kind,
            "team": card.team,
            "title": card.title,
            "recipient": card.recipient,
            "signatures": card.signatures,
            "url": card_page(&card),
        });
        let key = format!("delivered:{}", card.id);
        if announce(backend, "plugin.water.card.delivered", payload, &key).await
            && store.delivered(card.id).await?
        {
            delivered.push(card.id);
        }
    }
    Ok(json!({ "delivered": delivered }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewKudos {
    /// `team:<name>`, or a person's login.
    pub to: String,
    #[serde(default)]
    pub team: Option<String>,
    pub message: String,
}

pub fn kudos_page(kudos: &Kudos) -> String {
    api::page(&format!("kudos/{}/{}#k-{}", kudos.to_kind, kudos.to_name, kudos.id))
}

pub async fn give(
    backend: &Backend,
    sight: &mut Sight<'_>,
    asked: NewKudos,
) -> Result<Kudos, Refusal> {
    let me = login(backend)?;
    let to = asked.to.trim();
    let (to_kind, to_name) = match to.strip_prefix("team:") {
        Some(team) => ("team", team_of(sight, team).await?),
        None => ("user", user_of(sight, to).await?),
    };
    if to_kind == "user" && to_name == me {
        return Err(Refusal::bad("kudos are for someone else"));
    }
    let team = match (to_kind, asked.team.as_deref().map(str::trim).filter(|team| !team.is_empty()))
    {
        ("team", _) => Some(to_name.clone()),
        (_, Some(team)) => Some(team_of(sight, team).await?),
        _ => None,
    };
    let (text, html) = message(&asked.message)?;
    let kudos = Kudos {
        id: Uuid::now_v7(),
        giver: me.clone(),
        to_kind: to_kind.to_string(),
        to_name,
        team,
        message: text,
        html,
        created_at: String::new(),
    };
    Store(backend).give(&kudos).await?;
    let payload = json!({
        "kudos": kudos.id,
        "team": kudos.team,
        "from": kudos.giver,
        "to": kudos.to_name,
        "to_kind": kudos.to_kind,
        "message": kudos.message,
        "url": kudos_page(&kudos),
    });
    announce(backend, "plugin.water.kudos.given", payload, &format!("kudos:{}", kudos.id)).await;
    Ok(kudos)
}

pub fn card_shown(card: &Card) -> Value {
    json!({
        "id": card.id,
        "kind": card.kind,
        "title": card.title,
        "recipient": card.recipient,
        "team": card.team,
        "creator": card.creator,
        "reveal_at": card.reveal_at,
        "timezone": card.timezone,
        "revealed": revealed(card),
        "delivered_at": card.delivered_at,
        "signatures": card.signatures,
        "url": card_page(card),
    })
}

pub fn kudos_shown(kudos: &Kudos) -> Value {
    json!({
        "id": kudos.id,
        "from": kudos.giver,
        "to_kind": kudos.to_kind,
        "to": kudos.to_name,
        "team": kudos.team,
        "message": kudos.message,
        "html": kudos.html,
        "created_at": kudos.created_at,
        "url": kudos_page(kudos),
    })
}
