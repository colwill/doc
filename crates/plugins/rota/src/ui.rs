//! The pages at `/p/rota/...`: everyone's rotas and the caller's own shifts, each rota's shifts to
//! come with its gaps, the forms a lead makes and changes rotas with, the holidays a lead records,
//! and a panel on each team's page in the Catalogue.

use askama::Template;
use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, Request, Response};

use crate::Refusal;
use crate::api::{self, CoverInput, HolidayInput, RotaInput, id_of, parameter};
use crate::people::{Directory, Me};
use crate::plan::{self, cover_for, local};
use crate::store::{CADENCES, GAP, Holiday, Rota, Shift, Store};

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    fn notice(text: impl Into<String>) -> Self {
        Self { notice: Some(text.into()), error: None }
    }

    fn error(text: impl Into<String>) -> Self {
        Self { notice: None, error: Some(text.into()) }
    }
}

fn render<T: Template>(page: &T) -> Result<String, Refusal> {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

type Form = Vec<(String, String)>;

fn form(request: &Request) -> Form {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(form: &Form, name: &str) -> String {
    form.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone()).unwrap_or_default()
}

fn cadence_name(cadence: &str) -> &'static str {
    CADENCES.iter().find(|(name, _)| *name == cadence).map_or("", |(_, shown)| shown)
}

/// One shift as a page shows it.
pub struct ShiftRow {
    pub id: String,
    pub when: String,
    pub until: String,
    pub who: String,
    pub planned: String,
    pub state: String,
    pub badge: &'static str,
    pub now: bool,
    pub wants_cover: bool,
    /// Whom the lead could pick, when they may.
    pub choices: Vec<String>,
    pub rota: String,
    pub rota_title: String,
    pub team: String,
}

fn shift_row(rota: &Rota, shift: &Shift, now: DateTime<Utc>) -> ShiftRow {
    let (state, badge) = match (shift.state.as_str(), shift.clash) {
        (GAP, _) => {
            (format!("Gap: {} is away", shift.planned.as_deref().unwrap_or("they")), "down")
        }
        (_, true) => ("Cover, away for some of it".to_string(), "degraded"),
        ("cover", _) => ("Cover".to_string(), "loading"),
        _ => ("From the rotation".to_string(), "up"),
    };
    ShiftRow {
        id: shift.id.to_string(),
        when: local(rota, shift.starts_at),
        until: local(rota, shift.ends_at),
        who: shift.assignee.clone().unwrap_or_else(|| "Nobody".into()),
        planned: shift.planned.clone().unwrap_or_default(),
        state,
        badge,
        now: shift.starts_at <= now && shift.ends_at > now,
        wants_cover: shift.wants_cover(),
        choices: Vec::new(),
        rota: rota.id.to_string(),
        rota_title: rota.title.clone(),
        team: rota.team.clone(),
    }
}

/// A rota in a list, with who is on now and how many gaps are coming.
pub struct RotaRow {
    pub id: String,
    pub title: String,
    pub team: String,
    pub team_title: String,
    pub cadence: &'static str,
    pub on_now: String,
    pub gaps: usize,
}

async fn rota_row(
    store: &Store<'_>,
    directory: &Directory,
    rota: &Rota,
    now: DateTime<Utc>,
) -> Result<RotaRow, Refusal> {
    let shifts = store.shifts_from(rota.id, now).await?;
    let on_now = shifts
        .iter()
        .find(|shift| shift.starts_at <= now)
        .map(|shift| shift.assignee.clone().unwrap_or_else(|| "Nobody (a gap)".into()))
        .unwrap_or_else(|| "Not started".into());
    Ok(RotaRow {
        id: rota.id.to_string(),
        title: rota.title.clone(),
        team: rota.team.clone(),
        team_title: directory
            .team(rota.team_id)
            .map_or_else(|| rota.team.clone(), |team| team.title.clone()),
        cadence: cadence_name(&rota.cadence),
        on_now,
        gaps: shifts.iter().filter(|shift| shift.wants_cover()).count(),
    })
}

pub struct TeamChoice {
    pub name: String,
    pub title: String,
}

#[derive(Template)]
#[template(path = "home.html")]
struct Home {
    flash: Flash,
    mine: Vec<ShiftRow>,
    led: Vec<TeamChoice>,
    rotas: Vec<RotaRow>,
}

async fn home(backend: &Backend, flash: Flash) -> Result<String, Refusal> {
    let me = Me::of(backend)?;
    let directory = Directory::read(backend).await?;
    let store = Store(backend);
    let now = Utc::now();
    let rotas = store.rotas().await?;
    let mut rows = Vec::new();
    for rota in &rotas {
        rows.push(rota_row(&store, &directory, rota, now).await?);
    }
    rows.sort_by(|a, b| {
        (a.team_title.to_lowercase(), a.title.to_lowercase())
            .cmp(&(b.team_title.to_lowercase(), b.title.to_lowercase()))
    });
    let mut mine: Vec<ShiftRow> = Vec::new();
    for shift in store.shifts_of(&me.login, now).await?.iter().take(10) {
        if let Some(rota) = rotas.iter().find(|rota| rota.id == shift.rota) {
            mine.push(shift_row(rota, shift, now));
        }
    }
    let led = led(&me, &directory);
    render(&Home { flash, mine, led, rotas: rows })
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct OnCall {
    mine: Vec<ShiftRow>,
    rotas: Vec<RotaRow>,
}

/// On call, on a person's dashboard: their next shifts, and who is on now on their teams' rotas.
async fn on_call(backend: &Backend) -> Result<String, Refusal> {
    let me = Me::of(backend)?;
    let directory = Directory::read(backend).await?;
    let store = Store(backend);
    let now = Utc::now();
    let rotas = store.rotas().await?;
    let mut mine = Vec::new();
    for shift in store.shifts_of(&me.login, now).await?.iter().take(3) {
        if let Some(rota) = rotas.iter().find(|rota| rota.id == shift.rota) {
            mine.push(shift_row(rota, shift, now));
        }
    }
    let teams: Vec<uuid::Uuid> = me
        .id
        .map(|id| directory.teams_of(id).iter().map(|team| team.id).collect())
        .unwrap_or_default();
    let mut theirs = Vec::new();
    for rota in rotas.iter().filter(|rota| teams.contains(&rota.team_id)) {
        theirs.push(rota_row(&store, &directory, rota, now).await?);
    }
    render(&OnCall { mine, rotas: theirs })
}

/// The teams the caller makes rotas for, in title order: every team for an administrator.
fn led(me: &Me, directory: &Directory) -> Vec<TeamChoice> {
    let led = match (me.admin, me.id) {
        (true, _) => directory.teams.iter().collect::<Vec<_>>(),
        (false, Some(id)) => directory.led_by(id),
        (false, None) => Vec::new(),
    };
    let mut led: Vec<TeamChoice> = led
        .into_iter()
        .map(|team| TeamChoice { name: team.name.clone(), title: team.title.clone() })
        .collect();
    led.sort_by_key(|team| team.title.to_lowercase());
    led
}

#[derive(Template)]
#[template(path = "rota.html")]
struct RotaPage {
    flash: Flash,
    rota: Rota,
    team_title: String,
    cadence: &'static str,
    arranges: bool,
    shifts: Vec<ShiftRow>,
    members: Vec<String>,
    lead: Option<String>,
}

async fn rota_page(backend: &Backend, id: &str, flash: Flash) -> Result<String, Refusal> {
    let me = Me::of(backend)?;
    let directory = Directory::read(backend).await?;
    let store = Store(backend);
    let rota = api::found(&store, id_of(id, "rota")?).await?;
    let now = Utc::now();
    let arranges = me.arranges(&directory, rota.team_id);
    let recorded = store.holidays_from(now.date_naive()).await?;
    // The calendar too, so the cover offered here is only people who will actually be there.
    let logins = plan::people_of(std::slice::from_ref(&rota), &directory);
    let holidays = crate::away::everything(backend, recorded, &logins, now.date_naive()).await;
    let shifts = store
        .shifts_from(rota.id, now)
        .await?
        .iter()
        .map(|shift| {
            let mut row = shift_row(&rota, shift, now);
            if arranges {
                row.choices = cover_for(&rota, shift, &directory, &holidays);
            }
            row
        })
        .collect();
    let members = plan::serving(&rota, &directory);
    let page = RotaPage {
        flash,
        team_title: directory
            .team(rota.team_id)
            .map_or_else(|| rota.team.clone(), |team| team.title.clone()),
        cadence: cadence_name(&rota.cadence),
        arranges,
        shifts,
        lead: directory.lead_for(rota.team_id).map(|lead| lead.label()),
        members,
        rota,
    };
    render(&page)
}

pub struct MemberChoice {
    pub login: String,
    pub label: String,
    pub position: String,
}

#[derive(Template)]
#[template(path = "rota_form.html")]
struct RotaForm {
    flash: Flash,
    /// The rota being changed, or `None` for a new one.
    id: Option<String>,
    team: String,
    team_title: String,
    title: String,
    description: String,
    cadence: String,
    starts_on: String,
    handover: String,
    timezone: String,
    members: String,
    calendar: bool,
    people: Vec<MemberChoice>,
    cadences: Vec<(&'static str, &'static str)>,
    /// The other teams a new rota could be for; choosing one draws the form again for it.
    teams: Vec<TeamChoice>,
}

impl RotaForm {
    fn blank(team: &str, team_title: &str, people: Vec<MemberChoice>) -> Self {
        Self {
            flash: Flash::default(),
            id: None,
            team: team.to_string(),
            team_title: team_title.to_string(),
            title: String::new(),
            description: String::new(),
            cadence: "weekly".into(),
            starts_on: Utc::now().date_naive().to_string(),
            handover: "09:00".into(),
            timezone: "Europe/London".into(),
            members: people
                .iter()
                .map(|person| person.login.clone())
                .collect::<Vec<_>>()
                .join("\n"),
            calendar: true,
            people,
            cadences: CADENCES.to_vec(),
            teams: Vec::new(),
        }
    }

    fn from_rota(rota: &Rota, team_title: &str, people: Vec<MemberChoice>) -> Self {
        Self {
            flash: Flash::default(),
            id: Some(rota.id.to_string()),
            team: rota.team.clone(),
            team_title: team_title.to_string(),
            title: rota.title.clone(),
            description: rota.description.clone(),
            cadence: rota.cadence.clone(),
            starts_on: rota.starts_on.to_string(),
            handover: rota.handover.clone(),
            timezone: rota.timezone.clone(),
            members: rota.members.join("\n"),
            calendar: rota.calendar,
            people,
            cadences: CADENCES.to_vec(),
            teams: Vec::new(),
        }
    }

    /// What was typed, shown again with why it was refused.
    fn refilled(mut self, form: &Form, error: String) -> Self {
        self.title = field(form, "title");
        self.description = field(form, "description");
        self.cadence = field(form, "cadence");
        self.starts_on = field(form, "starts_on");
        self.handover = field(form, "handover");
        self.timezone = field(form, "timezone");
        self.members = field(form, "members");
        self.calendar = !field(form, "calendar").is_empty();
        self.flash = Flash::error(error);
        self
    }
}

fn people_of(directory: &Directory, team: uuid::Uuid) -> Vec<MemberChoice> {
    directory
        .members(team)
        .into_iter()
        .map(|(person, position)| MemberChoice {
            login: person.login.clone(),
            label: person.label(),
            position: position.unwrap_or_default(),
        })
        .collect()
}

fn input_of(form: &Form, team: &str) -> RotaInput {
    RotaInput {
        team: team.to_string(),
        title: field(form, "title"),
        description: field(form, "description"),
        cadence: field(form, "cadence"),
        starts_on: field(form, "starts_on"),
        handover: field(form, "handover"),
        timezone: field(form, "timezone"),
        members: field(form, "members").lines().map(str::to_string).collect(),
        calendar: Some(!field(form, "calendar").is_empty()),
    }
}

async fn new_form(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let me = Me::of(backend)?;
    let directory = Directory::read(backend).await?;
    let teams = led(&me, &directory);
    // Without a team, the first the caller leads.
    let asked = parameter(&request.query, "team")
        .or_else(|| teams.first().map(|team| team.name.clone()))
        .ok_or_else(|| Refusal::forbidden("only a team's lead makes its rotas"))?;
    let team = directory
        .team_named(asked.trim_start_matches("team:"))
        .ok_or_else(|| Refusal::bad("choose a team to make a rota for"))?;
    if !me.arranges(&directory, team.id) {
        return Err(Refusal::forbidden(
            "only the team's lead, or a lead above it, makes its rotas",
        ));
    }
    let mut page = RotaForm::blank(&team.name, &team.title, people_of(&directory, team.id));
    if teams.len() > 1 {
        page.teams = teams;
    }
    render(&page)
}

async fn edit_form(backend: &Backend, id: &str) -> Result<String, Refusal> {
    let me = Me::of(backend)?;
    let directory = Directory::read(backend).await?;
    let rota = api::found(&Store(backend), id_of(id, "rota")?).await?;
    if !me.arranges(&directory, rota.team_id) {
        return Err(Refusal::forbidden(
            "only the team's lead, or a lead above it, changes its rotas",
        ));
    }
    let title =
        directory.team(rota.team_id).map_or_else(|| rota.team.clone(), |team| team.title.clone());
    render(&RotaForm::from_rota(&rota, &title, people_of(&directory, rota.team_id)))
}

/// Makes or changes a rota from its form; the form again, with why, if that is refused.
async fn save(
    backend: &Backend,
    request: &Request,
    id: Option<&str>,
    moved: &mut Option<String>,
) -> Result<String, Refusal> {
    let form = form(request);
    let team = field(&form, "team");
    let saved = match id {
        Some(id) => api::update(backend, id_of(id, "rota")?, input_of(&form, &team)).await,
        None => api::create(backend, input_of(&form, &team)).await,
    };
    match saved {
        Ok(rota) => {
            *moved = Some(format!("/p/rota/rotas/{}", rota.id));
            rota_page(
                backend,
                &rota.id.to_string(),
                Flash::notice("Saved. Its shifts are planned, and on the calendars."),
            )
            .await
        }
        Err(refusal) if refusal.status < 500 => {
            let directory = Directory::read(backend).await?;
            let Some(found) = directory.team_named(&team) else { return Err(refusal) };
            let page = match id {
                Some(id) => {
                    let rota = api::found(&Store(backend), id_of(id, "rota")?).await?;
                    RotaForm::from_rota(&rota, &found.title, people_of(&directory, found.id))
                }
                None => RotaForm::blank(&found.name, &found.title, people_of(&directory, found.id)),
            };
            render(&page.refilled(&form, refusal.detail))
        }
        Err(refusal) => Err(refusal),
    }
}

#[derive(Template)]
#[template(path = "holidays.html")]
struct Holidays {
    flash: Flash,
    holidays: Vec<HolidayRow>,
    /// Whether the caller records anybody's holidays: they lead somebody.
    records: bool,
}

/// Recording a holiday, on a page of its own, with what was typed when it was refused.
#[derive(Template)]
#[template(path = "holiday_form.html")]
struct HolidayForm {
    flash: Flash,
    /// The people the caller records holidays for.
    people: Vec<MemberChoice>,
    user: String,
    starts_on: String,
    ends_on: String,
    note: String,
}

/// The people the caller records holidays for, by login.
fn recorded_for(me: &Me, directory: &Directory) -> Vec<MemberChoice> {
    let mut people: Vec<MemberChoice> = directory
        .people
        .values()
        .filter(|person| !person.disabled && me.records_for(directory, person.id))
        .map(|person| MemberChoice {
            login: person.login.clone(),
            label: person.label(),
            position: String::new(),
        })
        .collect();
    people.sort_by_key(|person| person.login.to_lowercase());
    people
}

async fn holiday_form(backend: &Backend, form: &Form, flash: Flash) -> Result<String, Refusal> {
    let me = Me::of(backend)?;
    let directory = Directory::read(backend).await?;
    let people = recorded_for(&me, &directory);
    if people.is_empty() {
        return Err(Refusal::forbidden("a lead records the holidays of the people they lead"));
    }
    let today = Utc::now().date_naive().to_string();
    let or_today = |name: &str| Some(field(form, name)).filter(|day| !day.is_empty());
    render(&HolidayForm {
        flash,
        people,
        user: field(form, "user"),
        starts_on: or_today("starts_on").unwrap_or_else(|| today.clone()),
        ends_on: or_today("ends_on").unwrap_or(today),
        note: field(form, "note"),
    })
}

pub struct HolidayRow {
    pub id: String,
    pub user: String,
    pub from: String,
    pub to: String,
    pub note: String,
    pub removable: bool,
    /// True for one read from somebody's own calendar, which is cancelled there and not here.
    pub from_calendar: bool,
}

async fn holidays(backend: &Backend, flash: Flash) -> Result<String, Refusal> {
    let me = Me::of(backend)?;
    let directory = Directory::read(backend).await?;
    let shown = |day: chrono::NaiveDate| day.format("%a %-d %b %Y").to_string();
    let rows = api::visible_holidays(backend, &me, &directory)
        .await?
        .into_iter()
        .map(|holiday: Holiday| HolidayRow {
            id: holiday.id.to_string(),
            removable: holiday.source != crate::away::CALENDAR
                && me.records_for(&directory, holiday.user_id),
            from_calendar: holiday.source == crate::away::CALENDAR,
            from: shown(holiday.starts_on),
            to: shown(holiday.ends_on),
            user: holiday.user,
            note: holiday.note,
        })
        .collect();
    let records = !recorded_for(&me, &directory).is_empty();
    render(&Holidays { flash, holidays: rows, records })
}

#[derive(Template)]
#[template(path = "panel.html")]
struct Panel {
    team: String,
    rotas: Vec<RotaRow>,
    arranges: bool,
}

async fn panel(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let resource = parameter(&request.query, "resource").unwrap_or_default();
    let name = resource
        .split_once(':')
        .filter(|(kind, _)| kind.eq_ignore_ascii_case("team"))
        .map(|(_, name)| name.to_string())
        .ok_or_else(|| Refusal::bad("the panel is for a team: resource=team:<name>"))?;
    let me = Me::of(backend)?;
    let directory = Directory::read(backend).await?;
    let Some(team) = directory.team_named(&name) else {
        return render(&Panel { team: name, rotas: Vec::new(), arranges: false });
    };
    let store = Store(backend);
    let now = Utc::now();
    let mut rotas = Vec::new();
    for rota in store.rotas_of(team.id).await? {
        rotas.push(rota_row(&store, &directory, &rota, now).await?);
    }
    render(&Panel { team: team.name.clone(), rotas, arranges: me.arranges(&directory, team.id) })
}

async fn route(
    backend: &Backend,
    request: &Request,
    path: &[&str],
    moved: &mut Option<String>,
) -> Result<String, Refusal> {
    match (request.method.as_str(), path) {
        ("GET", [] | [""]) => home(backend, Flash::default()).await,
        ("GET", ["panel"]) => panel(backend, request).await,
        ("GET", ["dashboard"]) => on_call(backend).await,
        ("GET", ["rotas", "new"]) => new_form(backend, request).await,
        ("POST", ["rotas"]) => save(backend, request, None, moved).await,
        ("GET", ["rotas", id]) => rota_page(backend, id, Flash::default()).await,
        ("GET", ["rotas", id, "edit"]) => edit_form(backend, id).await,
        ("POST", ["rotas", id]) => save(backend, request, Some(id), moved).await,
        ("POST", ["rotas", id, "delete"]) => {
            let rota = api::delete(backend, id_of(id, "rota")?).await?;
            *moved = Some("/p/rota/".into());
            home(
                backend,
                Flash::notice(format!("{} is deleted, and off the calendars.", rota.title)),
            )
            .await
        }
        ("POST", ["shifts", id, "cover"]) => {
            let form = form(request);
            let user = Some(field(&form, "user")).filter(|user| !user.trim().is_empty());
            let shift = id_of(id, "shift")?;
            let rota = match Store(backend).shift(shift).await? {
                Some(found) => found.rota.to_string(),
                None => return Err(Refusal::missing("there is no such shift")),
            };
            match api::cover(backend, shift, CoverInput { user }).await {
                Ok((_, shift)) => {
                    let said = match &shift.assignee {
                        Some(login) => {
                            format!("{login} is on it now, and it is on their calendar.")
                        }
                        None => {
                            "It is back with the rotation, which has nobody free for it.".into()
                        }
                    };
                    rota_page(backend, &rota, Flash::notice(said)).await
                }
                Err(refusal) if refusal.status < 500 => {
                    rota_page(backend, &rota, Flash::error(refusal.detail)).await
                }
                Err(refusal) => Err(refusal),
            }
        }
        ("GET", ["holidays"]) => holidays(backend, Flash::default()).await,
        ("GET", ["holidays", "new"]) => holiday_form(backend, &Form::new(), Flash::default()).await,
        ("POST", ["holidays"]) => {
            let form = form(request);
            let input = HolidayInput {
                user: field(&form, "user"),
                starts_on: field(&form, "starts_on"),
                ends_on: field(&form, "ends_on"),
                note: field(&form, "note"),
            };
            match api::add_holiday(backend, input).await {
                Ok(holiday) => {
                    let said = format!(
                        "{} is away then. Any shift of theirs it covers is a gap for their lead to fill.",
                        holiday.user
                    );
                    *moved = Some("/p/rota/holidays".into());
                    holidays(backend, Flash::notice(said)).await
                }
                Err(refusal) if refusal.status < 500 => {
                    holiday_form(backend, &form, Flash::error(refusal.detail)).await
                }
                Err(refusal) => Err(refusal),
            }
        }
        ("POST", ["holidays", id, "delete"]) => {
            let holiday = api::delete_holiday(backend, id_of(id, "holiday")?).await?;
            holidays(
                backend,
                Flash::notice(format!(
                    "{}'s holiday is gone, and their shifts are theirs again.",
                    holiday.user
                )),
            )
            .await
        }
        _ => Err(Refusal::missing("there is no such page")),
    }
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let fragment = matches!(path, ["panel"] | ["dashboard"]);
    let mut moved = None;
    match route(backend, request, path, &mut moved).await {
        Ok(html) => match moved {
            Some(url) => Response::html(html).with_header("hx-push-url", &url),
            None => Response::html(html),
        },
        Err(refusal) if fragment => {
            let text = format!(
                "<p class=\"doc-error-message\" role=\"alert\">{}</p>",
                escape(&refusal.detail)
            );
            Response::new(refusal.status, "text/html; charset=utf-8", text)
        }
        Err(refusal) => {
            let page = Blank { flash: Flash::error(refusal.detail.clone()) };
            let html = page.render().unwrap_or_else(|_| escape(&refusal.detail));
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}
