//! CSRF: every request that changes something carries a token, in the `x-csrf-token` header HTMX
//! sends or a `csrf` form field. It is an HMAC of a random cookie under a key only this process has.

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use doc_secret::Secret;
use hmac::{Hmac, KeyInit, Mac};
use http::header::{CONTENT_TYPE, SET_COOKIE};
use http::{Method, StatusCode};
use sha2::{Digest, Sha256};

use super::AppState;
use super::pages::Page;
use crate::session::cookie;
use crate::web::cookie::Cookie;

pub const CSRF_COOKIE: &str = "doc_csrf";
pub const CSRF_HEADER: &str = "x-csrf-token";
const MAX_FORM: usize = 64 * 1024;

#[derive(Clone)]
pub struct CsrfKey(Secret<[u8; 32]>);

impl CsrfKey {
    /// Derived from the frontend's own secret so tokens outlive a restart; random when it has none.
    pub fn from_secret(secret: &str) -> Self {
        if secret.is_empty() {
            return Self(Secret::new(random()));
        }
        Self(Secret::new(Sha256::digest(format!("doc-frontend-csrf\0{secret}").as_bytes()).into()))
    }

    fn mac(&self, seed: &str) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(self.0.expose())
            .unwrap_or_else(|_| unreachable!("HMAC takes a key of any length"));
        mac.update(seed.as_bytes());
        mac
    }

    pub fn token(&self, seed: &str) -> String {
        hex::encode(self.mac(seed).finalize().into_bytes())
    }

    fn verify(&self, seed: &str, token: &str) -> bool {
        hex::decode(token).is_ok_and(|bytes| self.mac(seed).verify_slice(&bytes).is_ok())
    }
}

fn random() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    if getrandom::fill(&mut bytes).is_err() {
        bytes = Sha256::digest(format!("{:?}", std::time::SystemTime::now()).as_bytes()).into();
    }
    bytes
}

/// The token this page's forms and HTMX requests must send back.
#[derive(Clone, Debug)]
pub struct Csrf(pub String);

fn refused() -> Response {
    let page = Page::new("Refused", "This form has expired. Reload the page and try again.");
    match page.render_html() {
        Ok(html) => (StatusCode::FORBIDDEN, html).into_response(),
        Err(_) => StatusCode::FORBIDDEN.into_response(),
    }
}

fn form_field(body: &[u8], name: &str) -> Option<String> {
    url::form_urlencoded::parse(body)
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

pub async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let (mut parts, body) = request.into_parts();
    let existing = cookie(&parts.headers, CSRF_COOKIE).filter(|seed| seed.len() == 64);
    let fresh = existing.is_none();
    let seed = existing.unwrap_or_else(|| hex::encode(random()));
    let mut body = body;
    if !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS) {
        let mut presented =
            parts.headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);
        let form = parts
            .headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
        if presented.is_none() && form {
            let Ok(bytes) = to_bytes(body, MAX_FORM).await else { return refused() };
            presented = form_field(&bytes, "csrf");
            body = Body::from(bytes);
        }
        if fresh || !presented.is_some_and(|token| state.csrf.verify(&seed, &token)) {
            tracing::info!(method = %parts.method, path = %parts.uri.path(), "refused a request without a valid CSRF token");
            return refused();
        }
    }
    parts.extensions.insert(Csrf(state.csrf.token(&seed)));
    let mut response = next.run(Request::from_parts(parts, body)).await;
    if fresh {
        response.headers_mut().append(SET_COOKIE, Cookie::new(CSRF_COOKIE, seed).header());
    }
    response
}
