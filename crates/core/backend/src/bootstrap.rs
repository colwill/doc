//! Fills the shared secrets volume: a certificate authority, one certificate per service, bus node
//! and plugin, a registration token per plugin, and the operator token. Re-running keeps what is
//! there, creates what is missing and renews a certificate within `RENEW_WITHIN_DAYS` of expiring.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::secrets::{self, TokenKind};

const INVENTORY: &str = "bootstrap.json";
/// The first sign-in: a username and a one-time password for DOC accounts, the `local` plugin,
/// which makes that account while it has none.
pub const FIRST_SIGN_IN: &str = "first-sign-in";
/// Each bus cluster authenticates its Raft and client calls with its own token (ADR-0002).
pub const BUSES: [&str; 3] = ["eventbus", "servicebus", "cachebus"];
const CA_YEARS: i32 = 10;
const LEAF_YEARS: i32 = 2;
pub const RENEW_WITHIN_DAYS: i64 = 30;

#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Inventory {
    pub version: u32,
    pub created_at: String,
    pub certificates: Vec<String>,
    pub plugins: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Report {
    pub created: Vec<String>,
    pub kept: Vec<String>,
}

impl Report {
    fn note(&mut self, path: &Path, created: bool) {
        let name = path.display().to_string();
        if created {
            self.created.push(name);
        } else {
            self.kept.push(name);
        }
    }
}

pub fn certificate_names(config: &Config) -> Vec<String> {
    let mut names = vec!["backend".to_string(), "frontend".to_string(), "workers".to_string()];
    names.extend(config.fabric.nodes().cloned());
    names.extend(config.plugins.ids.iter().map(|id| format!("plugin-{id}")));
    names
}

pub fn run(config: &Config, dir: &Path) -> Result<Report> {
    let mut report = Report::default();
    for sub in
        ["ca", "certs", "tokens", "tokens/plugins", "tokens/buses", secrets::SETTINGS_KEY_DIR]
    {
        fs::create_dir_all(dir.join(sub))
            .with_context(|| format!("creating {}", dir.join(sub).display()))?;
    }

    let issuer = certificate_authority(dir, &mut report)?;
    for name in certificate_names(config) {
        leaf_certificate(dir, &name, &issuer, &mut report)?;
    }

    let operator = dir.join("tokens/operator.token");
    write_token_if_missing(&operator, TokenKind::Operator, &mut report)?;
    let frontend = dir.join("tokens/frontend.token");
    write_token_if_missing(&frontend, TokenKind::Service, &mut report)?;
    for id in &config.plugins.ids {
        let path = dir.join(format!("tokens/plugins/{id}.token"));
        write_token_if_missing(&path, TokenKind::PluginRegistration, &mut report)?;
    }
    for bus in BUSES {
        let path = dir.join(format!("tokens/buses/{bus}.token"));
        write_token_if_missing(&path, TokenKind::Bus, &mut report)?;
    }
    write_settings_key(dir, &mut report)?;
    write_first_sign_in(config, dir, &mut report)?;

    write_inventory(config, dir, &mut report)?;
    secrets::own_tree(dir, config.secrets.owner_uid, config.secrets.owner_gid)?;
    Ok(report)
}

fn certificate_authority(dir: &Path, report: &mut Report) -> Result<Issuer<'static, KeyPair>> {
    let cert_path = dir.join("ca/ca.pem");
    let key_path = dir.join("ca/ca.key");
    if cert_path.exists() && key_path.exists() {
        let key = KeyPair::from_pem(&fs::read_to_string(&key_path)?)?;
        let issuer = Issuer::from_ca_cert_pem(&fs::read_to_string(&cert_path)?, key)?;
        report.note(&cert_path, false);
        return Ok(issuer);
    }

    let key = KeyPair::generate()?;
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, "DOC Bootstrap CA");
    params.distinguished_name.push(DnType::OrganizationName, "DOC");
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    set_validity(&mut params, CA_YEARS);
    let cert = params.self_signed(&key)?;

    secrets::write(&cert_path, cert.pem().as_bytes(), 0o644)?;
    secrets::write(&key_path, key.serialize_pem().as_bytes(), 0o600)?;
    report.note(&cert_path, true);
    Ok(Issuer::new(params, key))
}

fn leaf_certificate(
    dir: &Path,
    name: &str,
    issuer: &Issuer<'_, KeyPair>,
    report: &mut Report,
) -> Result<()> {
    let cert_path = dir.join(format!("certs/{name}.pem"));
    let key_path = dir.join(format!("certs/{name}.key"));
    let renew_by = chrono::Utc::now() + chrono::Duration::days(RENEW_WITHIN_DAYS);
    if cert_path.exists() && key_path.exists() && expiry(&cert_path)? > renew_by {
        report.note(&cert_path, false);
        return Ok(());
    }

    let key = KeyPair::generate()?;
    let mut params = CertificateParams::new(vec![name.to_string(), "localhost".to_string()])?;
    params.subject_alt_names.push(SanType::IpAddress("127.0.0.1".parse()?));
    params.distinguished_name.push(DnType::CommonName, name);
    params.distinguished_name.push(DnType::OrganizationName, "DOC");
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages =
        vec![ExtendedKeyUsagePurpose::ServerAuth, ExtendedKeyUsagePurpose::ClientAuth];
    set_validity(&mut params, LEAF_YEARS);
    let cert = params.signed_by(&key, issuer)?;

    secrets::write(&cert_path, cert.pem().as_bytes(), 0o644)?;
    secrets::write(&key_path, key.serialize_pem().as_bytes(), 0o600)?;
    report.note(&cert_path, true);
    Ok(())
}

/// When a PEM certificate stops being valid.
pub fn expiry(path: &Path) -> Result<chrono::DateTime<chrono::Utc>> {
    let pem = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let (_, pem) = x509_parser::pem::parse_x509_pem(&pem)
        .map_err(|err| anyhow::anyhow!("{} is not PEM: {err}", path.display()))?;
    let cert = pem
        .parse_x509()
        .map_err(|err| anyhow::anyhow!("{} is not a certificate: {err}", path.display()))?;
    chrono::DateTime::from_timestamp(cert.validity().not_after.timestamp(), 0)
        .with_context(|| format!("{} expires at an impossible time", path.display()))
}

fn set_validity(params: &mut CertificateParams, years: i32) {
    let now = chrono::Utc::now();
    params.not_before = rcgen::date_time_ymd(now.year_utc(), 1, 1);
    params.not_after = rcgen::date_time_ymd(now.year_utc() + years, 1, 1);
}

trait YearUtc {
    fn year_utc(&self) -> i32;
}

impl YearUtc for chrono::DateTime<chrono::Utc> {
    fn year_utc(&self) -> i32 {
        use chrono::Datelike;
        self.year()
    }
}

fn write_token_if_missing(path: &Path, kind: TokenKind, report: &mut Report) -> Result<()> {
    if path.exists() {
        report.note(path, false);
        return Ok(());
    }
    secrets::write(path, secrets::generate_token(kind)?.expose().as_bytes(), 0o600)?;
    report.note(path, true);
    Ok(())
}

/// The key every stored plugin secret is encrypted under (ADR-0007). It is made once and kept: a
/// new key would leave every secret already stored unreadable, so a rotation is asked for by name.
fn write_settings_key(dir: &Path, report: &mut Report) -> Result<()> {
    let keys = secrets::SettingsKeys::load(dir)?;
    if !keys.is_empty() {
        report.note(&dir.join("keys/settings.key"), false);
        return Ok(());
    }
    secrets::create_settings_key(dir)?;
    report.note(&dir.join("keys/settings.key"), true);
    Ok(())
}

/// Named after the first bootstrap admin, so that whoever signs in with it is an admin.
fn write_first_sign_in(config: &Config, dir: &Path, report: &mut Report) -> Result<()> {
    let path = dir.join(FIRST_SIGN_IN);
    if path.exists() {
        report.note(&path, false);
        return Ok(());
    }
    let username = config.bootstrap.admins.first().map_or("admin", String::as_str);
    let password = secrets::one_time_password()?;
    let text = format!(
        "# The first sign-in to DOC, with a DOC account (the `local` plugin), made while it has\n\
         # none. The password works once: you choose your own straight away.\n\
         username = \"{username}\"\npassword = \"{}\"\n",
        password.expose()
    );
    secrets::write(&path, text.as_bytes(), 0o600)?;
    report.note(&path, true);
    Ok(())
}

fn write_inventory(config: &Config, dir: &Path, report: &mut Report) -> Result<()> {
    let path = dir.join(INVENTORY);
    let existing: Option<Inventory> =
        fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let created_at = existing
        .as_ref()
        .map(|i| i.created_at.clone())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let inventory = Inventory {
        version: 1,
        created_at,
        certificates: certificate_names(config),
        plugins: config.plugins.ids.clone(),
    };
    if existing.as_ref() == Some(&inventory) {
        report.note(&path, false);
        return Ok(());
    }
    secrets::write(&path, serde_json::to_string_pretty(&inventory)?.as_bytes(), 0o644)?;
    report.note(&path, true);
    Ok(())
}

pub fn inventory(dir: &Path) -> Result<Inventory> {
    let path: PathBuf = dir.join(INVENTORY);
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        let mut config = Config::default();
        config.plugins.ids = vec!["rbac".into(), "hello".into()];
        config
    }

    #[test]
    fn fills_the_volume_and_then_changes_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first = run(&test_config(), dir.path()).expect("first run");
        assert!(first.kept.is_empty());
        assert!(first.created.iter().any(|p| p.ends_with("ca/ca.pem")));
        assert!(first.created.iter().any(|p| p.ends_with("certs/plugin-hello.pem")));
        assert!(first.created.iter().any(|p| p.ends_with("tokens/operator.token")));

        let before = fingerprint(dir.path());
        let second = run(&test_config(), dir.path()).expect("second run");
        assert!(second.created.is_empty(), "second run created {:?}", second.created);
        assert_eq!(before, fingerprint(dir.path()));
    }

    #[test]
    fn a_new_plugin_only_adds_its_own_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        run(&test_config(), dir.path()).expect("first run");
        let mut config = test_config();
        config.plugins.ids.push("kb".into());
        let report = run(&config, dir.path()).expect("second run");
        assert!(report.created.iter().any(|p| p.ends_with("certs/plugin-kb.pem")));
        assert!(report.created.iter().any(|p| p.ends_with("tokens/plugins/kb.token")));
        assert_eq!(inventory(dir.path()).expect("inventory").plugins.len(), 3);
    }

    #[test]
    fn tokens_are_unique_and_prefixed() {
        let dir = tempfile::tempdir().expect("temp dir");
        run(&test_config(), dir.path()).expect("run");
        let rbac = fs::read_to_string(dir.path().join("tokens/plugins/rbac.token")).expect("token");
        let hello =
            fs::read_to_string(dir.path().join("tokens/plugins/hello.token")).expect("token");
        let operator = fs::read_to_string(dir.path().join("tokens/operator.token")).expect("token");
        assert_ne!(rbac, hello);
        assert!(rbac.starts_with("doc_reg_"), "{rbac}");
        assert!(operator.starts_with("doc_ops_"), "{operator}");
    }

    fn fingerprint(dir: &Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(path) = stack.pop() {
            for entry in fs::read_dir(&path).expect("read dir") {
                let entry = entry.expect("entry");
                if entry.file_type().expect("file type").is_dir() {
                    stack.push(entry.path());
                } else {
                    let relative =
                        entry.path().strip_prefix(dir).expect("relative").display().to_string();
                    let bytes = fs::read(entry.path()).expect("read file");
                    out.push((relative, secrets::sha256_hex(&bytes)));
                }
            }
        }
        out.sort();
        out
    }
}
