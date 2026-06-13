//! TLS and QUIC settings built from the bootstrap secrets volume (ADR-0001). quiche uses
//! BoringSSL, so the private CA is loaded through a connection hook instead of a rustls config.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use boring::ssl::{SslContextBuilder, SslFiletype, SslMethod, SslVerifyMode};
use tokio_quiche::quic::ConnectionHook;
use tokio_quiche::settings::{CertificateKind, QuicSettings, TlsCertificatePaths};

pub const ALPN: &[u8] = b"h3";

/// quiche brings its own crypto; this only keeps rustls usable for Postgres and outbound HTTPS.
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn ca_path(secrets: &Path) -> PathBuf {
    secrets.join("ca/ca.pem")
}

pub fn certificate_path(secrets: &Path, name: &str) -> PathBuf {
    secrets.join(format!("certs/{name}.pem"))
}

pub fn key_path(secrets: &Path, name: &str) -> PathBuf {
    secrets.join(format!("certs/{name}.key"))
}

enum Role {
    Serve { certificate: PathBuf, key: PathBuf },
    Dial { ca: PathBuf },
}

/// Builds the BoringSSL context: servers present their bootstrap certificate, clients trust only
/// the bootstrap CA, which is what binds each service to the name it was issued.
pub struct SecretsHook(Role);

impl SecretsHook {
    pub fn serving(secrets: &Path, name: &str) -> Arc<Self> {
        Arc::new(Self(Role::Serve {
            certificate: certificate_path(secrets, name),
            key: key_path(secrets, name),
        }))
    }

    pub fn dialling(secrets: &Path) -> Arc<Self> {
        Arc::new(Self(Role::Dial { ca: ca_path(secrets) }))
    }

    fn build(&self) -> Result<SslContextBuilder> {
        let mut builder = SslContextBuilder::new(SslMethod::tls())?;
        match &self.0 {
            Role::Serve { certificate, key } => {
                builder
                    .set_certificate_chain_file(certificate)
                    .with_context(|| format!("reading {}", certificate.display()))?;
                builder
                    .set_private_key_file(key, SslFiletype::PEM)
                    .with_context(|| format!("reading {}", key.display()))?;
                builder.set_verify(SslVerifyMode::NONE);
            }
            Role::Dial { ca } => {
                builder.set_ca_file(ca).with_context(|| format!("reading {}", ca.display()))?;
                builder.set_verify(SslVerifyMode::PEER);
            }
        }
        Ok(builder)
    }
}

impl ConnectionHook for SecretsHook {
    fn create_custom_ssl_context_builder(
        &self,
        _paths: TlsCertificatePaths<'_>,
    ) -> Option<SslContextBuilder> {
        match self.build() {
            Ok(builder) => Some(builder),
            Err(err) => {
                tracing::error!(%err, "building the TLS context failed");
                None
            }
        }
    }
}

/// The hook ignores these, but tokio-quiche only calls it when a certificate is configured.
pub fn certificate_paths(path: &str) -> TlsCertificatePaths<'_> {
    TlsCertificatePaths { cert: path, private_key: path, kind: CertificateKind::X509 }
}

/// QUIC tuning from ADR-0001: 3s idle timeout and wide stream and connection windows. Clients
/// hold idle connections open themselves, since quiche has no keep-alive setting.
pub fn quic_settings(verify_peer: bool) -> QuicSettings {
    let mut settings = QuicSettings::default();
    settings.alpn = vec![ALPN.to_vec()];
    settings.verify_peer = verify_peer;
    settings.max_idle_timeout = Some(Duration::from_secs(3));
    settings.initial_max_data = 64 * 1024 * 1024;
    settings.initial_max_stream_data_bidi_local = 8 * 1024 * 1024;
    settings.initial_max_stream_data_bidi_remote = 8 * 1024 * 1024;
    settings.initial_max_streams_bidi = 4096;
    settings.max_connection_window = 64 * 1024 * 1024;
    settings.max_stream_window = 8 * 1024 * 1024;
    settings
}

#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub bind: SocketAddr,
    pub secrets: PathBuf,
    /// The certificate to serve; `None` makes a client-only endpoint.
    pub server_name: Option<String>,
    pub socket_buffer: Option<usize>,
}

impl EndpointConfig {
    pub fn client(secrets: impl Into<PathBuf>) -> Self {
        Self {
            bind: "0.0.0.0:0".parse().expect("valid address"),
            secrets: secrets.into(),
            server_name: None,
            socket_buffer: None,
        }
    }

    pub fn server(bind: SocketAddr, secrets: impl Into<PathBuf>, name: impl Into<String>) -> Self {
        Self { bind, secrets: secrets.into(), server_name: Some(name.into()), socket_buffer: None }
    }
}

/// Socket buffer sizes the kernel actually granted, which containers cap (ADR-0001).
#[derive(Debug, Clone, Copy)]
pub struct SocketReport {
    pub recv_buffer: usize,
    pub send_buffer: usize,
}
