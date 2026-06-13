//! HTTP/3 client: a small pool of QUIC connections per peer, redialled when they close.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::anyhow;
use bytes::{Bytes, BytesMut};
use http::{Method, Request, Response, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::oneshot;
use tokio_quiche::ClientH3Driver;
use tokio_quiche::QuicConnection;
use tokio_quiche::http3::driver::{
    ClientH3Command, ClientH3Event, ClientRequestSender, H3Event, InboundFrameStream,
    IncomingH3Headers, NewClientRequest, OutboundFrame, OutboundFrameSender, RequestSender,
};
use tokio_quiche::http3::settings::Http3Settings;
use tokio_quiche::quic::{ConnectionShutdownBehaviour, QuicCommand, QuicheConnection};
use tokio_quiche::settings::{ConnectionParams, Hooks};

use crate::endpoint::{self, Endpoint};
use crate::frames;
use crate::tls::{self, SecretsHook};

/// Dialling a peer that is not listening only fails after the idle timeout, so connects are
/// bounded separately (ADR-0001).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
pub const DEFAULT_POOL: usize = 2;
const KEEP_ALIVE: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("{peer} is unreachable: {source}")]
    Unreachable { peer: String, source: anyhow::Error },
    #[error("{peer} did not answer within the deadline")]
    Timeout { peer: String },
    #[error("{peer} returned {status}: {body}")]
    Status { peer: String, status: StatusCode, body: String },
    #[error("malformed response from {peer}: {source}")]
    Malformed { peer: String, source: anyhow::Error },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub struct H3ClientSend {
    send: OutboundFrameSender,
}

/// The response headers arrive as a connection event, so the body only exists once they have.
pub struct H3ClientRecv {
    headers: Option<oneshot::Receiver<IncomingH3Headers>>,
    body: Option<InboundFrameStream>,
}

pub struct H3ClientStream {
    send: H3ClientSend,
    recv: H3ClientRecv,
}

impl H3ClientSend {
    pub async fn send_data(&mut self, data: Bytes) -> anyhow::Result<()> {
        frames::send(&mut self.send, OutboundFrame::Body(data, false)).await
    }

    pub async fn finish(&mut self) -> anyhow::Result<()> {
        frames::send(&mut self.send, OutboundFrame::Body(Bytes::new(), true)).await
    }
}

impl H3ClientRecv {
    pub async fn recv_response(&mut self) -> anyhow::Result<Response<()>> {
        let headers =
            self.headers.take().ok_or_else(|| anyhow!("the response was already read"))?;
        let incoming = headers.await.map_err(|_| anyhow!("the connection closed"))?;
        let response = frames::to_response(&incoming.headers)?;
        self.body = Some(incoming.recv);
        Ok(response)
    }

    pub async fn recv_data(&mut self) -> anyhow::Result<Option<Bytes>> {
        if self.body.is_none() && self.headers.is_some() {
            self.recv_response().await?;
        }
        match self.body.as_mut() {
            Some(body) => frames::recv(body).await,
            None => Ok(None),
        }
    }
}

impl H3ClientStream {
    pub fn split(self) -> (H3ClientSend, H3ClientRecv) {
        (self.send, self.recv)
    }

    pub async fn send_data(&mut self, data: Bytes) -> anyhow::Result<()> {
        self.send.send_data(data).await
    }

    pub async fn finish(&mut self) -> anyhow::Result<()> {
        self.send.finish().await
    }

    pub async fn recv_response(&mut self) -> anyhow::Result<Response<()>> {
        self.recv.recv_response().await
    }

    pub async fn recv_data(&mut self) -> anyhow::Result<Option<Bytes>> {
        self.recv.recv_data().await
    }
}

/// quiche has no keep-alive setting, so an idle connection is held open by asking the connection
/// to send an ack-eliciting packet each second; without it the 3s idle timeout would close it.
fn keep_alive(commands: RequestSender<ClientH3Command, QuicCommand>, alive: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(KEEP_ALIVE);
        ticker.tick().await;
        while alive.load(Ordering::Acquire) {
            ticker.tick().await;
            let nudge = QuicCommand::Custom(Box::new(|connection: &mut QuicheConnection| {
                let _ = connection.send_ack_eliciting();
            }));
            if commands.send(nudge).is_err() {
                return;
            }
        }
    });
}

#[derive(Default)]
struct Pending {
    by_request: HashMap<u64, oneshot::Sender<IncomingH3Headers>>,
    by_stream: HashMap<u64, oneshot::Sender<IncomingH3Headers>>,
}

struct Live {
    requests: ClientRequestSender,
    commands: RequestSender<ClientH3Command, QuicCommand>,
    pending: Arc<Mutex<Pending>>,
    alive: Arc<AtomicBool>,
    next_request: AtomicU64,
    _connection: QuicConnection,
}

/// Dropping a pooled connection has to close it: the driver task owns the UDP socket and keeps
/// running — and the keep-alive keeps nudging it — until the connection itself ends.
impl Drop for Live {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        let _ = self.commands.send(QuicCommand::ConnectionClose(ConnectionShutdownBehaviour {
            send_application_close: true,
            error_code: 0,
            reason: Vec::new(),
        }));
    }
}

struct Slot {
    live: tokio::sync::Mutex<Option<Arc<Live>>>,
}

struct Inner {
    endpoint: Endpoint,
    addr: SocketAddr,
    server_name: String,
    slots: Vec<Slot>,
    next: AtomicUsize,
}

/// One peer, reached over a pool of multiplexed HTTP/3 connections.
#[derive(Clone)]
pub struct H3Client(Arc<Inner>);

impl H3Client {
    pub fn new(endpoint: Endpoint, addr: SocketAddr, server_name: impl Into<String>) -> Self {
        Self::with_pool(endpoint, addr, server_name, DEFAULT_POOL)
    }

    pub fn with_pool(
        endpoint: Endpoint,
        addr: SocketAddr,
        server_name: impl Into<String>,
        pool: usize,
    ) -> Self {
        let slots =
            (0..pool.max(1)).map(|_| Slot { live: tokio::sync::Mutex::new(None) }).collect();
        Self(Arc::new(Inner {
            endpoint,
            addr,
            server_name: server_name.into(),
            slots,
            next: AtomicUsize::new(0),
        }))
    }

    pub fn server_name(&self) -> &str {
        &self.0.server_name
    }

    pub fn addr(&self) -> SocketAddr {
        self.0.addr
    }

    pub fn uri(&self, path: &str) -> String {
        format!("https://{}{}", self.0.server_name, path)
    }

    fn unreachable(&self, source: impl Into<anyhow::Error>) -> TransportError {
        TransportError::Unreachable { peer: self.0.server_name.clone(), source: source.into() }
    }

    async fn dial(&self) -> Result<Arc<Live>, TransportError> {
        let secrets = self.0.endpoint.secrets().to_path_buf();
        let socket = endpoint::dialling_socket(self.0.addr, self.0.endpoint.socket_buffer())
            .map_err(|e| self.unreachable(e))?;
        let socket = socket.try_into().map_err(|e: std::io::Error| self.unreachable(e))?;
        let ca = tls::ca_path(&secrets).to_string_lossy().into_owned();
        let params = ConnectionParams::new_client(
            tls::quic_settings(true),
            Some(tls::certificate_paths(&ca)),
            Hooks { connection_hook: Some(SecretsHook::dialling(&secrets)) },
        );
        let (driver, mut controller) = ClientH3Driver::new(Http3Settings::default());
        let connecting = tokio_quiche::quic::connect_with_config(
            socket,
            Some(&self.0.server_name),
            &params,
            driver,
        );
        let connection = match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
            Ok(Ok(connection)) => connection,
            Ok(Err(err)) => return Err(self.unreachable(anyhow!(err.to_string()))),
            Err(_) => return Err(TransportError::Timeout { peer: self.0.server_name.clone() }),
        };

        let pending = Arc::new(Mutex::new(Pending::default()));
        let alive = Arc::new(AtomicBool::new(true));
        let live = Arc::new(Live {
            requests: controller.request_sender(),
            commands: controller.cmd_sender(),
            pending: pending.clone(),
            alive: alive.clone(),
            next_request: AtomicU64::new(0),
            _connection: connection,
        });
        keep_alive(controller.cmd_sender(), alive.clone());
        tokio::spawn(async move {
            while let Some(event) = controller.event_receiver_mut().recv().await {
                match event {
                    ClientH3Event::NewOutboundRequest { stream_id, request_id } => {
                        let mut pending = pending.lock().expect("the pending map is not poisoned");
                        if let Some(waiting) = pending.by_request.remove(&request_id) {
                            pending.by_stream.insert(stream_id, waiting);
                        }
                    }
                    ClientH3Event::Core(H3Event::IncomingHeaders(headers)) => {
                        let waiting = pending
                            .lock()
                            .expect("the pending map is not poisoned")
                            .by_stream
                            .remove(&headers.stream_id);
                        if let Some(waiting) = waiting {
                            let _ = waiting.send(headers);
                        }
                    }
                    // Dropping the waiter fails the caller now instead of leaving it pending.
                    ClientH3Event::Core(
                        H3Event::ResetStream { stream_id } | H3Event::StreamClosed { stream_id },
                    ) => {
                        pending
                            .lock()
                            .expect("the pending map is not poisoned")
                            .by_stream
                            .remove(&stream_id);
                    }
                    ClientH3Event::Core(
                        H3Event::ConnectionShutdown(_) | H3Event::ConnectionError(_),
                    ) => break,
                    ClientH3Event::Core(_) => {}
                }
            }
            alive.store(false, Ordering::Release);
            pending.lock().expect("the pending map is not poisoned").by_stream.clear();
        });
        Ok(live)
    }

    async fn live(&self) -> Result<Arc<Live>, TransportError> {
        let index = self.0.next.fetch_add(1, Ordering::Relaxed) % self.0.slots.len();
        let mut slot = self.0.slots[index].live.lock().await;
        if let Some(live) = slot.as_ref()
            && live.alive.load(Ordering::Acquire)
        {
            return Ok(live.clone());
        }
        let live = self.dial().await?;
        *slot = Some(live.clone());
        Ok(live)
    }

    /// Opens a request stream, for callers that stream their own bodies.
    pub async fn open(&self, request: Request<()>) -> Result<H3ClientStream, TransportError> {
        let live = self.live().await?;
        let request_id = live.next_request.fetch_add(1, Ordering::Relaxed);
        let (headers_tx, headers_rx) = oneshot::channel();
        let (body_tx, body_rx) = oneshot::channel();
        live.pending
            .lock()
            .expect("the pending map is not poisoned")
            .by_request
            .insert(request_id, headers_tx);
        live.requests
            .send(NewClientRequest {
                request_id,
                headers: frames::request_headers(&request),
                body_writer: Some(body_tx),
            })
            .map_err(|_| self.unreachable(anyhow!("the connection closed")))?;
        let send = body_rx.await.map_err(|_| self.unreachable(anyhow!("the connection closed")))?;
        Ok(H3ClientStream {
            send: H3ClientSend { send },
            recv: H3ClientRecv { headers: Some(headers_rx), body: None },
        })
    }

    pub async fn call(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Bytes,
    ) -> Result<(StatusCode, Bytes), TransportError> {
        let mut builder = Request::builder().method(method).uri(self.uri(path));
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(()).map_err(|e| TransportError::Malformed {
            peer: self.0.server_name.clone(),
            source: e.into(),
        })?;
        let mut stream = self.open(request).await?;
        if !body.is_empty() {
            stream.send_data(body).await.map_err(|e| self.unreachable(e))?;
        }
        stream.finish().await.map_err(|e| self.unreachable(e))?;
        let response = stream.recv_response().await.map_err(|e| self.unreachable(e))?;
        let mut out = BytesMut::new();
        while let Some(chunk) = stream.recv_data().await.map_err(|e| self.unreachable(e))? {
            out.extend_from_slice(&chunk);
        }
        Ok((response.status(), out.freeze()))
    }

    /// POSTs JSON and decodes the JSON answer, mapping non-2xx answers to an error.
    pub async fn post_json<T: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
        deadline: Duration,
    ) -> Result<R, TransportError> {
        let payload = Bytes::from(serde_json::to_vec(body).map_err(|e| {
            TransportError::Malformed { peer: self.0.server_name.clone(), source: e.into() }
        })?);
        let call = self.call(Method::POST, path, &[("content-type", "application/json")], payload);
        let (status, bytes) = match tokio::time::timeout(deadline, call).await {
            Ok(result) => result?,
            Err(_) => return Err(TransportError::Timeout { peer: self.0.server_name.clone() }),
        };
        if !status.is_success() {
            return Err(TransportError::Status {
                peer: self.0.server_name.clone(),
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        serde_json::from_slice(&bytes).map_err(|e| TransportError::Malformed {
            peer: self.0.server_name.clone(),
            source: e.into(),
        })
    }

    /// Drops the pooled connections, so the next call dials again.
    pub async fn reset(&self) {
        for slot in &self.0.slots {
            slot.live.lock().await.take();
        }
    }
}
