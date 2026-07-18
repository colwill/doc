//! A layered layout: one column per kind, in the order its caller gives — for the catalogue's map
//! that is the order the catalogue declares its kinds in — and each column ordered to cross as few
//! connections as it can. A column the caller did not name follows the ones it did.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub const NODE_WIDTH: u32 = 200;
pub const NODE_HEIGHT: u32 = 36;
const COLUMN_GAP: u32 = 70;
const ROW_GAP: u32 = 14;
const MARGIN: u32 = 10;
const HEADING: u32 = 34;
const SWEEPS: usize = 4;

/// What a layout needs of a node, wherever the graph came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub column: String,
    pub label: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub href: Option<String>,
    #[serde(default)]
    pub focus: bool,
    /// Where it sits in its column, for a column whose sequence means something, such as the
    /// actions an automation takes in turn. A column with any is kept in that order, never
    /// rearranged to cross fewer links; items without one follow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<u32>,
    /// Something not there yet, such as a place to add one, drawn dashed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ghost: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub derived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Placed {
    #[serde(flatten)]
    pub item: Item,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub label: String,
    pub x: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    #[serde(flatten)]
    pub link: Link,
    /// An SVG path from the right side of the node on the left to the left side of the other.
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layout {
    pub columns: Vec<Column>,
    pub nodes: Vec<Placed>,
    pub edges: Vec<Route>,
    pub width: u32,
    pub height: u32,
}

fn average(positions: impl Iterator<Item = usize>) -> Option<f64> {
    let (sum, count) = positions.fold((0usize, 0usize), |(sum, count), at| (sum + at, count + 1));
    (count > 0).then(|| sum as f64 / count as f64)
}

pub fn lay_out(items: &[Item], links: Vec<Link>, order: &[String]) -> Layout {
    let mut columns: Vec<String> = order
        .iter()
        .filter(|kind| items.iter().any(|item| &item.column == *kind))
        .cloned()
        .collect();
    for item in items {
        if !columns.contains(&item.column) {
            columns.push(item.column.clone());
        }
    }
    let mut stacks: Vec<Vec<Item>> = columns
        .iter()
        .map(|column| {
            let mut stack: Vec<Item> =
                items.iter().filter(|item| &item.column == column).cloned().collect();
            match stack.iter().any(|item| item.order.is_some()) {
                true => stack.sort_by_key(|item| item.order.unwrap_or(u32::MAX)),
                false => {
                    stack.sort_by(|a, b| b.focus.cmp(&a.focus).then_with(|| a.label.cmp(&b.label)))
                }
            }
            stack
        })
        .collect();
    let joined: BTreeSet<(String, String)> = links
        .iter()
        .flat_map(|link| {
            [(link.from.clone(), link.to.clone()), (link.to.clone(), link.from.clone())]
        })
        .collect();
    for sweep in 0..SWEEPS {
        let indices: Vec<usize> = match sweep % 2 {
            0 => (1..stacks.len()).collect(),
            _ => (0..stacks.len().saturating_sub(1)).rev().collect(),
        };
        for index in indices {
            if stacks[index].iter().any(|item| item.order.is_some()) {
                continue;
            }
            let beside = if sweep % 2 == 0 { index - 1 } else { index + 1 };
            let rows: BTreeMap<String, usize> = stacks[beside]
                .iter()
                .enumerate()
                .map(|(row, item)| (item.id.clone(), row))
                .collect();
            let mut keyed: Vec<(f64, usize, Item)> = stacks[index]
                .drain(..)
                .enumerate()
                .map(|(row, item)| {
                    let near = rows
                        .iter()
                        .filter(|(other, _)| joined.contains(&(item.id.clone(), (*other).clone())));
                    let key = average(near.map(|(_, at)| *at)).unwrap_or(row as f64);
                    (key, row, item)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            stacks[index] = keyed.into_iter().map(|(_, _, item)| item).collect();
        }
    }
    let mut placed = Vec::new();
    let mut at: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    for (column, stack) in stacks.iter().enumerate() {
        let x = MARGIN + column as u32 * (NODE_WIDTH + COLUMN_GAP);
        for (row, item) in stack.iter().enumerate() {
            let y = MARGIN + HEADING + row as u32 * (NODE_HEIGHT + ROW_GAP);
            at.insert(item.id.clone(), (x, y));
            placed.push(Placed {
                item: item.clone(),
                x,
                y,
                width: NODE_WIDTH,
                height: NODE_HEIGHT,
            });
        }
    }
    let edges = links
        .into_iter()
        .filter_map(|link| {
            let (a, b) = (at.get(&link.from)?, at.get(&link.to)?);
            let (left, right) = if a.0 <= b.0 { (a, b) } else { (b, a) };
            let (x1, y1) = (left.0 + NODE_WIDTH, left.1 + NODE_HEIGHT / 2);
            let (x2, y2) = (right.0, right.1 + NODE_HEIGHT / 2);
            let bend = (x2.saturating_sub(x1)) / 2;
            let path = format!("M {x1} {y1} C {} {y1}, {} {y2}, {x2} {y2}", x1 + bend, x2 - bend);
            Some(Route { link, path })
        })
        .collect();
    let tallest = stacks.iter().map(Vec::len).max().unwrap_or(0) as u32;
    let columns: Vec<Column> = columns
        .into_iter()
        .enumerate()
        .map(|(index, label)| Column {
            label,
            x: MARGIN + index as u32 * (NODE_WIDTH + COLUMN_GAP),
        })
        .collect();
    let width = MARGIN * 2 + (columns.len() as u32).max(1) * (NODE_WIDTH + COLUMN_GAP) - COLUMN_GAP;
    let height = MARGIN * 2 + HEADING + tallest.max(1) * (NODE_HEIGHT + ROW_GAP);
    Layout { columns, nodes: placed, edges, width, height }
}
