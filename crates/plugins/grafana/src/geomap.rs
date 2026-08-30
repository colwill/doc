//! Geomap panels: each marker or heatmap layer's rows, as Grafana's page shapes them, placed by
//! Grafana's own rules — coordinates, a geohash, or a country looked up — for the platform's map
//! (DOC-SPEC §11.24) to draw. Grafana's basemaps and other layers are its own.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::dashboard::Panel;
use crate::frames::{Answer, Frame, Kind, cell_text, figure};

/// The most places one map draws, so a page stays light; the figures page has them all.
const MOST_PLACES: usize = 2_000;
/// The layers drawn here. Grafana draws the rest itself.
const DRAWN: [&str; 2] = ["markers", "heatmap"];

/// How Grafana's `auto` location finds its columns, by name.
const LATITUDE: [&str; 2] = ["lat", "latitude"];
const LONGITUDE: [&str; 4] = ["lon", "lng", "long", "longitude"];
const GEOHASH: [&str; 1] = ["geohash"];
const LOOKUP: [&str; 1] = ["lookup"];
/// Columns that read best as a place's name, before any other text.
const NAMES: [&str; 6] = ["name", "label", "city", "site", "location", "country"];

/// One row a layer is drawn from, by column name, with the unit each value is in.
#[derive(Debug, Default)]
struct Row {
    values: BTreeMap<String, Value>,
    units: BTreeMap<String, String>,
}

impl Row {
    fn put(&mut self, name: String, value: Value, unit: Option<&String>) {
        if let Some(unit) = unit {
            self.units.insert(name.clone(), unit.clone());
        }
        self.values.insert(name, value);
    }
}

/// A frame's rows as Grafana's geomap reads them: a labelled series is its labels and latest value.
fn rows(frame: &Frame) -> Vec<Row> {
    let labelled: Vec<_> = frame
        .fields
        .iter()
        .filter(|field| field.kind == Kind::Number && !field.labels.is_empty())
        .collect();
    if !labelled.is_empty() {
        let time = frame.fields.iter().find(|field| field.kind == Kind::Time);
        return labelled
            .iter()
            .filter_map(|field| {
                let latest = (0..field.values.len())
                    .rev()
                    .find(|row| field.values[*row].as_f64().is_some_and(f64::is_finite))?;
                let mut row = Row::default();
                for (key, value) in &field.labels {
                    row.put(key.clone(), json!(value), None);
                }
                if let Some(time) = time.and_then(|time| time.values.get(latest)) {
                    row.put("Time".into(), time.clone(), None);
                }
                let value = &field.values[latest];
                let names =
                    ["Value".to_string(), format!("Value #{}", frame.ref_id), field.name.clone()];
                for name in names.into_iter().chain(field.shown.clone()) {
                    row.put(name, value.clone(), field.unit.as_ref());
                }
                Some(row)
            })
            .collect();
    }
    let count = frame.fields.iter().map(|field| field.values.len()).max().unwrap_or_default();
    (0..count)
        .map(|index| {
            let mut row = Row::default();
            for field in &frame.fields {
                let value = field.values.get(index).cloned().unwrap_or(Value::Null);
                for name in std::iter::once(field.name.clone()).chain(field.shown.clone()) {
                    row.put(name, value.clone(), field.unit.as_ref());
                }
            }
            row
        })
        .collect()
}

/// The new names an `organize` transformation gives columns, so a layer finds them by those.
fn renamed(panel: &Panel) -> BTreeMap<String, String> {
    panel
        .transformations
        .as_array()
        .into_iter()
        .flatten()
        .filter(|step| step["id"].as_str() == Some("organize"))
        .flat_map(|step| step["options"]["renameByName"].as_object().cloned().unwrap_or_default())
        .filter_map(|(from, to)| Some((from, to.as_str().filter(|to| !to.is_empty())?.to_string())))
        .collect()
}

fn rename(row: &mut Row, names: &BTreeMap<String, String>) {
    for (from, to) in names {
        if let Some(value) = row.values.remove(from) {
            row.values.insert(to.clone(), value);
        }
        if let Some(unit) = row.units.remove(from) {
            row.units.insert(to.clone(), unit);
        }
    }
}

/// The column named `name`, or else the first of `fallback`, ignoring case.
fn column(columns: &[String], name: Option<&str>, fallback: &[&str]) -> Option<String> {
    let found =
        |wanted: &str| columns.iter().find(|held| held.eq_ignore_ascii_case(wanted)).cloned();
    match name.filter(|name| !name.is_empty()) {
        Some(name) => found(name),
        None => fallback.iter().find_map(|name| found(name)),
    }
}

fn number(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    value.as_f64().or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

/// Where a geohash is: the middle of the cell it names.
fn geohash(hash: &str) -> Option<(f64, f64)> {
    const BASE32: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";
    let hash = hash.trim().to_ascii_lowercase();
    if hash.is_empty() {
        return None;
    }
    let (mut lat, mut lon) = ((-90.0_f64, 90.0_f64), (-180.0_f64, 180.0_f64));
    let mut longitude = true;
    for byte in hash.bytes() {
        let index = BASE32.iter().position(|known| *known == byte)?;
        for bit in (0..5).rev() {
            let range = if longitude { &mut lon } else { &mut lat };
            let middle = (range.0 + range.1) / 2.0;
            match (index >> bit) & 1 {
                1 => range.0 = middle,
                _ => range.1 = middle,
            }
            longitude = !longitude;
        }
    }
    Some(((lat.0 + lat.1) / 2.0, (lon.0 + lon.1) / 2.0))
}

/// How one layer finds where a row is, by the columns that say so.
enum Locate {
    Coordinates(String, String),
    Geohash(String),
    Country(String),
}

impl Locate {
    fn columns(&self) -> Vec<&str> {
        match self {
            Self::Coordinates(latitude, longitude) => vec![latitude, longitude],
            Self::Geohash(column) | Self::Country(column) => vec![column],
        }
    }
}

fn locate(columns: &[String], location: &Value) -> Result<Locate, String> {
    let given = |key: &str| location[key].as_str();
    let coordinates = || {
        let latitude = column(columns, given("latitude"), &LATITUDE)?;
        let longitude = column(columns, given("longitude"), &LONGITUDE)?;
        Some(Locate::Coordinates(latitude, longitude))
    };
    let hashed = || column(columns, given("geohash"), &GEOHASH).map(Locate::Geohash);
    let looked_up = || -> Result<Option<Locate>, String> {
        let gazetteer = given("gazetteer").unwrap_or_default();
        if !gazetteer.is_empty() && !gazetteer.contains("countries") {
            return Err(format!(
                "Its places are looked up in {gazetteer}, and DOC knows only countries."
            ));
        }
        Ok(column(columns, given("lookup"), &LOOKUP).map(Locate::Country))
    };
    let mode = location["mode"].as_str().unwrap_or("auto");
    let found = match mode {
        "coords" => coordinates(),
        "geohash" => hashed(),
        "lookup" => looked_up()?,
        "auto" => match coordinates().or_else(hashed) {
            Some(found) => Some(found),
            None => looked_up()?,
        },
        other => return Err(format!("Its places are given as {other}, which DOC does not read.")),
    };
    found.ok_or_else(|| {
        let wanted = match mode {
            "coords" => format!(
                "the columns {} and {}",
                given("latitude").unwrap_or("latitude"),
                given("longitude").unwrap_or("longitude")
            ),
            "geohash" => format!("the column {}", given("geohash").unwrap_or("geohash")),
            "lookup" => format!("the column {}", given("lookup").unwrap_or("lookup")),
            _ => "a latitude and longitude, a geohash or a lookup column".to_string(),
        };
        let seen = if columns.is_empty() { "none".to_string() } else { columns.join(", ") };
        format!(
            "No column in its answer says where a row is: it looks for {wanted}, and the answer \
             has {seen}."
        )
    })
}

/// The map a geomap panel draws, as `data-geomap`, and how many places are on it.
pub fn map(panel: &Panel, answer: &Answer, unit: &str) -> Result<(Value, usize), String> {
    let mut layers: Vec<Value> = panel.options["layers"].as_array().cloned().unwrap_or_default();
    if layers.is_empty() {
        layers.push(json!({ "type": "markers" }));
    }
    let skipped: Vec<String> = layers
        .iter()
        .filter_map(|layer| layer["type"].as_str())
        .filter(|kind| !DRAWN.contains(kind))
        .map(str::to_string)
        .collect();
    let names = renamed(panel);
    let mut points = Vec::new();
    let mut found = 0;
    let drawn =
        layers.iter().filter(|layer| DRAWN.contains(&layer["type"].as_str().unwrap_or("markers")));
    for layer in drawn {
        let only = match layer["filterData"]["id"].as_str() {
            Some("byRefId") => layer["filterData"]["options"].as_str(),
            _ => None,
        };
        let style = &layer["config"]["style"];
        let sized = style["size"]["field"].as_str().or_else(|| style["color"]["field"].as_str());
        let labelled = style["text"]["field"].as_str();
        let frames =
            answer.frames.iter().filter(|frame| only.is_none_or(|only| frame.ref_id == only));
        for frame in frames {
            let mut rows = rows(frame);
            rows.iter_mut().for_each(|row| rename(row, &names));
            let Some(first) = rows.first() else { continue };
            let columns: Vec<String> = first.values.keys().cloned().collect();
            let locate = locate(&columns, &layer["location"])?;
            let used = locate.columns();
            let value = column(&columns, sized, &[]).or_else(|| {
                columns
                    .iter()
                    .filter(|name| !used.contains(&name.as_str()) && *name != "Time")
                    .find(|name| first.values[*name].is_number())
                    .cloned()
            });
            let label =
                column(&columns, labelled, &NAMES).filter(|name| !used.contains(&name.as_str()));
            for row in &rows {
                let mut point = match &locate {
                    Locate::Coordinates(latitude, longitude) => {
                        match (number(row.values.get(latitude)), number(row.values.get(longitude)))
                        {
                            (Some(lat), Some(lon)) => json!({ "lat": lat, "lon": lon }),
                            _ => continue,
                        }
                    }
                    Locate::Geohash(column) => {
                        match row.values.get(column).and_then(Value::as_str).and_then(geohash) {
                            Some((lat, lon)) => json!({ "lat": lat, "lon": lon }),
                            None => continue,
                        }
                    }
                    Locate::Country(column) => match row.values.get(column).map(cell_text) {
                        Some(country) if !country.is_empty() => json!({ "country": country }),
                        _ => continue,
                    },
                };
                found += 1;
                if points.len() >= MOST_PLACES {
                    continue;
                }
                let named_as = label.as_ref().and_then(|name| row.values.get(name)).map(cell_text);
                if let Some(named_as) = named_as.filter(|text| !text.is_empty()) {
                    point["label"] = json!(named_as);
                }
                if let Some(name) = &value
                    && let Some(amount) = number(row.values.get(name))
                {
                    let unit = row.units.get(name).map_or(unit, String::as_str);
                    point["value"] = json!(amount);
                    point["shown"] = json!(figure(amount, unit, panel.decimals()));
                }
                points.push(point);
            }
        }
    }
    if found == 0 {
        return Err(match skipped.as_slice() {
            [] => "Nothing in this range has a place to show.".to_string(),
            [one] => {
                format!("DOC draws marker and heatmap layers, and this map has only a {one} layer.")
            }
            many => format!(
                "DOC draws marker and heatmap layers, and this map has only {} layers.",
                many.join(" and ")
            ),
        });
    }
    let fit = match panel.options["view"]["id"].as_str().unwrap_or("zero") {
        "zero" => "world",
        _ => "points",
    };
    Ok((json!({ "points": points, "fit": fit }), found))
}
