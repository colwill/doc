//! `Secret<T>` holds a token, password or credential. It has no `Display` or `Serialize`, and its
//! `Debug` is redacted, so it cannot reach a log line or a response by accident. Reading it takes
//! `expose`, and sending it on purpose takes `#[serde(serialize_with = "doc_secret::exposed")]`.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Default)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &T {
        &self.0
    }
}

impl<T: AsRef<[u8]>> Secret<T> {
    /// Compares in constant time, so how much of a guess was right cannot be timed.
    pub fn matches(&self, presented: impl AsRef<[u8]>) -> bool {
        use subtle::ConstantTimeEq;
        self.0.as_ref().ct_eq(presented.as_ref()).into()
    }

    pub fn is_empty(&self) -> bool {
        self.0.as_ref().is_empty()
    }
}

impl<T: AsRef<[u8]>> PartialEq for Secret<T> {
    fn eq(&self, other: &Self) -> bool {
        self.matches(other.0.as_ref())
    }
}

impl<T: AsRef<[u8]>> Eq for Secret<T> {}

impl<T> From<T> for Secret<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(redacted)")
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Secret<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self)
    }
}

/// Serialises the secret itself, for a message whose whole point is to carry it.
pub fn exposed<T: Serialize, S: Serializer>(
    secret: &Secret<T>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    secret.0.serialize(serializer)
}

/// `exposed` for an optional secret.
pub fn exposed_option<T: Serialize, S: Serializer>(
    secret: &Option<Secret<T>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    secret.as_ref().map(|secret| &secret.0).serialize(serializer)
}
