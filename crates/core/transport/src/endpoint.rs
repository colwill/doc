//! The bound UDP socket a service accepts on, plus the credentials it dials out with. quiche
//! gives every client connection its own socket, so dialling needs only the secrets directory.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio_quiche::metrics::DefaultMetrics;
use tokio_quiche::settings::{ConnectionParams, Hooks};

use crate::tls::{self, EndpointConfig, SecretsHook, SocketReport};

pub(crate) type Incoming = tokio::sync::mpsc::Receiver<
    std::io::Result<tokio_quiche::InitialQuicConnection<UdpSocket, DefaultMetrics>>,
>;

struct Inner {
    secrets: PathBuf,
    socket_buffer: Option<usize>,
    incoming: Mutex<Option<Incoming>>,
}

/// A service's QUIC endpoint: `serve` drains the inbound side, clients clone it to dial out.
#[derive(Clone)]
pub struct Endpoint(Arc<Inner>);

impl Endpoint {
    pub fn secrets(&self) -> &Path {
        &self.0.secrets
    }

    pub(crate) fn socket_buffer(&self) -> Option<usize> {
        self.0.socket_buffer
    }

    pub(crate) async fn take_incoming(&self) -> Option<Incoming> {
        self.0.incoming.lock().await.take()
    }

    /// Stops accepting: tokio-quiche shuts the listener down once its stream is dropped.
    pub async fn close(&self) {
        self.0.incoming.lock().await.take();
    }
}

fn bind(bind: SocketAddr, buffer: Option<usize>) -> Result<(std::net::UdpSocket, SocketReport)> {
    let socket = Socket::new(Domain::for_address(bind), Type::DGRAM, Some(Protocol::UDP))?;
    if let Some(bytes) = buffer {
        let _ = socket.set_recv_buffer_size(bytes);
        let _ = socket.set_send_buffer_size(bytes);
    }
    socket.set_nonblocking(true)?;
    socket.bind(&bind.into()).with_context(|| format!("binding UDP {bind}"))?;
    let report = SocketReport {
        recv_buffer: socket.recv_buffer_size()?,
        send_buffer: socket.send_buffer_size()?,
    };
    Ok((socket.into(), report))
}

pub(crate) fn dialling_socket(peer: SocketAddr, buffer: Option<usize>) -> Result<UdpSocket> {
    let local: SocketAddr = match peer {
        SocketAddr::V4(_) => "0.0.0.0:0".parse().expect("valid address"),
        SocketAddr::V6(_) => "[::]:0".parse().expect("valid address"),
    };
    let (socket, _) = bind(local, buffer)?;
    socket.connect(peer).with_context(|| format!("connecting UDP to {peer}"))?;
    Ok(UdpSocket::from_std(socket)?)
}

pub fn endpoint(config: &EndpointConfig) -> Result<(Endpoint, SocketReport)> {
    let (socket, report) = bind(config.bind, config.socket_buffer)?;
    let incoming = match &config.server_name {
        Some(name) => {
            let certificate = tls::certificate_path(&config.secrets, name);
            let certificate = certificate.to_string_lossy().into_owned();
            let params = ConnectionParams::new_server(
                tls::quic_settings(false),
                tls::certificate_paths(&certificate),
                Hooks { connection_hook: Some(SecretsHook::serving(&config.secrets, name)) },
            );
            let socket = UdpSocket::from_std(socket)?;
            let mut listeners = tokio_quiche::listen([socket], params, DefaultMetrics)
                .with_context(|| format!("listening for QUIC on {}", config.bind))?;
            Some(listeners.remove(0).into_inner())
        }
        None => None,
    };
    let inner = Inner {
        secrets: config.secrets.clone(),
        socket_buffer: config.socket_buffer,
        incoming: Mutex::new(incoming),
    };
    Ok((Endpoint(Arc::new(inner)), report))
}
