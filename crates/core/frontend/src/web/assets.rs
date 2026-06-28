//! Assets are embedded so the runtime image needs no asset directory, and each is served both at
//! a content-hashed path cached forever and at the plain path the nhsuk CSS hard-codes for icons.

use std::collections::HashMap;
use std::sync::LazyLock;

use axum::extract::Path;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use http::header::{CACHE_CONTROL, CONTENT_TYPE};

const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const SVG: &str = "image/svg+xml";
const PNG: &str = "image/png";
const WOFF2: &str = "font/woff2";
const JSON: &str = "application/json";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const REVALIDATE: &str = "public, max-age=3600";
const HASH_LEN: usize = 16;

const EMBEDDED: &[(&str, &str, &[u8])] = &[
    ("doc.css", CSS, include_bytes!("../../assets/doc.css")),
    ("dev-reload.js", JS, include_bytes!("../../assets/dev-reload.js")),
    ("doc-live.js", JS, include_bytes!("../../assets/doc-live.js")),
    ("doc-charts.js", JS, include_bytes!("../../assets/doc-charts.js")),
    ("doc-suggest.js", JS, include_bytes!("../../assets/doc-suggest.js")),
    ("doc-tags.js", JS, include_bytes!("../../assets/doc-tags.js")),
    ("doc-contents.js", JS, include_bytes!("../../assets/doc-contents.js")),
    ("doc-tabs.js", JS, include_bytes!("../../assets/doc-tabs.js")),
    ("doc-reorder.js", JS, include_bytes!("../../assets/doc-reorder.js")),
    ("doc-menu.js", JS, include_bytes!("../../assets/doc-menu.js")),
    ("doc-calendar.js", JS, include_bytes!("../../assets/doc-calendar.js")),
    ("doc-combo.js", JS, include_bytes!("../../assets/doc-combo.js")),
    ("doc-builder.js", JS, include_bytes!("../../assets/doc-builder.js")),
    ("doc-drag.js", JS, include_bytes!("../../assets/doc-drag.js")),
    ("doc-filter.js", JS, include_bytes!("../../assets/doc-filter.js")),
    ("doc-highlight.js", JS, include_bytes!("../../assets/doc-highlight.js")),
    ("doc-graph.js", JS, include_bytes!("../../assets/doc-graph.js")),
    ("doc-canvas.js", JS, include_bytes!("../../assets/doc-canvas.js")),
    ("doc-geomap.js", JS, include_bytes!("../../assets/doc-geomap.js")),
    ("geo/countries-110m.json", JSON, include_bytes!("../../assets/geo/countries-110m.json")),
    ("doc-permalink.js", JS, include_bytes!("../../assets/doc-permalink.js")),
    ("doc-editor.js", JS, include_bytes!("../../assets/doc-editor.js")),
    ("brand/doc.png", PNG, include_bytes!("../../assets/brand/doc.png")),
    ("brand/rundoc.png", PNG, include_bytes!("../../assets/brand/rundoc.png")),
    (
        "vendor/montserrat/montserrat-latin-800-normal.woff2",
        WOFF2,
        include_bytes!("../../assets/vendor/montserrat/montserrat-latin-800-normal.woff2"),
    ),
    (
        "vendor/nhsuk/nhsuk-frontend-10.6.1.min.css",
        CSS,
        include_bytes!("../../assets/vendor/nhsuk/nhsuk-frontend-10.6.1.min.css"),
    ),
    (
        "vendor/nhsapp/nhsapp-5.2.0.min.css",
        CSS,
        include_bytes!("../../assets/vendor/nhsapp/nhsapp-5.2.0.min.css"),
    ),
    (
        "vendor/htmx/htmx-4.0.0.min.js",
        JS,
        include_bytes!("../../assets/vendor/htmx/htmx-4.0.0.min.js"),
    ),
    (
        "vendor/alpine/alpine-csp-3.17.3.min.js",
        JS,
        include_bytes!("../../assets/vendor/alpine/alpine-csp-3.17.3.min.js"),
    ),
    (
        "vendor/chartjs/chart-4.5.1.umd.min.js",
        JS,
        include_bytes!("../../assets/vendor/chartjs/chart-4.5.1.umd.min.js"),
    ),
    (
        "vendor/squire/squire-2.4.9.js",
        JS,
        include_bytes!("../../assets/vendor/squire/squire-2.4.9.js"),
    ),
    (
        "vendor/dompurify/purify-3.4.16.min.js",
        JS,
        include_bytes!("../../assets/vendor/dompurify/purify-3.4.16.min.js"),
    ),
    (
        "vendor/marked/marked-18.0.14.umd.js",
        JS,
        include_bytes!("../../assets/vendor/marked/marked-18.0.14.umd.js"),
    ),
    (
        "vendor/turndown/turndown-7.2.4.browser.umd.js",
        JS,
        include_bytes!("../../assets/vendor/turndown/turndown-7.2.4.browser.umd.js"),
    ),
    (
        "vendor/turndown/turndown-plugin-gfm-1.0.2.js",
        JS,
        include_bytes!("../../assets/vendor/turndown/turndown-plugin-gfm-1.0.2.js"),
    ),
    (
        "images/nhsuk-icon-arrow-down.svg",
        SVG,
        include_bytes!("../../assets/vendor/nhsuk/assets/images/nhsuk-icon-arrow-down.svg"),
    ),
    (
        "images/nhsuk-icon-arrow-up.svg",
        SVG,
        include_bytes!("../../assets/vendor/nhsuk/assets/images/nhsuk-icon-arrow-up.svg"),
    ),
    (
        "images/nhsuk-icon-arrow-up-down.svg",
        SVG,
        include_bytes!("../../assets/vendor/nhsuk/assets/images/nhsuk-icon-arrow-up-down.svg"),
    ),
    (
        "images/nhsuk-icon-cross.svg",
        SVG,
        include_bytes!("../../assets/vendor/nhsuk/assets/images/nhsuk-icon-cross.svg"),
    ),
];

struct Asset {
    mime: &'static str,
    body: &'static [u8],
    cache: &'static str,
}

struct Assets {
    served: HashMap<String, Asset>,
    urls: HashMap<&'static str, String>,
}

static ASSETS: LazyLock<Assets> = LazyLock::new(|| {
    let mut served = HashMap::with_capacity(EMBEDDED.len() * 2);
    let mut urls = HashMap::with_capacity(EMBEDDED.len());
    for (name, mime, body) in EMBEDDED {
        let hashed = hashed_path(name, body);
        urls.insert(*name, format!("/assets/{hashed}"));
        served.insert(hashed, Asset { mime, body, cache: IMMUTABLE });
        served.insert((*name).to_string(), Asset { mime, body, cache: REVALIDATE });
    }
    Assets { served, urls }
});

fn hashed_path(name: &str, body: &[u8]) -> String {
    use sha2::Digest;
    let digest = hex::encode(sha2::Sha256::digest(body));
    let hash = &digest[..HASH_LEN];
    match name.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}.{hash}.{ext}"),
        None => format!("{name}.{hash}"),
    }
}

/// An unknown name falls back to the plain path, which is served but not cached forever.
pub fn url(name: &str) -> String {
    ASSETS.urls.get(name).cloned().unwrap_or_else(|| format!("/assets/{name}"))
}

pub fn warm() -> usize {
    ASSETS.urls.len()
}

pub async fn asset(Path(path): Path<String>) -> Response {
    match ASSETS.served.get(&path) {
        Some(asset) => {
            ([(CONTENT_TYPE, asset.mime), (CACHE_CONTROL, asset.cache)], asset.body).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
