//! A page's sanitised HTML as Markdown, for an MCP client to read. Headings, lists, code, quotes,
//! links, images and tables keep their shape; any other element is kept as its text.

enum Token<'a> {
    Text(&'a str),
    Open(String, Vec<(String, String)>),
    Close(String),
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';').filter(|end| *end <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let decoded = match &rest[1..end] {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            entity => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .map(|hex| u32::from_str_radix(hex, 16))
                .or_else(|| entity.strip_prefix('#').map(str::parse::<u32>))
                .and_then(Result::ok)
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A tag's name and attributes; values may hold `>`, since the serialiser only escapes quotes.
fn tag(inside: &str) -> Token<'static> {
    let (closing, inside) = match inside.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, inside),
    };
    let inside = inside.trim_end_matches('/').trim();
    let name_end = inside.find(|c: char| c.is_whitespace()).unwrap_or(inside.len());
    let name = inside[..name_end].to_ascii_lowercase();
    if closing {
        return Token::Close(name);
    }
    let mut attributes = Vec::new();
    let mut rest = inside[name_end..].trim_start();
    while !rest.is_empty() {
        let key_end = rest.find(|c: char| c == '=' || c.is_whitespace()).unwrap_or(rest.len());
        let key = rest[..key_end].to_ascii_lowercase();
        rest = rest[key_end..].trim_start();
        let mut value = String::new();
        if let Some(after) = rest.strip_prefix('=') {
            let after = after.trim_start();
            let (quoted, body) = match after.chars().next() {
                Some(quote @ ('"' | '\'')) => (Some(quote), &after[1..]),
                _ => (None, after),
            };
            let end = match quoted {
                Some(quote) => body.find(quote).unwrap_or(body.len()),
                None => body.find(char::is_whitespace).unwrap_or(body.len()),
            };
            value = unescape(&body[..end]);
            rest = body.get(end + usize::from(quoted.is_some())..).unwrap_or_default().trim_start();
        }
        if !key.is_empty() {
            attributes.push((key, value));
        }
    }
    Token::Open(name, attributes)
}

fn tokens(html: &str) -> Vec<Token<'_>> {
    let mut found = Vec::new();
    let mut rest = html;
    while !rest.is_empty() {
        let Some(open) = rest.find('<') else {
            found.push(Token::Text(rest));
            break;
        };
        if open > 0 {
            found.push(Token::Text(&rest[..open]));
        }
        rest = &rest[open + 1..];
        if let Some(comment) = rest.strip_prefix("!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
            continue;
        }
        let mut quote = None;
        let mut end = rest.len();
        for (at, c) in rest.char_indices() {
            match (quote, c) {
                (None, '"' | '\'') => quote = Some(c),
                (Some(q), c) if c == q => quote = None,
                (None, '>') => {
                    end = at;
                    break;
                }
                _ => {}
            }
        }
        found.push(tag(&rest[..end]));
        rest = rest.get(end + 1..).unwrap_or_default();
    }
    found
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    cell: Option<String>,
}

#[derive(Default)]
struct Writer {
    out: String,
    base: String,
    quote: usize,
    pre: usize,
    /// One entry for each open list: the next number of an ordered one.
    lists: Vec<Option<usize>>,
    links: Vec<Option<String>>,
    table: Option<Table>,
    /// A break still to be written, and how deep in quotes it was asked for.
    pending: Option<(bool, usize)>,
    line_start: bool,
}

impl Writer {
    fn prefix(depth: usize) -> String {
        "> ".repeat(depth)
    }

    fn block(&mut self) {
        if !self.out.is_empty() && self.table.is_none() {
            self.pending = Some((true, self.quote));
        }
    }

    fn line(&mut self) {
        let blank = matches!(self.pending, Some((true, _)));
        if !self.out.is_empty() && self.table.is_none() && !blank {
            self.pending = Some((false, self.quote));
        }
    }

    /// A list item's or heading's marker, after which the text starts its line.
    fn marker(&mut self, text: &str) {
        self.raw(text);
        self.line_start = true;
    }

    fn flush(&mut self) {
        if let Some((blank, depth)) = self.pending.take() {
            let trimmed = self.out.trim_end_matches(' ').len();
            self.out.truncate(trimmed);
            self.out.push('\n');
            if blank {
                self.out.push_str(Self::prefix(depth.min(self.quote)).trim_end());
                self.out.push('\n');
            }
            self.out.push_str(&Self::prefix(self.quote));
            self.line_start = true;
        }
    }

    /// Markup or text written as it is, as long as it is not inside a table cell.
    fn raw(&mut self, text: &str) {
        match self.table.as_mut() {
            Some(table) => {
                if let Some(cell) = table.cell.as_mut() {
                    cell.push_str(text);
                }
            }
            None => {
                self.flush();
                self.out.push_str(text);
                self.line_start = false;
            }
        }
    }

    fn text(&mut self, text: &str) {
        let text = unescape(text);
        if self.pre > 0 && self.table.is_none() {
            self.flush();
            self.out.push_str(&text.replace('\n', &format!("\n{}", Self::prefix(self.quote))));
            self.line_start = text.ends_with('\n');
            return;
        }
        let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let spaced =
            match (text.starts_with(char::is_whitespace), text.ends_with(char::is_whitespace)) {
                _ if collapsed.is_empty() => {
                    if !text.is_empty() && !self.line_start && self.pending.is_none() {
                        self.raw(" ");
                    }
                    return;
                }
                (true, true) => format!(" {collapsed} "),
                (true, false) => format!(" {collapsed}"),
                (false, true) => format!("{collapsed} "),
                (false, false) => collapsed,
            };
        let spaced = match self.table.is_some() {
            true => spaced.replace('|', "\\|"),
            false => spaced,
        };
        if self.table.is_none() {
            self.flush();
        }
        let spaced = if self.line_start { spaced.trim_start().to_string() } else { spaced };
        self.raw(&spaced);
    }

    fn address(&self, url: &str) -> String {
        match url.starts_with('/') && !url.starts_with("//") {
            true => format!("{}{url}", self.base),
            false => url.to_string(),
        }
    }

    fn open(&mut self, name: &str, attributes: &[(String, String)]) {
        let attribute = |key: &str| {
            attributes.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
        };
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.block();
                let level = usize::from(name.as_bytes()[1] - b'0');
                self.marker(&format!("{} ", "#".repeat(level)));
            }
            "p" | "div" | "section" | "article" | "header" | "footer" | "dl" => {
                if self.lists.is_empty() {
                    self.block();
                }
            }
            "dt" | "dd" | "figcaption" => self.line(),
            "br" => self.line(),
            "hr" => {
                self.block();
                self.raw("---");
                self.block();
            }
            "blockquote" => {
                self.block();
                self.quote += 1;
            }
            "pre" => {
                self.block();
                self.raw("```");
                self.line();
                self.flush();
                self.pre += 1;
            }
            "code" if self.pre == 0 => self.raw("`"),
            "strong" | "b" => self.raw("**"),
            "em" | "i" => self.raw("_"),
            "del" | "s" => self.raw("~~"),
            "ul" | "ol" => {
                match self.lists.is_empty() {
                    true => self.block(),
                    false => self.line(),
                }
                self.lists.push((name == "ol").then_some(1));
            }
            "li" => {
                self.line();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(Some(next)) => {
                        *next += 1;
                        format!("{}. ", *next - 1)
                    }
                    _ => "- ".into(),
                };
                self.marker(&format!("{}{marker}", "   ".repeat(depth)));
            }
            "a" => {
                let href = attribute("href")
                    .filter(|href| !href.is_empty())
                    .map(|href| self.address(href));
                if href.is_some() {
                    self.raw("[");
                }
                self.links.push(href);
            }
            "img" => {
                let alt = attribute("alt").unwrap_or_default().replace(['[', ']'], "");
                if let Some(source) = attribute("src").filter(|source| !source.is_empty()) {
                    let source = self.address(source);
                    self.raw(&format!("![{alt}]({source})"));
                }
            }
            "table" => {
                self.block();
                self.flush();
                self.table = Some(Table::default());
            }
            "tr" => {
                if let Some(table) = self.table.as_mut() {
                    table.rows.push(Vec::new());
                }
            }
            "th" | "td" => {
                if let Some(table) = self.table.as_mut() {
                    table.cell = Some(String::new());
                }
            }
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "hr" => self.block(),
            "p" | "div" | "section" | "article" | "header" | "footer" | "dl" => {
                match self.lists.is_empty() {
                    true => self.block(),
                    false => self.line(),
                }
            }
            "blockquote" => {
                self.quote = self.quote.saturating_sub(1);
                self.block();
            }
            "pre" => {
                self.pre = self.pre.saturating_sub(1);
                if !self.out.ends_with(&format!("\n{}", Self::prefix(self.quote))) {
                    self.line();
                }
                self.raw("```");
                self.block();
            }
            "code" if self.pre == 0 => self.raw("`"),
            "strong" | "b" => self.raw("**"),
            "em" | "i" => self.raw("_"),
            "del" | "s" => self.raw("~~"),
            "ul" | "ol" => {
                self.lists.pop();
                match self.lists.is_empty() {
                    true => self.block(),
                    false => self.line(),
                }
            }
            "a" => {
                if let Some(Some(href)) = self.links.pop() {
                    self.raw(&format!("]({href})"));
                }
            }
            "th" | "td" => {
                if let Some(table) = self.table.as_mut()
                    && let Some(cell) = table.cell.take()
                    && let Some(row) = table.rows.last_mut()
                {
                    row.push(cell.trim().to_string());
                }
            }
            "table" => {
                if let Some(table) = self.table.take() {
                    self.table_out(&table.rows);
                }
                self.block();
            }
            _ => {}
        }
    }

    fn table_out(&mut self, rows: &[Vec<String>]) {
        let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        for (index, row) in rows.iter().enumerate() {
            let mut cells: Vec<&str> = row.iter().map(String::as_str).collect();
            cells.resize(columns, "");
            self.raw(&format!("| {} |", cells.join(" | ")));
            self.line();
            if index == 0 {
                self.raw(&format!("|{}", " --- |".repeat(columns)));
                self.line();
            }
        }
    }
}

/// `base` is put before links and images that start at `/`, so they work outside DOC.
pub fn markdown(html: &str, base: &str) -> String {
    let mut writer = Writer {
        base: base.trim_end_matches('/').to_string(),
        line_start: true,
        ..Writer::default()
    };
    for token in tokens(html) {
        match token {
            Token::Text(text) => writer.text(text),
            Token::Open(name, attributes) => writer.open(&name, &attributes),
            Token::Close(name) => writer.close(&name),
        }
    }
    let lines: Vec<&str> = writer.out.lines().map(str::trim_end).collect();
    lines.join("\n").trim().to_string()
}
