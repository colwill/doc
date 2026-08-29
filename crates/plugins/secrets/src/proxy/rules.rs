//! What a caller may reach through a proxied account: a method and a path pattern, deny by
//! default (ADR-0014 §4), under denials no allowance may cross (§5). Nothing here reads a body or
//! asks the vendor anything — a rule decides on the head alone, which is what lets the proxy
//! stream (§9).

use serde_json::{Value, json};

use crate::store::Denial;

/// Any method at all. `DELETE` is the exception: it is never inherited from `ANY`, because a
/// rule written for reading should not turn out to delete a repository (§5).
pub const ANY: &str = "ANY";

/// The methods a rule may name, and what the form offers.
pub const METHODS: [&str; 7] = [ANY, "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];

/// A method and a path pattern. It is an allowance's grant when it sits on an allowance, and a
/// denial when it sits on an account or on the instance; the matching is the same either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub method: String,
    pub path: String,
}

impl Rule {
    pub fn new(method: &str, path: &str) -> Self {
        Self { method: method.trim().to_ascii_uppercase(), path: tidied(path) }
    }

    /// One rule as an allowance stores it, or nothing when the row is malformed. A malformed rule
    /// grants nothing rather than everything, which is the only safe way for this to fail.
    pub fn read(value: &Value) -> Option<Self> {
        let path = value["path"].as_str()?.trim();
        if path.is_empty() {
            return None;
        }
        let method = value["method"].as_str().unwrap_or(ANY).trim().to_ascii_uppercase();
        if !METHODS.contains(&method.as_str()) {
            return None;
        }
        Some(Self { method, path: tidied(path) })
    }

    /// Every rule on an allowance, in the order they were written.
    pub fn all(rules: &Value) -> Vec<Self> {
        rules.as_array().into_iter().flatten().filter_map(Self::read).collect()
    }

    pub fn of(denial: &Denial) -> Self {
        Self::new(&denial.method, &denial.path)
    }

    pub fn value(&self) -> Value {
        json!({ "method": self.method, "path": self.path })
    }

    /// Whether this rule is about the call: `GET /repos/*/*/pulls/*` and
    /// `GET /repos/acme/payments-api/pulls/41`.
    pub fn covers(&self, method: &str, path: &str) -> bool {
        self.method_covers(method) && matches(&self.path, path)
    }

    fn method_covers(&self, method: &str) -> bool {
        let method = method.to_ascii_uppercase();
        match self.method.as_str() {
            // A HEAD is a GET without the body, so a rule that allows reading allows asking
            // whether there is anything to read.
            "GET" => method == "GET" || method == "HEAD",
            ANY => method != "DELETE",
            named => named == method,
        }
    }

    /// Whether it lets through everything of its method, which is a thing somebody has to mean.
    pub fn sweeping(&self) -> bool {
        matches!(self.path.as_str(), "/**" | "/*")
    }

    pub fn shown(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

/// A path pattern with a leading slash and no trailing one, so `repos/*` and `/repos/*/` are the
/// same rule and nobody is caught out by a slash.
fn tidied(path: &str) -> String {
    let path = path.trim();
    let path = path.strip_prefix('/').unwrap_or(path);
    let path = path.strip_suffix('/').unwrap_or(path);
    format!("/{path}")
}

/// `*` stands for anything within one segment of the path, `**` for any run of segments. So
/// `/repos/*/*/pulls/*` is one repository's pull requests and `/repos/**` is everything under
/// repositories.
pub fn matches(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').filter(|part| !part.is_empty()).collect();
    let path: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    matched(&pattern, &path)
}

fn matched(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", [])) => true,
        Some((&"**", rest)) => (0..=path.len()).any(|from| matched(rest, &path[from..])),
        Some((head, rest)) => match path.split_first() {
            Some((first, tail)) => within(head, first) && matched(rest, tail),
            None => false,
        },
    }
}

/// One segment against one pattern segment, where `*` is any run of characters but a slash.
fn within(pattern: &str, segment: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == segment;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let Some((first, rest)) = parts.split_first() else { return false };
    let Some(mut left) = segment.strip_prefix(first) else { return false };
    let Some((last, middle)) = rest.split_last() else { return true };
    for part in middle {
        match left.find(part) {
            Some(at) => left = &left[at + part.len()..],
            None => return false,
        }
    }
    left.len() >= last.len() && left.ends_with(last)
}

/// The denials DOC writes itself, which are on every account from the moment it is onboarded and
/// which nobody may lift — not an account's manager, not an administrator asking through the
/// proxy (§5). Anything that mints or changes a credential at the vendor walks a caller out of
/// DOC holding something DOC cannot revoke, and every other rule stops meaning anything.
///
/// The patterns are vendor-agnostic on purpose: they are the names vendors give these endpoints,
/// not knowledge of any one vendor's API.
pub fn built_in() -> Vec<Rule> {
    const CREDENTIALS: [&str; 14] = [
        "/**/*token*",
        "/**/*token*/**",
        "/**/*credential*",
        "/**/*credential*/**",
        "/**/*secret*",
        "/**/*secret*/**",
        "/**/*password*",
        "/**/*password*/**",
        "/**/keys",
        "/**/keys/**",
        "/**/*_keys",
        "/**/*_keys/**",
        "/**/*-keys",
        "/**/*-keys/**",
    ];
    const MINTING: [&str; 4] = ["POST", "PUT", "PATCH", "DELETE"];
    MINTING
        .iter()
        .flat_map(|method| CREDENTIALS.iter().map(|path| Rule::new(method, path)))
        .collect()
}

/// Why a built-in denial refused, in the words a caller and the log both get.
pub const MINTING_DENIAL: &str = "nothing may mint or change a credential at the vendor";

#[cfg(test)]
mod deciding {
    use super::*;

    #[test]
    fn a_star_stays_inside_one_segment() {
        assert!(matches("/repos/*/*/pulls/*", "/repos/acme/payments-api/pulls/41"));
        assert!(!matches("/repos/*/pulls/*", "/repos/acme/payments-api/pulls/41"));
        assert!(matches("/git-*", "/git-upload-pack"));
        assert!(!matches("/git-*", "/git/upload-pack"));
    }

    #[test]
    fn two_stars_cross_segments_and_may_match_nothing() {
        assert!(matches("/repos/**", "/repos/acme/payments-api/pulls/41"));
        assert!(matches("/repos/**", "/repos"));
        assert!(matches("/**", "/anything/at/all"));
        assert!(!matches("/repos/**", "/orgs/acme"));
    }

    #[test]
    fn a_slash_either_side_is_the_same_rule() {
        let written = Rule::new("get", "repos/acme/");
        assert_eq!(written.shown(), "GET /repos/acme");
        assert!(written.covers("GET", "/repos/acme"));
    }

    #[test]
    fn delete_is_never_inherited_from_any() {
        let sweeping = Rule::new(ANY, "/**");
        assert!(sweeping.covers("POST", "/repos/acme/payments-api"));
        assert!(!sweeping.covers("DELETE", "/repos/acme/payments-api"));
        assert!(
            Rule::new("DELETE", "/repos/acme/payments-api")
                .covers("DELETE", "/repos/acme/payments-api")
        );
    }

    #[test]
    fn reading_allows_asking_whether_there_is_anything_to_read() {
        let reading = Rule::new("GET", "/repos/**");
        assert!(reading.covers("HEAD", "/repos/acme"));
        assert!(!reading.covers("POST", "/repos/acme"));
    }

    #[test]
    fn a_malformed_rule_grants_nothing() {
        assert!(Rule::read(&json!({ "method": "GET" })).is_none());
        assert!(Rule::read(&json!({ "method": "SUDO", "path": "/**" })).is_none());
        assert!(Rule::read(&json!({ "path": "  " })).is_none());
        assert_eq!(Rule::all(&json!([{ "path": "/a" }, { "path": "" }])).len(), 1);
    }

    #[test]
    fn minting_a_credential_is_denied_wherever_it_is_asked_for() {
        let denials = built_in();
        let stopped =
            |method: &str, path: &str| denials.iter().any(|rule| rule.covers(method, path));
        assert!(stopped("POST", "/user/keys"));
        assert!(stopped("POST", "/artifactory/api/security/token"), "a token endpoint by any name");
        assert!(stopped("POST", "/repos/acme/payments-api/keys"));
        assert!(stopped("PUT", "/orgs/acme/actions/secrets/deploy"));
        assert!(!stopped("GET", "/user/keys"), "listing them is not minting one");
        assert!(!stopped("POST", "/repos/acme/payments-api/pulls"));
    }
}
