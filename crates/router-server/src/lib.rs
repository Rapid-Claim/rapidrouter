//! HTTP surface: routes, application state, middleware, and graceful
//! serving.

mod proxy;
mod upstream;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use router_core::config::Config;
use router_core::router::RoutingTable;
use router_core::{ErrorClass, GatewayError};
use serde_json::json;

pub use proxy::Endpoint;

pub struct AppState {
    pub config: ArcSwap<Config>,
    pub table: ArcSwap<RoutingTable>,
    pub upstream: upstream::UpstreamClient,
    draining: AtomicBool,
    prometheus: PrometheusHandle,
}

impl AppState {
    pub fn new(config: Config) -> Arc<Self> {
        let table = RoutingTable::from_config(&config);
        Arc::new(Self {
            config: ArcSwap::from_pointee(config),
            table: ArcSwap::from_pointee(table),
            upstream: upstream::UpstreamClient::new(),
            draining: AtomicBool::new(false),
            prometheus: prometheus_handle().clone(),
        })
    }

    /// Apply a validated config: rebuild the routing snapshot and swap
    /// both atomically. In-flight requests keep the table they resolved
    /// against.
    pub fn apply_config(&self, config: Config) {
        let table = RoutingTable::from_config(&config);
        self.table.store(Arc::new(table));
        self.config.store(Arc::new(config));
    }

    pub fn set_draining(&self) {
        self.draining.store(true, Ordering::Relaxed);
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Relaxed)
    }
}

/// The Prometheus recorder is process-global; installing twice (tests,
/// reload paths) must be a no-op rather than a panic.
fn prometheus_handle() -> &'static PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        let handle = PrometheusBuilder::new()
            .install_recorder()
            .expect("no other metrics recorder may be installed");
        metrics::gauge!("caret_build_info", "version" => env!("CARGO_PKG_VERSION")).set(1.0);
        handle
    })
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let max_body = state.config.load().server.max_body_size as usize;

    let v1 = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/models", get(proxy::models))
        .route("/anthropic/v1/messages", post(anthropic_messages))
        .route("/genai/v1beta/models/{model_action}", post(genai_generate))
        .route("/genai/v1/models/{model_action}", post(genai_generate))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            gateway_auth,
        ))
        .layer(DefaultBodyLimit::max(max_body));

    Router::new()
        .merge(v1)
        .route("/health", get(health))
        .route("/metrics", get(metrics_endpoint))
        .layer(axum::middleware::from_fn(request_id))
        .with_state(state)
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    match proxy::InboundChat::from_openai(body) {
        Ok(inbound) => proxy::handle_chat(state, inbound, headers).await,
        Err(err) => proxy::error_response(&err),
    }
}

async fn anthropic_messages(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    match proxy::InboundChat::from_anthropic(&body) {
        Ok(inbound) => proxy::handle_chat(state, inbound, headers).await,
        Err(err) => proxy::error_response_in(router_providers::Dialect::Anthropic, &err),
    }
}

/// Gemini routes carry `{model}:{action}` as one path segment.
async fn genai_generate(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(model_action): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    let dialect = router_providers::Dialect::Gemini;
    let Some((model, action)) = model_action.split_once(':') else {
        return proxy::error_response_in(
            dialect,
            &GatewayError::new(ErrorClass::NotFound, "expected `models/{model}:{action}`"),
        );
    };
    let stream = match action {
        "generateContent" => false,
        "streamGenerateContent" => true,
        other => {
            return proxy::error_response_in(
                dialect,
                &GatewayError::new(
                    ErrorClass::NotFound,
                    format!("unsupported action `{other}`"),
                ),
            );
        }
    };
    match proxy::InboundChat::from_gemini(&body, model.to_owned(), stream) {
        Ok(inbound) => proxy::handle_chat(state, inbound, headers).await,
        Err(err) => proxy::error_response_in(dialect, &err),
    }
}

async fn completions(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    proxy::handle_relay(state, Endpoint::Completions, headers, body).await
}

async fn embeddings(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    proxy::handle_relay(state, Endpoint::Embeddings, headers, body).await
}

/// Honor an inbound `x-request-id` or mint a UUIDv7, and echo it on the
/// response.
async fn request_id(request: Request, next: Next) -> Response {
    let id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    let mut response = next.run(request).await;
    if let Ok(v) = HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", v);
    }
    response
}

/// Constant-time gateway key check; a no-op when no keys are configured.
async fn gateway_auth(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let config = state.config.load();
    if config.server.auth_keys.is_empty() {
        return next.run(request).await;
    }

    // Every dialect's native credential carrier is accepted: OpenAI SDKs
    // send `Authorization: Bearer`, Anthropic SDKs `x-api-key`, Google
    // SDKs `x-goog-api-key` (or `?key=`).
    let headers = request.headers();
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()))
        .or_else(|| headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()))
        .or_else(|| {
            request
                .uri()
                .query()
                .and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("key=")))
        });

    match presented {
        Some(token) if config.server.auth_keys.iter().any(|k| k.verify(token)) => {
            next.run(request).await
        }
        _ => proxy::error_response(&GatewayError::new(
            ErrorClass::Authentication,
            "missing or invalid gateway API key",
        )),
    }
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if state.is_draining() {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "draining" })),
        )
    } else {
        (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
            })),
        )
    }
}

async fn metrics_endpoint(State(state): State<Arc<AppState>>) -> String {
    state.prometheus.render()
}

/// Serve until `shutdown` resolves, then drain: stop accepting, flip
/// `/health` to draining, and give in-flight requests up to
/// `drain_timeout` to finish before returning.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
    app: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let drain_timeout = state.config.load().server.drain_timeout;
    let drain_state = state;

    let (drained_tx, drained_rx) = tokio::sync::oneshot::channel::<()>();
    let graceful = async move {
        shutdown.await;
        drain_state.set_draining();
        tracing::info!("shutdown requested; draining in-flight requests");
        let _ = drained_tx.send(());
    };

    let server = axum::serve(listener, app).with_graceful_shutdown(graceful);
    tokio::select! {
        result = server => result,
        _ = enforce_drain_deadline(drained_rx, drain_timeout) => {
            tracing::warn!(?drain_timeout, "drain deadline exceeded; exiting with requests in flight");
            Ok(())
        }
    }
}

async fn enforce_drain_deadline(drained: tokio::sync::oneshot::Receiver<()>, timeout: Duration) {
    // The deadline starts when draining starts, not when the server starts.
    let _ = drained.await;
    tokio::time::sleep(timeout).await;
}
