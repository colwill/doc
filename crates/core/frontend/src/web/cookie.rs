//! The only way the frontend sets a cookie. Every one is host-only, `HttpOnly`, `Secure` and
//! `SameSite=Lax` with `Path=/`, and a call site can choose only its name, value and lifetime.

use http::HeaderValue;

pub struct Cookie {
    name: &'static str,
    value: String,
    max_age: Option<i64>,
}

impl Cookie {
    pub fn new(name: &'static str, value: impl Into<String>) -> Self {
        Self { name, value: value.into(), max_age: None }
    }

    pub fn lasting(mut self, seconds: i64) -> Self {
        self.max_age = Some(seconds.max(0));
        self
    }

    /// Tells the browser to drop the cookie now.
    pub fn cleared(name: &'static str) -> Self {
        Self::new(name, "").lasting(0)
    }

    /// The `Set-Cookie` value, or the cleared cookie when the value cannot be a header.
    pub fn header(&self) -> HeaderValue {
        let mut cookie =
            format!("{}={}; Path=/; HttpOnly; Secure; SameSite=Lax", self.name, self.value);
        if let Some(seconds) = self.max_age {
            cookie.push_str(&format!("; Max-Age={seconds}"));
        }
        HeaderValue::from_str(&cookie).unwrap_or_else(|_| Self::cleared(self.name).header())
    }
}
