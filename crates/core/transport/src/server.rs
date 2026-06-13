//! HTTP/3 server loop: one task per connection, one per request, so a panic fails one call.

use std::future::Future;
use std::net::SocketAddr;

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use http::{Request, Response, StatusCode};
use serde::Serialize;
use tokio_quiche::ServerH3Driver;
use tokio_quiche::http3::driver::{
    H3Event, InboundFrameStream, IncomingH3Headers, OutboundFrame, OutboundFrameSender,
    ServerH3Event,
};
use tokio_quiche::http3::settings::Http3Settings;

use crate::endpoint::Endpoint;
use crate::frames;

pub struct H3ServerSend {
    send: OutboundFrameSender,
}

pub struct H3ServerRecv {
    recv: InboundFrameStream,
}

/// One request: the body arrives on `recv`, the response goes out on `send`.
pub struct H3ServerStream {
    send: H3ServerSend,
    recv: H3ServerRecv,
}

impl H3ServerSend {
    pub async fn send_response(&mut self, response: Response<()>) -> Result<()> {
        let headers = frames::response_headers(&response);
        frames::send(&mut self.send, OutboundFrame::Headers(headers, None)).await
    }

    pub async fn send_data(&mut self, data: Bytes) -> Result<()> {
        frames::send(&mut self.send, OutboundFrame::Body(data, false)).await
    }

    pub async fn finish(&mut self) -> Result<()> {
        frames::send(&mut self.send, OutboundFrame::Body(Bytes::new(), true)).await
    }
}

impl H3ServerRecv {
    pub async fn recv_data(&mut self) -> Result<Option<Bytes>> {
        frames::recv(&mut self.recv).await
    }
}

impl H3ServerStream {
    pub fn split(self) -> (H3ServerSend, H3ServerRecv) {
        (self.send, self.recv)
    }

    pub async fn send_response(&mut self, response: Response<()>) -> Result<()> {
        self.send.send_response(response).await
    }

    pub async fn send_data(&mut self, data: Bytes) -> Result<()> {
        self.send.send_data(data).await
    }

    pub async fn finish(&mut self) -> Result<()> {
        self.send.finish().await
    }

    pub async fn recv_data(&mut self) -> Result<Option<Bytes>> {
        self.recv.recv_data().await
    }
}

pub async fn read_body(stream: &mut H3ServerStream) -> Result<Bytes> {
    let mut out = BytesMut::new();
    while let Some(chunk) = stream.recv_data().await? {
        out.extend_from_slice(&chunk);
    }
    Ok(out.freeze())
}

pub async fn respond(stream: &mut H3ServerStream, status: StatusCode, body: Bytes) -> Result<()> {
    let mut builder = Response::builder().status(status);
    if !body.is_empty() {
        builder = builder.header("content-type", "application/json");
    }
    stream.send_response(builder.body(())?).await?;
    if !body.is_empty() {
        stream.send_data(body).await?;
    }
    stream.finish().await?;
    Ok(())
}

pub async fn respond_json<T: Serialize>(
    stream: &mut H3ServerStream,
    status: StatusCode,
    body: &T,
) -> Result<()> {
    respond(stream, status, Bytes::from(serde_json::to_vec(body)?)).await
}

pub fn bearer(request: &Request<()>) -> Option<&str> {
    request.headers().get(http::header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

/// Constant-time comparison, so a wrong token cannot be guessed from timing.
pub fn token_matches(presented: Option<&str>, expected: &str) -> bool {
    use subtle::ConstantTimeEq;
    presented.is_some_and(|token| token.as_bytes().ct_eq(expected.as_bytes()).into())
}

/// Accepts QUIC connections and dispatches each HTTP/3 request to `handler` on its own task.
pub async fn serve<H, F>(endpoint: Endpoint, handler: H)
where
    H: Fn(Request<()>, H3ServerStream, SocketAddr) -> F + Clone + Send + Sync + 'static,
    F: Future<Output = Result<()>> + Send + 'static,
{
    let Some(mut incoming) = endpoint.take_incoming().await else {
        tracing::error!("serve was called on an endpoint with no listener");
        return;
    };
    while let Some(connection) = incoming.recv().await {
        let connection = match connection {
            Ok(connection) => connection,
            Err(err) => {
                tracing::warn!(%err, "incoming QUIC connection failed");
                continue;
            }
        };
        let handler = handler.clone();
        tokio::spawn(async move {
            let (driver, mut controller) = ServerH3Driver::new(Http3Settings::default());
            let connection = connection.start(driver);
            let remote = connection.peer_addr();
            while let Some(event) = controller.event_receiver_mut().recv().await {
                let incoming_headers = match event {
                    ServerH3Event::Headers { incoming_headers, .. } => incoming_headers,
                    ServerH3Event::Core(
                        H3Event::ConnectionShutdown(_) | H3Event::ConnectionError(_),
                    ) => break,
                    ServerH3Event::Core(_) => continue,
                };
                let IncomingH3Headers { headers, send, recv, .. } = incoming_headers;
                let request = match frames::to_request(&headers) {
                    Ok(request) => request,
                    Err(err) => {
                        tracing::debug!(%err, %remote, "malformed request headers");
                        continue;
                    }
                };
                let stream =
                    H3ServerStream { send: H3ServerSend { send }, recv: H3ServerRecv { recv } };
                let handler = handler.clone();
                tokio::spawn(async move {
                    if let Err(err) = handler(request, stream, remote).await {
                        tracing::debug!(%err, "request handler failed");
                    }
                });
            }
        });
    }
}
