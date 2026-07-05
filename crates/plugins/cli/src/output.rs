//! Tables for people and the API's own JSON for scripts.

use serde_json::Value;

/// `println!` that stops quietly when the reader has gone, as when piped into `head`.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stdout().lock(), $($arg)*);
    }};
}
pub(crate) use say;

/// A field as a table cell: strings bare, missing values as `-`, lists joined with commas.
pub fn cell(value: &Value, path: &str) -> String {
    let found = match path {
        "" => Some(value),
        path => path.split('.').try_fold(value, |value, key| value.get(key)),
    };
    match found {
        None | Some(Value::Null) => "-".into(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) if items.is_empty() => "-".into(),
        Some(Value::Array(items)) => {
            items.iter().map(|item| cell(item, "")).collect::<Vec<_>>().join(", ")
        }
        Some(other) => other.to_string(),
    }
}

pub struct Table {
    headers: Vec<&'static str>,
    rows: Vec<Vec<String>>,
}

impl Table {
    pub fn new(headers: &[&'static str]) -> Self {
        Self { headers: headers.to_vec(), rows: Vec::new() }
    }

    /// One row per item, each column read from the item by its dotted path.
    pub fn of(headers: &[&'static str], items: &[Value], paths: &[&str]) -> Self {
        let mut table = Self::new(headers);
        for item in items {
            table.rows.push(paths.iter().map(|path| cell(item, path)).collect());
        }
        table
    }

    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    pub fn print(&self) {
        let widths: Vec<usize> = (0..self.headers.len())
            .map(|column| {
                self.rows
                    .iter()
                    .filter_map(|row| row.get(column))
                    .map(|cell| cell.chars().count())
                    .chain([self.headers[column].len()])
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let line = |cells: Vec<&str>| {
            let padded: Vec<String> =
                cells.iter().zip(&widths).map(|(cell, width)| format!("{cell:<width$}")).collect();
            say!("{}", padded.join("  ").trim_end());
        };
        line(self.headers.clone());
        for row in &self.rows {
            line(row.iter().map(String::as_str).collect());
        }
    }
}

pub struct Output {
    pub json: bool,
}

impl Output {
    /// The JSON as the API sent it, or whatever table `render` makes of it.
    pub fn show(&self, value: &Value, render: impl FnOnce(&Value)) {
        match self.json {
            true => say!("{value:#}"),
            false => render(value),
        }
    }

    pub fn done(&self, value: &Value, message: &str) {
        self.show(value, |_| say!("{message}"));
    }
}
