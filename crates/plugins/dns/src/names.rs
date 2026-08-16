//! Domain names as the plugin keeps and compares them: lowercase ASCII, international names in
//! their punycode form, and no trailing dot, such as `api.internal.example.com`.

use hickory_proto::rr::Name;

const MAX_NAME: usize = 253;
const MAX_LABEL: usize = 63;

/// A domain name as typed, made normal, or why it isn't one. A leading `*` label is kept, since a
/// record may be a wildcard; `owner` and `zone` say where that is allowed.
pub fn normal(text: &str) -> Result<String, String> {
    let trimmed = text.trim().trim_end_matches('.');
    if trimmed.is_empty() {
        return Err("a name is needed".into());
    }
    let ascii = match trimmed.is_ascii() {
        true => trimmed.to_ascii_lowercase(),
        false => Name::from_utf8(trimmed)
            .map(|name| name.to_ascii().trim_end_matches('.').to_ascii_lowercase())
            .map_err(|_| format!("{trimmed} is not a domain name"))?,
    };
    if ascii.len() > MAX_NAME {
        return Err(format!("a domain name is at most {MAX_NAME} characters"));
    }
    for (index, label) in ascii.split('.').enumerate() {
        let fine = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
        let wildcard = index == 0 && label == "*";
        if label.is_empty() || label.len() > MAX_LABEL {
            return Err(format!(
                "{trimmed} is not a domain name: each part between dots is 1 to {MAX_LABEL} \
                 characters"
            ));
        }
        if !wildcard && (!label.chars().all(fine) || label.starts_with('-') || label.ends_with('-'))
        {
            return Err(format!(
                "{trimmed} is not a domain name: use letters, digits, - and _, with no - at the \
                 start or end of a part"
            ));
        }
    }
    Ok(ascii)
}

/// A name that is never a wildcard, such as a domain DOC owns or where a record points.
pub fn plain(text: &str) -> Result<String, String> {
    let name = normal(text)?;
    match name.starts_with("*.") || name == "*" {
        true => Err(format!("{name} is a wildcard; name it in full")),
        false => Ok(name),
    }
}

/// Whether `name` is `zone` or under it.
pub fn within(name: &str, zone: &str) -> bool {
    name == zone
        || name.strip_suffix(zone).is_some_and(|head| head.ends_with('.') && head.len() > 1)
}

/// The name one label up, if there is one.
pub fn parent(name: &str) -> Option<&str> {
    name.split_once('.').map(|(_, rest)| rest)
}

/// `name` as a name in a DNS message, fully qualified.
pub fn fqdn(name: &str) -> Option<Name> {
    Name::from_ascii(format!("{name}.")).ok()
}

/// A name from a DNS message, made normal: empty for the root.
pub fn from_wire(name: &Name) -> String {
    name.to_lowercase().to_ascii().trim_end_matches('.').to_string()
}

/// The part of `name` before `zone`: `api` for `api.internal.example.com` in
/// `internal.example.com`, and `@` for the domain itself.
pub fn host(name: &str, zone: &str) -> String {
    match name.strip_suffix(zone).and_then(|head| head.strip_suffix('.')) {
        Some(head) if !head.is_empty() => head.to_string(),
        _ => "@".to_string(),
    }
}
