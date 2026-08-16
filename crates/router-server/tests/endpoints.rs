use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use router_core::config::{Config, Format};
use router_server::{AppState, build_router};
use tower::ServiceExt;

fn test_state() -> Arc<AppState> {
    let config = Config::from_str_with_env(
        "[providers.ollama]\nauth = \"none\"\n",
        Format::Toml,
        &|_: &str| None,
    )
    .unwrap();
    AppState::new(config)
}

#[tokio::test]
async fn health_reports_ok() {
    let app = build_router(test_state());
    let res = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn health_flips_to_draining() {
    let state = test_state();
    let app = build_router(state.clone());
    state.set_draining();
    let res = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(res.into_body(), 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "draining");
}

#[tokio::test]
async fn metrics_render_prometheus_text() {
    let app = build_router(test_state());
    let res = app
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.contains("rapid_build_info"),
        "metrics output was: {text}"
    );
}
