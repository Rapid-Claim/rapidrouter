//! Static web console embedded into the executable at compile time.

use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../../console/dist/"]
struct Assets;

pub async fn root() -> Response {
    asset(None).await
}

pub async fn asset(path: Option<Path<String>>) -> Response {
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
    let mut response = asset.data.into_owned().into_response();
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
