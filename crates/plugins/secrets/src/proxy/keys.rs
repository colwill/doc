//! DOC keys: what a caller sends to reach a proxied account (ADR-0014 §3). A key is DOC's own —
//! it means nothing at the vendor and is the same shape whichever vendor is behind it.
//!
//! The plugin mints them, so it can check one without asking core, which is what keeps core off
//! the hot path (§9). Only the digest is stored: a key is shown once when it is made, and after
//! that nobody — not an administrator, not whoever reads the database — can get the value back.

use doc_plugin_sdk::protocol::Secret as Hidden;
use ring::digest;
use ring::rand::{SecureRandom, SystemRandom};

use crate::Refusal;

/// What every key starts with, so one found in a log or a shell is recognisable as DOC's and not
/// mistaken for the vendor's.
pub const PREFIX: &str = "doc_";

/// The random part, in bytes before encoding.
const BYTES: usize = 32;

/// How much of a key a page may show: enough to tell two apart, not enough to use.
const SHOWN: usize = PREFIX.len() + 8;

/// A key as it is made: the value, which is shown once and never again, and the digest and
/// fragment that are all DOC keeps of it.
pub struct Minted {
    pub value: Hidden<String>,
    pub digest: String,
    pub shown: String,
}

/// A new key. The randomness comes from the system, and a failure to get any is a refusal rather
/// than a weaker key.
pub fn mint() -> Result<Minted, Refusal> {
    let mut bytes = [0u8; BYTES];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| Refusal::unavailable("this machine would not give any randomness"))?;
    let value = format!("{PREFIX}{}", hex::encode(bytes));
    Ok(Minted {
        digest: digest_of(&value),
        shown: value.chars().take(SHOWN).collect::<String>() + "…",
        value: Hidden::new(value),
    })
}

/// The digest a key is looked up by. SHA-256 of the whole key, so what is stored is useless to
/// anybody who reads it and the lookup is still one hop.
pub fn digest_of(key: &str) -> String {
    hex::encode(digest::digest(&digest::SHA256, key.as_bytes()))
}

/// The key out of an `Authorization` header, or nothing. Bearer only: a proxy that accepted a
/// key in a query parameter would put it in every intermediary's access log.
pub fn presented(header: Option<&str>) -> Option<String> {
    let header = header?.trim();
    let (kind, value) = header.split_once(' ')?;
    if !kind.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let value = value.trim();
    match value.starts_with(PREFIX) && value.len() > SHOWN {
        true => Some(value.to_string()),
        false => None,
    }
}

#[cfg(test)]
mod minting {
    use super::*;

    #[test]
    fn a_key_says_it_is_docs_and_is_never_stored() {
        let key = mint().expect("randomness");
        let value = key.value.expose().to_string();
        assert!(value.starts_with(PREFIX));
        assert_eq!(key.digest, digest_of(&value));
        assert!(!key.digest.contains(&value), "the digest cannot contain the key");
        assert!(value.starts_with(key.shown.trim_end_matches('…')));
        assert!(key.shown.len() < value.len(), "a page shows a fragment, not the key");
    }

    #[test]
    fn two_keys_are_not_the_same_key() {
        let (one, two) = (mint().expect("randomness"), mint().expect("randomness"));
        assert_ne!(one.digest, two.digest);
    }

    #[test]
    fn only_a_bearer_key_of_docs_own_shape_is_read() {
        let key = mint().expect("randomness");
        let value = key.value.expose().to_string();
        assert_eq!(presented(Some(&format!("Bearer {value}"))).as_deref(), Some(value.as_str()));
        assert_eq!(presented(Some(&format!("bearer {value}"))).as_deref(), Some(value.as_str()));
        assert!(presented(Some(&format!("Basic {value}"))).is_none());
        assert!(presented(Some("Bearer ghp_averyrealgithubtoken")).is_none());
        assert!(presented(Some("Bearer doc_short")).is_none());
        assert!(presented(None).is_none());
    }
}
