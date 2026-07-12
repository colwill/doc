//! The provider as OpenID Connect describes it: what its discovery document says, the keys it
//! signs with, and the checks an ID token has to pass before a single claim in it is believed.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

/// Discovery and keys are asked for again after this, so a provider that rotates its keys or
/// moves an endpoint is followed without a restart.
const FRESH_FOR: Duration = Duration::from_secs(10 * 60);
/// A little room for clocks that disagree, which they always do.
const SKEW: i64 = 120;

/// The discovery document as it arrives, before its URLs are known to be URLs.
#[derive(Debug, Clone, Deserialize)]
struct Document {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "Document")]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: Url,
    pub token_endpoint: Url,
    pub jwks_uri: Url,
    pub userinfo_endpoint: Option<Url>,
}

impl TryFrom<Document> for Discovery {
    type Error = String;

    fn try_from(document: Document) -> Result<Self, Self::Error> {
        let url = |name: &str, value: &str| {
            Url::parse(value).map_err(|err| format!("{name} is not a URL: {err}"))
        };
        Ok(Self {
            authorization_endpoint: url(
                "authorization_endpoint",
                &document.authorization_endpoint,
            )?,
            token_endpoint: url("token_endpoint", &document.token_endpoint)?,
            jwks_uri: url("jwks_uri", &document.jwks_uri)?,
            userinfo_endpoint: document
                .userinfo_endpoint
                .as_deref()
                .and_then(|value| Url::parse(value).ok()),
            issuer: document.issuer,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Key {
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    alg: Option<String>,
    #[serde(default)]
    kty: String,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Keys {
    #[serde(default)]
    keys: Vec<Key>,
}

/// What the ID token said about the person, once it has been believed.
#[derive(Debug, Clone, Default)]
pub struct Claims {
    /// The provider's own identifier for them, which never changes: `sub`.
    pub subject: String,
    pub login: Option<String>,
    pub name: Option<String>,
    pub email: Option<String>,
    pub first_name: Option<String>,
    pub surname: Option<String>,
}

impl Claims {
    fn text(values: &BTreeMap<String, Value>, name: &str) -> Option<String> {
        values.get(name).and_then(Value::as_str).map(str::to_string).filter(|s| !s.is_empty())
    }

    fn read(values: &BTreeMap<String, Value>) -> Self {
        let email = Self::text(values, "email");
        Self {
            subject: Self::text(values, "sub").unwrap_or_default(),
            // What to call them: what the provider calls them, else the local part of their email.
            login: Self::text(values, "preferred_username").or_else(|| {
                email.as_deref().and_then(|email| email.split('@').next()).map(String::from)
            }),
            name: Self::text(values, "name"),
            email,
            first_name: Self::text(values, "given_name"),
            surname: Self::text(values, "family_name"),
        }
    }
}

#[derive(Debug)]
pub struct Failure(pub String);

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn failed(what: impl Into<String>) -> Failure {
    Failure(what.into())
}

/// Discovery and keys, kept for a while rather than asked for on every sign-in.
#[derive(Default)]
struct Cached {
    discovery: Option<(Instant, Discovery)>,
    keys: Option<(Instant, Vec<Key>)>,
}

pub struct Provider {
    http: reqwest::Client,
    cached: Mutex<Cached>,
}

impl Default for Provider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("doc-oidc")
            .build()
            .unwrap_or_default();
        Self { http, cached: Mutex::new(Cached::default()) }
    }

    /// What the provider publishes about itself. The issuer in the document must be the issuer we
    /// asked, or the document belongs to somebody else.
    pub async fn discovery(&self, issuer: &Url) -> Result<Discovery, Failure> {
        if let Some((at, found)) = self.cached.lock().ok().and_then(|c| c.discovery.clone())
            && at.elapsed() < FRESH_FOR
        {
            return Ok(found);
        }
        let url = issuer
            .join(".well-known/openid-configuration")
            .map_err(|err| failed(format!("the issuer is not a URL to join onto: {err}")))?;
        let answer = self
            .http
            .get(url.clone())
            .send()
            .await
            .map_err(|err| failed(format!("{url} could not be reached: {err}")))?;
        if !answer.status().is_success() {
            return Err(failed(format!("{url} answered {}", answer.status())));
        }
        let discovery: Discovery = answer
            .json()
            .await
            .map_err(|err| failed(format!("{url} is not a discovery document: {err}")))?;
        let named = format!("{}/", discovery.issuer.trim_end_matches('/'));
        if named != issuer.as_str() {
            return Err(failed(format!("{url} says it is for {}, not {issuer}", discovery.issuer)));
        }
        if let Ok(mut cached) = self.cached.lock() {
            cached.discovery = Some((Instant::now(), discovery.clone()));
        }
        Ok(discovery)
    }

    async fn keys(&self, discovery: &Discovery) -> Result<Vec<Key>, Failure> {
        if let Some((at, found)) = self.cached.lock().ok().and_then(|c| c.keys.clone())
            && at.elapsed() < FRESH_FOR
        {
            return Ok(found);
        }
        let url = discovery.jwks_uri.clone();
        let answer = self
            .http
            .get(url.clone())
            .send()
            .await
            .map_err(|err| failed(format!("{url} could not be reached: {err}")))?;
        let keys: Keys =
            answer.json().await.map_err(|err| failed(format!("{url} is not a key set: {err}")))?;
        if let Ok(mut cached) = self.cached.lock() {
            cached.keys = Some((Instant::now(), keys.keys.clone()));
        }
        Ok(keys.keys)
    }

    /// Swaps the code for tokens. The client authenticates itself with its secret, so nobody who
    /// merely intercepts a code can use it.
    pub async fn exchange(
        &self,
        discovery: &Discovery,
        client_id: &str,
        client_secret: &str,
        code: &str,
        redirect: &Url,
    ) -> Result<Value, Failure> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect.as_str()),
            ("client_id", client_id),
        ];
        let request = self.http.post(discovery.token_endpoint.clone()).form(&form);
        let request = match client_secret.is_empty() {
            true => request,
            false => request.basic_auth(client_id, Some(client_secret)),
        };
        let answer = request
            .send()
            .await
            .map_err(|err| failed(format!("the token endpoint could not be reached: {err}")))?;
        let status = answer.status();
        let body: Value = answer
            .json()
            .await
            .map_err(|err| failed(format!("the token endpoint did not answer with JSON: {err}")))?;
        if !status.is_success() {
            let detail = body["error_description"]
                .as_str()
                .or_else(|| body["error"].as_str())
                .unwrap_or("no reason given");
            return Err(failed(format!("the provider refused the code: {detail}")));
        }
        Ok(body)
    }

    /// The claims of an ID token that is this client's, from this issuer, signed by a key the
    /// provider publishes, unexpired, and carrying the nonce this sign-in started with.
    pub async fn claims(
        &self,
        discovery: &Discovery,
        token: &str,
        client_id: &str,
        nonce: &str,
    ) -> Result<Claims, Failure> {
        let parts: Vec<&str> = token.split('.').collect();
        let [head, payload, signature] = parts.as_slice() else {
            return Err(failed("an ID token has three parts"));
        };
        let decode = |part: &str| {
            URL_SAFE_NO_PAD
                .decode(part)
                .map_err(|err| failed(format!("an ID token is not base64url: {err}")))
        };
        let header: BTreeMap<String, Value> = serde_json::from_slice(&decode(head)?)
            .map_err(|err| failed(format!("an ID token's header is not JSON: {err}")))?;
        let values: BTreeMap<String, Value> = serde_json::from_slice(&decode(payload)?)
            .map_err(|err| failed(format!("an ID token's claims are not JSON: {err}")))?;
        let signed = format!("{head}.{payload}");
        self.verify(discovery, &header, signed.as_bytes(), &decode(signature)?).await?;

        let text = |name: &str| values.get(name).and_then(Value::as_str).unwrap_or_default();
        if format!("{}/", text("iss").trim_end_matches('/'))
            != format!("{}/", discovery.issuer.trim_end_matches('/'))
        {
            return Err(failed("an ID token from another issuer"));
        }
        let audience = match values.get("aud") {
            Some(Value::String(one)) => one == client_id,
            Some(Value::Array(many)) => many.iter().any(|value| value.as_str() == Some(client_id)),
            _ => false,
        };
        if !audience {
            return Err(failed("an ID token meant for another client"));
        }
        let now = chrono::Utc::now().timestamp();
        match values.get("exp").and_then(Value::as_i64) {
            Some(expires) if expires + SKEW > now => {}
            _ => return Err(failed("an ID token that has expired")),
        }
        if let Some(issued) = values.get("iat").and_then(Value::as_i64)
            && issued - SKEW > now
        {
            return Err(failed("an ID token issued in the future"));
        }
        if text("nonce") != nonce {
            return Err(failed("an ID token from a different sign-in"));
        }
        let claims = Claims::read(&values);
        match claims.subject.is_empty() {
            true => Err(failed("an ID token naming nobody")),
            false => Ok(claims),
        }
    }

    /// Some providers put little more than `sub` in the ID token and keep the rest at `userinfo`.
    /// Asked only when something is missing, and only ever to fill a gap, never to overrule a
    /// claim that was signed.
    pub async fn fill_in(
        &self,
        discovery: &Discovery,
        access_token: Option<&str>,
        claims: &mut Claims,
    ) {
        let wanted = claims.name.is_none() || claims.email.is_none() || claims.login.is_none();
        let (Some(endpoint), Some(token), true) =
            (discovery.userinfo_endpoint.clone(), access_token, wanted)
        else {
            return;
        };
        let answered = self.http.get(endpoint).bearer_auth(token).send().await;
        let values: BTreeMap<String, Value> = match answered {
            Ok(answer) if answer.status().is_success() => answer.json().await.unwrap_or_default(),
            Ok(answer) => {
                tracing::debug!(status = %answer.status(), "userinfo was not read");
                return;
            }
            Err(err) => {
                tracing::debug!(%err, "userinfo could not be reached");
                return;
            }
        };
        let found = Claims::read(&values);
        claims.login = claims.login.take().or(found.login);
        claims.name = claims.name.take().or(found.name);
        claims.email = claims.email.take().or(found.email);
        claims.first_name = claims.first_name.take().or(found.first_name);
        claims.surname = claims.surname.take().or(found.surname);
    }

    async fn verify(
        &self,
        discovery: &Discovery,
        header: &BTreeMap<String, Value>,
        signed: &[u8],
        signature: &[u8],
    ) -> Result<(), Failure> {
        let algorithm = header.get("alg").and_then(Value::as_str).unwrap_or_default();
        let scheme = match algorithm {
            "RS256" => &signature::RSA_PKCS1_2048_8192_SHA256,
            "RS384" => &signature::RSA_PKCS1_2048_8192_SHA384,
            "RS512" => &signature::RSA_PKCS1_2048_8192_SHA512,
            other => {
                return Err(failed(format!("ID tokens signed with {other} are not understood")));
            }
        };
        let kid = header.get("kid").and_then(Value::as_str);
        let keys = self.keys(discovery).await?;
        let usable = keys.iter().filter(|key| {
            key.kty == "RSA"
                && key.alg.as_deref().is_none_or(|alg| alg == algorithm)
                && (kid.is_none() || key.kid.is_none() || key.kid.as_deref() == kid)
        });
        let decode = |part: &str| URL_SAFE_NO_PAD.decode(part).unwrap_or_default();
        for key in usable {
            let (Some(n), Some(e)) = (key.n.as_deref(), key.e.as_deref()) else { continue };
            let components = signature::RsaPublicKeyComponents { n: decode(n), e: decode(e) };
            if components.verify(scheme, signed, signature).is_ok() {
                return Ok(());
            }
        }
        Err(failed("an ID token signed by a key the provider does not publish"))
    }
}
