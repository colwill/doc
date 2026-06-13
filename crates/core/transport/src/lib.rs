//! Shared QUIC + HTTP/3 transport (ADR-0001): TLS from the secrets volume, endpoints, a pooled
//! client and a server loop, all on Cloudflare quiche. Used by the fabric buses and plugins.

pub mod client;
pub mod endpoint;
mod frames;
pub mod lines;
pub mod server;
pub mod tls;

pub use client::{
    CONNECT_TIMEOUT, H3Client, H3ClientRecv, H3ClientSend, H3ClientStream, TransportError,
};
pub use endpoint::{Endpoint, endpoint};
pub use lines::Lines;
pub use server::{
    H3ServerRecv, H3ServerSend, H3ServerStream, bearer, read_body, respond, respond_json, serve,
    token_matches,
};
pub use tls::{EndpointConfig, SocketReport, install_crypto, quic_settings};
