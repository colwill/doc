//! The little language a template is written in. It fills in `{{ values.name }}`, chooses with
//! `{% if %}` and repeats with `{% for %}`, and nothing else: there is no arithmetic, no function
//! call and no way out of the context it is given, because a template runs as whoever launched it.

use serde_json::Value;

/// Nothing a template renders may grow past this, so a loop cannot fill the run's memory.
const MAX_OUTPUT: usize = 1024 * 1024;
/// How many times one `{% for %}` may go round.
const MAX_ITERATIONS: usize = 1_000;
/// How deeply blocks may nest.
const MAX_DEPTH: usize = 10;

#[derive(Debug, Clone, PartialEq)]
enum Term {
    /// A dotted path into the context, such as `values.name`.
    Path(String),
    /// A quoted string, which is how `{{ '{{' }}` writes a brace a template should keep.
    Literal(String),
}

#[derive(Debug, Clone, PartialEq)]
struct Expression {
    term: Term,
    filters: Vec<Filter>,
    /// `{% if !values.private %}`: true when what it reads is false.
    negated: bool,
    /// `{% if values.app == 'cli' %}`: what it is compared with, and whether they must match.
    compared: Option<(Box<Expression>, bool)>,
}

#[derive(Debug, Clone, PartialEq)]
struct Filter {
    name: String,
    argument: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Text(String),
    Value(Expression),
    If { test: Expression, then: Vec<Node>, otherwise: Vec<Node> },
    For { binding: String, over: Expression, body: Vec<Node> },
}

/// A piece of the source: text, `{{ … }}` or `{% … %}`.
#[derive(Debug, Clone, PartialEq)]
enum Piece {
    Text(String),
    Value(String),
    Tag(String),
}

fn pieces(source: &str) -> Result<Vec<Piece>, String> {
    let bytes = source.as_bytes();
    let (mut pieces, mut text, mut at) = (Vec::new(), String::new(), 0usize);
    while at < bytes.len() {
        let opening = source[at..].find("{{").map(|found| (at + found, "}}", true));
        let tag = source[at..].find("{%").map(|found| (at + found, "%}", false));
        let (start, closing, is_value) = match (opening, tag) {
            (Some(value), Some(tag)) if value.0 <= tag.0 => value,
            (Some(value), None) => value,
            (_, Some(tag)) => tag,
            (None, None) => break,
        };
        text.push_str(&source[at..start]);
        let rest = &source[start + 2..];
        let end = closes(rest, closing)
            .ok_or_else(|| format!("`{}` was never closed", &source[start..start + 2]))?;
        let inside = rest[..end].trim().to_string();
        at = start + 2 + end + 2;
        if !is_value && inside == "raw" {
            let (kept, after) =
                raw(&source[at..]).ok_or("`{% raw %}` was never closed with `{% endraw %}`")?;
            text.push_str(kept);
            at += after;
            continue;
        }
        if !text.is_empty() {
            pieces.push(Piece::Text(std::mem::take(&mut text)));
        }
        pieces.push(match is_value {
            true => Piece::Value(inside),
            false => Piece::Tag(inside),
        });
    }
    text.push_str(&source[at..]);
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    Ok(pieces)
}

/// What `{% raw %}` keeps as it is written — Go's `html/template`, say, whose `{{ .Name }}` is not
/// this language's — and how far past its `{% endraw %}` the source goes on.
fn raw(rest: &str) -> Option<(&str, usize)> {
    let mut from = 0;
    while let Some(found) = rest[from..].find("{%") {
        let opening = from + found;
        let close = rest[opening + 2..].find("%}")?;
        if rest[opening + 2..opening + 2 + close].trim() == "endraw" {
            return Some((&rest[..opening], opening + 2 + close + 2));
        }
        from = opening + 2;
    }
    None
}

/// Where `close` ends the tag that `rest` begins, leaving alone one inside quotes, which is how
/// `{{ '}}' }}` writes a brace a template should keep.
fn closes(rest: &str, close: &str) -> Option<usize> {
    let mut quote = None::<char>;
    for (index, letter) in rest.char_indices() {
        match (quote, letter) {
            (Some(open), letter) if letter == open => quote = None,
            (Some(_), _) => {}
            (None, '\'') | (None, '"') => quote = Some(letter),
            (None, _) if rest[index..].starts_with(close) => return Some(index),
            (None, _) => {}
        }
    }
    None
}

/// `values.name | lower | default('service')`, as an expression and its filters.
fn expression(source: &str) -> Result<Expression, String> {
    // A comparison is two expressions, so it is split off before anything else is read.
    for (operator, must_match) in [("==", true), ("!=", false)] {
        if let Some(at) = outside_quotes(source, operator) {
            let (left, right) = source.split_at(at);
            let mut left = expression(left)?;
            let right = expression(&right[operator.len()..])?;
            if left.compared.is_some() {
                return Err("an expression compares two things, not three".into());
            }
            left.compared = Some((Box::new(right), must_match));
            return Ok(left);
        }
    }
    let (negated, source) = match source.trim().strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, source),
    };
    let mut parts = split_filters(source)?;
    if parts.is_empty() {
        return Err("an expression is empty".into());
    }
    let head = parts.remove(0);
    let term = match head.chars().next() {
        Some('\'') | Some('"') => Term::Literal(unquote(&head)?),
        _ => Term::Path(path(&head)?),
    };
    let mut filters = Vec::new();
    for part in parts {
        let (name, argument) = match part.split_once('(') {
            Some((name, rest)) => {
                let rest = rest
                    .strip_suffix(')')
                    .ok_or_else(|| format!("the filter `{}` was never closed", name.trim()))?;
                (name.trim().to_string(), Some(unquote(rest.trim())?))
            }
            None => (part.trim().to_string(), None),
        };
        if !KNOWN.contains(&name.as_str()) {
            return Err(format!(
                "there is no filter called `{name}`; there is {}",
                KNOWN.join(", ")
            ));
        }
        filters.push(Filter { name, argument });
    }
    Ok(Expression { term, filters, negated, compared: None })
}

/// Where `operator` appears in `source` outside any quoted value.
fn outside_quotes(source: &str, operator: &str) -> Option<usize> {
    let mut quote = None::<char>;
    for (at, letter) in source.char_indices() {
        match (quote, letter) {
            (Some(open), letter) if letter == open => quote = None,
            (Some(_), _) => {}
            (None, '\'') | (None, '"') => quote = Some(letter),
            (None, _) if source[at..].starts_with(operator) => return Some(at),
            (None, _) => {}
        }
    }
    None
}

const KNOWN: [&str; 12] = [
    "lower", "upper", "kebab", "snake", "pascal", "title", "trim", "json", "length", "default",
    "join", "name",
];

/// Splits on `|`, leaving the pipes inside quotes alone.
fn split_filters(source: &str) -> Result<Vec<String>, String> {
    let (mut parts, mut current, mut quote) = (Vec::new(), String::new(), None::<char>);
    for letter in source.chars() {
        match (quote, letter) {
            (Some(open), letter) if letter == open => {
                quote = None;
                current.push(letter);
            }
            (Some(_), letter) => current.push(letter),
            (None, '\'') | (None, '"') => {
                quote = Some(letter);
                current.push(letter);
            }
            (None, '|') => parts.push(std::mem::take(&mut current).trim().to_string()),
            (None, letter) => current.push(letter),
        }
    }
    if quote.is_some() {
        return Err("a quoted value was never closed".into());
    }
    let last = current.trim().to_string();
    if !last.is_empty() {
        parts.push(last);
    }
    Ok(parts.into_iter().filter(|part| !part.is_empty()).collect())
}

fn unquote(source: &str) -> Result<String, String> {
    let source = source.trim();
    let mut letters = source.chars();
    match (letters.next(), source.chars().last()) {
        (Some('\''), Some('\'')) | (Some('"'), Some('"')) if source.len() >= 2 => {
            Ok(source[1..source.len() - 1].to_string())
        }
        _ => Err(format!("`{source}` is not a quoted value")),
    }
}

/// A dotted path, which is all a template may reach for.
fn path(source: &str) -> Result<String, String> {
    let source = source.trim();
    let allowed = |letter: char| letter.is_alphanumeric() || matches!(letter, '_' | '-' | '.');
    if source.is_empty() || !source.chars().all(allowed) {
        return Err(format!("`{source}` is not a name a template can read"));
    }
    Ok(source.to_string())
}

/// Parses the pieces from `at` until one of `until` (which is left unconsumed by the caller's loop).
fn parse(
    pieces: &[Piece],
    at: &mut usize,
    until: &[&str],
    depth: usize,
) -> Result<Vec<Node>, String> {
    if depth > MAX_DEPTH {
        return Err(format!("blocks are nested more than {MAX_DEPTH} deep"));
    }
    let mut nodes = Vec::new();
    while let Some(piece) = pieces.get(*at) {
        match piece {
            Piece::Text(text) => {
                nodes.push(Node::Text(text.clone()));
                *at += 1;
            }
            Piece::Value(source) => {
                nodes.push(Node::Value(expression(source)?));
                *at += 1;
            }
            Piece::Tag(tag) => {
                let keyword = tag.split_whitespace().next().unwrap_or_default();
                if until.contains(&keyword) {
                    return Ok(nodes);
                }
                *at += 1;
                match keyword {
                    "if" => {
                        let test = expression(tag.trim_start_matches("if").trim())?;
                        let then = parse(pieces, at, &["else", "endif"], depth + 1)?;
                        let mut otherwise = Vec::new();
                        match pieces.get(*at) {
                            Some(Piece::Tag(next)) if next.trim() == "else" => {
                                *at += 1;
                                otherwise = parse(pieces, at, &["endif"], depth + 1)?;
                                expect(pieces, at, "endif")?;
                            }
                            _ => expect(pieces, at, "endif")?,
                        }
                        nodes.push(Node::If { test, then, otherwise });
                    }
                    "for" => {
                        let rest = tag.trim_start_matches("for").trim();
                        let (binding, over) = rest
                            .split_once(" in ")
                            .ok_or_else(|| format!("`{tag}` should read `for x in values.list`"))?;
                        let binding = path(binding)?;
                        if binding.contains('.') {
                            return Err(format!("`{binding}` is not a name a loop can bind"));
                        }
                        let body = parse(pieces, at, &["endfor"], depth + 1)?;
                        expect(pieces, at, "endfor")?;
                        nodes.push(Node::For { binding, over: expression(over)?, body });
                    }
                    other => return Err(format!("there is no `{other}` block")),
                }
            }
        }
    }
    match until.is_empty() {
        true => Ok(nodes),
        false => Err(format!("a block was never closed with `{{% {} %}}`", until[until.len() - 1])),
    }
}

fn expect(pieces: &[Piece], at: &mut usize, keyword: &str) -> Result<(), String> {
    match pieces.get(*at) {
        Some(Piece::Tag(tag)) if tag.trim() == keyword => {
            *at += 1;
            Ok(())
        }
        _ => Err(format!("`{{% {keyword} %}}` is missing")),
    }
}

/// What a path names in the context, or null.
pub fn lookup(context: &Value, path: &str) -> Value {
    let mut here = context;
    for part in path.split('.') {
        here = match here {
            Value::Object(fields) => fields.get(part).unwrap_or(&Value::Null),
            Value::Array(items) => match part.parse::<usize>() {
                Ok(index) => items.get(index).unwrap_or(&Value::Null),
                Err(_) => &Value::Null,
            },
            _ => &Value::Null,
        };
    }
    here.clone()
}

/// Whether a value counts as true: a set string, a number that is not zero, a list with something
/// in it, or `true` itself.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(yes) => *yes,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.trim().is_empty() && text != "false",
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

/// A value as a template writes it: a string as itself, everything else as JSON.
fn written(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn kebab(text: &str) -> String {
    let letters: Vec<char> = text.chars().collect();
    let mut out = String::new();
    for (at, letter) in letters.iter().copied().enumerate() {
        if !letter.is_alphanumeric() {
            if !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
            continue;
        }
        let before = at.checked_sub(1).map(|previous| letters[previous]);
        let after = letters.get(at + 1).copied();
        // `paymentsAPI` and `APIKey` each break into two words; `API` alone stays one.
        let word = letter.is_uppercase()
            && before.is_some_and(|before| {
                before.is_lowercase()
                    || before.is_ascii_digit()
                    || (before.is_uppercase() && after.is_some_and(char::is_lowercase))
            });
        if word && !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
        out.extend(letter.to_lowercase());
    }
    out.trim_matches('-').to_string()
}

fn pascal(text: &str) -> String {
    kebab(text)
        .split('-')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut letters = word.chars();
            match letters.next() {
                Some(first) => first.to_uppercase().collect::<String>() + letters.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn titled(text: &str) -> String {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|word| {
            let mut letters = word.chars();
            match letters.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &letters.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect();
    words.join(" ")
}

fn filtered(value: Value, filter: &Filter) -> Result<Value, String> {
    let text = || written(&value);
    Ok(match filter.name.as_str() {
        "lower" => Value::String(text().to_lowercase()),
        "upper" => Value::String(text().to_uppercase()),
        "kebab" => Value::String(kebab(&text())),
        "snake" => Value::String(kebab(&text()).replace('-', "_")),
        "pascal" => Value::String(pascal(&text())),
        "title" => Value::String(titled(&text())),
        "trim" => Value::String(text().trim().to_string()),
        // `Team:payments-core` is how the Catalogue's picker answers; `name` is the team itself.
        "name" => {
            let text = text();
            Value::String(text.split_once(':').map_or(text.clone(), |(_, name)| name.to_string()))
        }
        "json" => Value::String(
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".to_string()),
        ),
        "length" => match &value {
            Value::Array(items) => Value::from(items.len()),
            Value::Object(fields) => Value::from(fields.len()),
            other => Value::from(written(other).chars().count()),
        },
        "default" => {
            let fallback = filter
                .argument
                .clone()
                .ok_or_else(|| "`default` needs a value, as `default('none')`".to_string())?;
            match truthy(&value) {
                true => value,
                false => Value::String(fallback),
            }
        }
        "join" => {
            let between = filter.argument.clone().unwrap_or_else(|| ", ".to_string());
            match &value {
                Value::Array(items) => {
                    Value::String(items.iter().map(written).collect::<Vec<_>>().join(&between))
                }
                other => Value::String(written(other)),
            }
        }
        other => return Err(format!("there is no filter called `{other}`")),
    })
}

fn evaluate(expression: &Expression, context: &Value) -> Result<Value, String> {
    let mut value = match &expression.term {
        Term::Literal(text) => Value::String(text.clone()),
        Term::Path(path) => lookup(context, path),
    };
    for filter in &expression.filters {
        value = filtered(value, filter)?;
    }
    if let Some((other, must_match)) = &expression.compared {
        let other = evaluate(other, context)?;
        let same = written(&value) == written(&other);
        return Ok(Value::Bool(same == *must_match));
    }
    Ok(match expression.negated {
        true => Value::Bool(!truthy(&value)),
        false => value,
    })
}

fn write(nodes: &[Node], context: &Value, out: &mut String) -> Result<(), String> {
    for node in nodes {
        if out.len() > MAX_OUTPUT {
            return Err(format!("what it renders is longer than {} KiB", MAX_OUTPUT / 1024));
        }
        match node {
            Node::Text(text) => out.push_str(text),
            Node::Value(expression) => out.push_str(&written(&evaluate(expression, context)?)),
            Node::If { test, then, otherwise } => {
                let taken = match truthy(&evaluate(test, context)?) {
                    true => then,
                    false => otherwise,
                };
                write(taken, context, out)?;
            }
            Node::For { binding, over, body } => {
                let items = match evaluate(over, context)? {
                    Value::Array(items) => items,
                    Value::Null => Vec::new(),
                    other => vec![other],
                };
                if items.len() > MAX_ITERATIONS {
                    return Err(format!("a loop may go round at most {MAX_ITERATIONS} times"));
                }
                for (at, item) in items.iter().enumerate() {
                    let mut inner = context.clone();
                    if let Value::Object(fields) = &mut inner {
                        fields.insert(binding.clone(), item.clone());
                        fields.insert("loop".into(), serde_json::json!({ "index": at, "first": at == 0, "last": at + 1 == items.len() }));
                    }
                    write(body, &inner, &mut *out)?;
                }
            }
        }
    }
    Ok(())
}

/// Renders `source` against `context`, saying what is wrong with the template rather than guessing.
pub fn render(source: &str, context: &Value) -> Result<String, String> {
    let pieces = pieces(source)?;
    let mut at = 0;
    let nodes = parse(&pieces, &mut at, &[], 0)?;
    let mut out = String::with_capacity(source.len());
    write(&nodes, context, &mut out)?;
    Ok(out)
}

/// Renders a value a step was configured with, which is usually one line.
pub fn line(source: &str, context: &Value) -> Result<String, String> {
    render(source, context).map(|rendered| rendered.trim().to_string())
}

/// Checks that a template parses, without rendering it.
pub fn check(source: &str) -> Result<(), String> {
    let pieces = pieces(source)?;
    let mut at = 0;
    parse(&pieces, &mut at, &[], 0).map(|_| ())
}
