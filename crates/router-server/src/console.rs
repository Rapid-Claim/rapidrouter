//! Static web console embedded into the executable at compile time.

use axum::extract::Path;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../console/dist/"]
struct Assets;

pub async fn root(headers: HeaderMap) -> Response {
    asset(None, headers).await
}

/// Browsers request the icon at the domain root on their own, and a page
/// visited at `/console` (no slash) resolves relative links there too.
pub async fn favicon(headers: HeaderMap) -> Response {
    asset(Some(Path("favicon.svg".to_owned())), headers).await
}

pub async fn asset(path: Option<Path<String>>, headers: HeaderMap) -> Response {
    let requested = path.map(|Path(path)| path).unwrap_or_default();
    let name = if requested.is_empty() {
        "index.html"
    } else {
        requested.as_str()
    };
    let (asset, served_name) = match Assets::get(name) {
        Some(asset) => (asset, name),
        None => match Assets::get("index.html") {
            Some(asset) => (asset, "index.html"),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let content_type = match served_name.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    // `index.html` is `no-cache`, which means "revalidate", not "do not
    // store" — but without a validator there is nothing to revalidate
    // against, so every reload re-downloaded the document. rust-embed
    // hands us the file's hash, which is exactly the validator this
    // wants: a reload of an unchanged console becomes a 304 with no body.
    let etag = format!("\"{}\"", hex(&asset.metadata.sha256_hash()[..8]));
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|presented| presented.split(',').any(|tag| tag.trim() == etag))
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }

    let mut response = asset.data.into_owned().into_response();
    if let Ok(etag) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, etag);
    }
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if served_name == "index.html" {
            "no-cache"
        } else {
            "public, max-age=31536000, immutable"
        }),
    );
    response
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
