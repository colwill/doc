//! Browser reload for `just dev`. The page polls for this process's boot id; a rebuild gives the
//! next process a new one, so the restart itself is the signal and nothing has to watch files.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::response::{IntoResponse, Response};
use http::header::{CACHE_CONTROL, CONTENT_TYPE};

static ENABLED: OnceLock<bool> = OnceLock::new();
static BOOT: OnceLock<String> = OnceLock::new();

pub fn set_enabled(enabled: bool) {
    let _ = ENABLED.set(enabled);
}

pub fn enabled() -> bool {
    *ENABLED.get().unwrap_or(&false)
}

fn boot() -> &'static str {
    BOOT.get_or_init(|| {
        let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        format!("{}", since.as_nanos())
    })
}

pub async fn boot_id() -> Response {
    ([(CONTENT_TYPE, "text/plain; charset=utf-8"), (CACHE_CONTROL, "no-store")], boot())
        .into_response()
}
