//! Newline-delimited JSON over an HTTP/3 stream, used by long-lived subscriptions.

use anyhow::Result;
use bytes::{Buf, Bytes, BytesMut};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub fn encode<T: Serialize>(value: &T) -> Result<Bytes> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    Ok(Bytes::from(bytes))
}

/// Buffers stream chunks and hands back one decoded message per line.
#[derive(Default)]
pub struct Lines {
    buffer: BytesMut,
}

impl Lines {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, mut chunk: impl Buf) {
        self.buffer.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
    }

    pub fn next_line(&mut self) -> Option<Bytes> {
        let position = self.buffer.iter().position(|byte| *byte == b'\n')?;
        let line = self.buffer.split_to(position + 1);
        Some(Bytes::from(line[..position].to_vec()))
    }

    pub fn next_message<T: DeserializeOwned>(&mut self) -> Option<Result<T>> {
        let line = self.next_line()?;
        if line.is_empty() {
            return None;
        }
        Some(serde_json::from_slice(&line).map_err(Into::into))
    }
}
