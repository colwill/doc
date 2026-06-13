//! Translation between `http` types and quiche's HTTP/3 header lists, and the frame channels
//! tokio-quiche hands out per stream.

use anyhow::{Result, anyhow};
use bytes::Bytes;
use http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use quiche::h3::{Header, NameValue};
use tokio_quiche::http3::driver::{
    InboundFrame, InboundFrameStream, OutboundFrame, OutboundFrameSender,
};

pub(crate) async fn send(sender: &mut OutboundFrameSender, frame: OutboundFrame) -> Result<()> {
    std::future::poll_fn(|cx| sender.poll_reserve(cx))
        .await
        .map_err(|_| anyhow!("the stream was closed by the peer"))?;
    sender.send_item(frame).map_err(|_| anyhow!("the stream was closed by the peer"))
}

pub(crate) async fn recv(stream: &mut InboundFrameStream) -> Result<Option<Bytes>> {
    loop {
        match stream.recv().await {
            Some(InboundFrame::Body(chunk, fin)) => {
                if !chunk.is_empty() {
                    return Ok(Some(chunk.freeze()));
                }
                if fin {
                    return Ok(None);
                }
            }
            Some(InboundFrame::Datagram(_)) => continue,
            None => return Ok(None),
        }
    }
}

pub(crate) fn request_headers(request: &Request<()>) -> Vec<Header> {
    let authority = request.uri().authority().map(|a| a.as_str()).unwrap_or_default();
    let path = request.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let mut headers = vec![
        Header::new(b":method", request.method().as_str().as_bytes()),
        Header::new(b":scheme", b"https"),
        Header::new(b":authority", authority.as_bytes()),
        Header::new(b":path", path.as_bytes()),
    ];
    headers.extend(
        request
            .headers()
            .iter()
            .map(|(name, value)| Header::new(name.as_str().as_bytes(), value.as_bytes())),
    );
    headers
}

pub(crate) fn response_headers(response: &Response<()>) -> Vec<Header> {
    let mut headers = vec![Header::new(b":status", response.status().as_str().as_bytes())];
    headers.extend(
        response
            .headers()
            .iter()
            .map(|(name, value)| Header::new(name.as_str().as_bytes(), value.as_bytes())),
    );
    headers
}

/// Rebuilds the request, folding `:authority` and `:path` back into an absolute URI.
pub(crate) fn to_request(headers: &[Header]) -> Result<Request<()>> {
    let mut method = None;
    let mut authority = None;
    let mut path = None;
    let mut builder = Request::builder();
    for header in headers {
        match header.name() {
            b":method" => method = Some(std::str::from_utf8(header.value())?.to_owned()),
            b":authority" => authority = Some(std::str::from_utf8(header.value())?.to_owned()),
            b":path" => path = Some(std::str::from_utf8(header.value())?.to_owned()),
            name if name.starts_with(b":") => {}
            name => {
                builder = builder.header(
                    HeaderName::from_bytes(name)?,
                    HeaderValue::from_bytes(header.value())?,
                );
            }
        }
    }
    let path = path.ok_or_else(|| anyhow!("the request has no :path"))?;
    let uri = match authority {
        Some(authority) if !authority.is_empty() => format!("https://{authority}{path}"),
        _ => path,
    };
    builder
        .method(method.ok_or_else(|| anyhow!("the request has no :method"))?.as_str())
        .uri(uri)
        .body(())
        .map_err(Into::into)
}

pub(crate) fn to_response(headers: &[Header]) -> Result<Response<()>> {
    let mut status = None;
    let mut builder = Response::builder();
    for header in headers {
        match header.name() {
            b":status" => status = Some(StatusCode::from_bytes(header.value())?),
            name if name.starts_with(b":") => {}
            name => {
                builder = builder.header(
                    HeaderName::from_bytes(name)?,
                    HeaderValue::from_bytes(header.value())?,
                );
            }
        }
    }
    builder
        .status(status.ok_or_else(|| anyhow!("the response has no :status"))?)
        .body(())
        .map_err(Into::into)
}
