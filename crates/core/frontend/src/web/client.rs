//! Who the browser is, passed to the backend as `x-forwarded-for`, so the backend's limits count
//! each browser on its own rather than everyone the frontend serves as one client.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request};
use axum::middleware::Next;
use axum::response::Response;

tokio::task_local! {
    static FORWARDED: String;
}

/// Runs the request with its forwarding chain: any a proxy in front of the frontend sent, then the peer.
pub async fn forwarded(request: Request, next: Next) -> Response {
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>().map(|info| info.0.ip());
    let Some(peer) = peer else { return next.run(request).await };
    let earlier = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty());
    let chain = match earlier {
        Some(earlier) => format!("{earlier}, {peer}"),
        None => peer.to_string(),
    };
    FORWARDED.scope(chain, next.run(request)).await
}

/// The chain for the request being handled, if there is one.
pub fn chain() -> Option<String> {
    FORWARDED.try_with(String::clone).ok()
}
