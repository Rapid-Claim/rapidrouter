//! HTTP surface: routes, application state, middleware, and graceful
//! serving.

mod admin;
#[cfg(feature = "console")]
mod console;
mod proxy;
mod upstream;
pub mod usage;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use router_cluster::Store;
use router_core::config::Config;
use router_core::router::RoutingTable;
use router_core::vkey::{self, VirtualKeyDef, VkRuntime, VkTable};
use router_core::{ErrorClass, GatewayError};
use serde_json::json;

pub use proxy::Endpoint;

/// The authenticated virtual key of a request (None: static key or open
/// access). Inserted by the auth layer, consumed by dispatch for scope,
/// limit, and budget enforcement and usage attribution.
#[derive(Clone, Default)]
pub struct VkCtx(pub Option<Arc<VkRuntime>>);

pub struct AppState {
    pub config: ArcSwap<Config>,
    pub table: ArcSwap<RoutingTable>,
    pub vkeys: ArcSwap<VkTable>,
    pub pricing: ArcSwap<usage::Pricing>,
    pub usage: Arc<usage::UsagePipeline>,
    /// The managed store; `None` in file/env mode.
    pub store: Option<Arc<Store>>,
    /// Live console events (request ticks, config applies).
    pub events: tokio::sync::broadcast::Sender<serde_json::Value>,
    /// Admin session tokens -> unix-ms expiry.
    pub sessions: Mutex<HashMap<String, u64>>,
    /// Config's source of truth is a file: admin writes are disabled.
    pub file_managed: bool,
    /// The Raft member, when `[cluster]` is configured. Absent means a
    /// single node, which is the default and changes nothing else.
    pub cluster: std::sync::OnceLock<Arc<router_cluster::raft::ClusterNode>>,
    pub upstream: upstream::UpstreamClient,
    draining: AtomicBool,
    prometheus: PrometheusHandle,
}

impl AppState {
    /// Ephemeral state: no store, no usage persistence. Tests and
    /// pure-env deployments.
    pub fn new(config: Config) -> Arc<Self> {
        Self::build(config, None, None, true)
    }

    /// Managed mode: the store is the source of truth and admin writes
    /// are enabled.
    pub fn managed(config: Config, store: Arc<Store>, data_dir: PathBuf) -> Arc<Self> {
        Self::build(config, Some(store), Some(data_dir), false)
    }

    /// File mode with a data dir: config stays file-owned (admin
    /// read-only) but usage history and store-held virtual keys persist.
    pub fn file_with_data_dir(config: Config, store: Arc<Store>, data_dir: PathBuf) -> Arc<Self> {
        Self::build(config, Some(store), Some(data_dir), true)
    }

    fn build(
        config: Config,
        store: Option<Arc<Store>>,
        data_dir: Option<PathBuf>,
        file_managed: bool,
    ) -> Arc<Self> {
        let table = RoutingTable::from_config(&config);
        let defs = Self::collect_vkey_defs(&config, store.as_deref());
        let vkeys = VkTable::build(&defs, None);
        let pricing = usage::Pricing::from_config(&config);
        let node_id = store
            .as_deref()
            .map(|s| s.node_id().to_owned())
            .unwrap_or_else(|| "node".into());
        let usage = usage::UsagePipeline::start(data_dir.clone(), &config.usage, &node_id);
        if let Some(dir) = data_dir.as_deref() {
            usage::UsagePipeline::seed_budgets(dir, &vkeys);
        }
        let (events, _) = tokio::sync::broadcast::channel(256);
        Arc::new(Self {
            config: ArcSwap::from_pointee(config),
            table: ArcSwap::from_pointee(table),
            vkeys: ArcSwap::from_pointee(vkeys),
            pricing: ArcSwap::from_pointee(pricing),
            usage,
            store,
            events,
            sessions: Mutex::new(HashMap::new()),
            file_managed,
            cluster: std::sync::OnceLock::new(),
            upstream: upstream::UpstreamClient::new(),
            draining: AtomicBool::new(false),
            prometheus: prometheus_handle().clone(),
        })
    }

    /// File-declared keys plus store-held keys (store wins on id clash —
    /// rotation state lives there).
    fn collect_vkey_defs(config: &Config, store: Option<&Store>) -> Vec<VirtualKeyDef> {
        let mut by_id: std::collections::BTreeMap<String, VirtualKeyDef> = config
            .virtual_keys
            .iter()
            .map(|d| (d.id.clone(), d.clone()))
            .collect();
        if let Some(store) = store {
            for def in store.read().0.virtual_key_defs() {
                by_id.insert(def.id.clone(), def);
            }
        }
        by_id.into_values().collect()
    }

    /// Apply a validated config: rebuild the routing snapshot, key table,
    /// and pricing, and swap them atomically. In-flight requests keep the
    /// snapshots they resolved against.
    pub fn apply_config(&self, config: Config) {
        let table = RoutingTable::from_config(&config);
        let defs = self.collect_defs(&config);
        let prev = self.vkeys.load();
        self.vkeys.store(Arc::new(VkTable::build_with_shares(
            &defs,
            Some(&prev),
            self.live_nodes(),
        )));
        self.pricing
            .store(Arc::new(usage::Pricing::from_config(&config)));
        self.table.store(Arc::new(table));
        self.config.store(Arc::new(config));
        let _ = self.events.send(json!({ "type": "config_applied" }));
    }

    /// Rebuild the key table after a store-side key change (create,
    /// rotate, revoke) without touching routing.
    pub fn refresh_vkeys(&self) {
        let config = self.config.load();
        let defs = self.collect_defs(&config);
        let prev = self.vkeys.load();
        self.vkeys.store(Arc::new(VkTable::build_with_shares(
            &defs,
            Some(&prev),
            self.live_nodes(),
        )));
    }

    /// Key definitions from the config plus whichever control plane is
    /// authoritative — the replicated state when clustered, the local
    /// store otherwise.
    fn collect_defs(&self, config: &Config) -> Vec<VirtualKeyDef> {
        let mut by_id: std::collections::BTreeMap<String, VirtualKeyDef> = config
            .virtual_keys
            .iter()
            .map(|d| (d.id.clone(), d.clone()))
            .collect();
        if let Some((state, _)) = self.store_read() {
            for def in state.virtual_key_defs() {
                by_id.insert(def.id.clone(), def);
            }
        }
        by_id.into_values().collect()
    }

    /// Keep per-node limit shares tracking the live member count. Cheap
    /// enough to poll: it reads one atomic-ish metric and only rebuilds
    /// the key table when the count actually moves.
    pub fn spawn_share_tracker(self: &Arc<Self>) {
        let state = self.clone();
        tokio::spawn(async move {
            let mut last = state.live_nodes();
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                let now = state.live_nodes();
                if now != last {
                    last = now;
                    state.refresh_vkeys();
                    tracing::info!(live_nodes = now, "rate-limit shares rescaled");
                    let _ = state
                        .events
                        .send(json!({ "type": "membership_changed", "live": now }));
                }
            }
        });
    }

    /// Attach the cluster member once it has started.
    pub fn attach_cluster(&self, node: Arc<router_cluster::raft::ClusterNode>) {
        let _ = self.cluster.set(node);
    }

    /// How many gateway nodes are alive. Rate limits divide by this, so a
    /// key's `rpm` tracks the fleet without anyone editing configs.
    pub fn live_nodes(&self) -> usize {
        self.cluster
            .get()
            .map(|c| c.live_members())
            .unwrap_or(1)
            .max(1)
    }

    /// Commit a control-plane change: through consensus when clustered,
    /// straight to the local store when not.
    pub async fn commit(
        &self,
        expect: Option<u64>,
        command: router_cluster::Command,
    ) -> Result<u64, String> {
        if let Some(cluster) = self.cluster.get() {
            // Raft owns ordering; the CAS check happens against the
            // applied version we just read.
            if let Some(expected) = expect {
                let (_, current) = cluster.read();
                if expected != current {
                    return Err(format!(
                        "version conflict: expected {expected}, store is at {current}"
                    ));
                }
            }
            return cluster.commit(command).await.map_err(|e| e.to_string());
        }
        let store = self.store.as_deref().ok_or("no store configured")?;
        store.commit(expect, command).map_err(|e| e.to_string())
    }

    /// The current control-plane document and the version it reflects.
    pub fn store_read(&self) -> Option<(router_cluster::StoreState, u64)> {
        if let Some(cluster) = self.cluster.get() {
            return Some(cluster.read());
        }
        self.store.as_deref().map(|s| s.read())
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
        .route("/v1/responses", post(responses))
        .route("/v1/completions", post(completions))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/audio/speech", post(audio_speech))
        .route("/v1/audio/transcriptions", post(audio_transcriptions))
        .route("/v1/audio/translations", post(audio_translations))
        .route("/v1/images/generations", post(images_generations))
        .route("/v1/files", post(files_upload).get(files_list))
        .route("/v1/files/{id}", get(files_retrieve))
        .route("/v1/files/{id}/content", get(files_content))
        .route("/v1/models", get(proxy::models))
        .route("/anthropic/v1/messages", post(anthropic_messages))
        .route("/genai/v1beta/models/{model_action}", post(genai_generate))
        .route("/genai/v1/models/{model_action}", post(genai_generate))
        .route(
            "/passthrough/{provider}/{*rest}",
            axum::routing::any(passthrough),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            gateway_auth,
        ))
        .layer(DefaultBodyLimit::max(max_body));

    let app = Router::new()
        .merge(v1)
        .nest("/admin/api", admin::router(state.clone()))
        .route("/health", get(health))
        .route("/metrics", get(metrics_endpoint))
        .layer(axum::middleware::from_fn(request_id));
    #[cfg(feature = "console")]
    let app = app
        .route("/console", get(console::root))
        .route("/console/", get(console::root))
        .route("/console/{*path}", get(console::asset));
    app.with_state(state)
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    axum::Extension(vk): axum::Extension<VkCtx>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    match proxy::InboundChat::from_openai(body) {
        Ok(inbound) => proxy::handle_chat(state, inbound, headers, vk.0).await,
        Err(err) => proxy::error_response(&err),
    }
}

async fn anthropic_messages(
    State(state): State<Arc<AppState>>,
    axum::Extension(vk): axum::Extension<VkCtx>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    match proxy::InboundChat::from_anthropic(&body) {
        Ok(inbound) => proxy::handle_chat(state, inbound, headers, vk.0).await,
        Err(err) => proxy::error_response_in(router_providers::Dialect::Anthropic, &err),
    }
}

/// Gemini routes carry `{model}:{action}` as one path segment.
async fn genai_generate(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(model_action): axum::extract::Path<String>,
    axum::Extension(vk): axum::Extension<VkCtx>,
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
        Ok(inbound) => proxy::handle_chat(state, inbound, headers, vk.0).await,
        Err(err) => proxy::error_response_in(dialect, &err),
    }
}

async fn responses(
    State(state): State<Arc<AppState>>,
    axum::Extension(vk): axum::Extension<VkCtx>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    proxy::handle_responses(state, headers, body, vk.0).await
}

async fn passthrough(
    State(state): State<Arc<AppState>>,
    axum::extract::Path((provider, rest)): axum::extract::Path<(String, String)>,
    axum::Extension(vk): axum::Extension<VkCtx>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    let query = uri.query().map(str::to_owned);
    proxy::handle_passthrough(state, provider, rest, method, query, headers, body, vk.0).await
}

async fn completions(
    State(state): State<Arc<AppState>>,
    axum::Extension(vk): axum::Extension<VkCtx>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    proxy::handle_relay(state, Endpoint::Completions, headers, body, vk.0).await
}

async fn embeddings(
    State(state): State<Arc<AppState>>,
    axum::Extension(vk): axum::Extension<VkCtx>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    proxy::handle_relay(state, Endpoint::Embeddings, headers, body, vk.0).await
}

async fn audio_speech(
    State(state): State<Arc<AppState>>,
    axum::Extension(vk): axum::Extension<VkCtx>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    proxy::handle_relay(state, Endpoint::AudioSpeech, headers, body, vk.0).await
}

async fn images_generations(
    State(state): State<Arc<AppState>>,
    axum::Extension(vk): axum::Extension<VkCtx>,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
) -> Response {
    proxy::handle_relay(state, Endpoint::ImagesGenerations, headers, body, vk.0).await
}

async fn audio_transcriptions(
    State(state): State<Arc<AppState>>,
    axum::Extension(vk): axum::Extension<VkCtx>,
    headers: axum::http::HeaderMap,
    body: Body,
) -> Response {
    proxy::handle_stream_relay(
        state,
        "/audio/transcriptions",
        "audio_transcriptions",
        headers,
        body,
        vk.0,
        true,
    )
    .await
}

async fn audio_translations(
    State(state): State<Arc<AppState>>,
    axum::Extension(vk): axum::Extension<VkCtx>,
    headers: axum::http::HeaderMap,
    body: Body,
) -> Response {
    proxy::handle_stream_relay(
        state,
        "/audio/translations",
        "audio_translations",
        headers,
        body,
        vk.0,
        true,
    )
    .await
}

async fn files_upload(
    State(state): State<Arc<AppState>>,
    axum::Extension(vk): axum::Extension<VkCtx>,
    headers: axum::http::HeaderMap,
    body: Body,
) -> Response {
    proxy::handle_stream_relay(state, "/files", "files_upload", headers, body, vk.0, false).await
}

async fn files_list(
    State(state): State<Arc<AppState>>,
    axum::Extension(vk): axum::Extension<VkCtx>,
    headers: axum::http::HeaderMap,
) -> Response {
    proxy::handle_provider_relay(state, "/files", "files_list", headers, vk.0).await
}

async fn files_retrieve(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Extension(vk): axum::Extension<VkCtx>,
    headers: axum::http::HeaderMap,
) -> Response {
    proxy::handle_provider_relay(
        state,
        &format!("/files/{id}"),
        "files_retrieve",
        headers,
        vk.0,
    )
    .await
}

async fn files_content(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Extension(vk): axum::Extension<VkCtx>,
    headers: axum::http::HeaderMap,
) -> Response {
    proxy::handle_provider_relay(
        state,
        &format!("/files/{id}/content"),
        "files_content",
        headers,
        vk.0,
    )
    .await
}

/// Honor an inbound `x-request-id` or mint a UUIDv7, and echo it on the
/// response.
async fn request_id(mut request: Request, next: Next) -> Response {
    let id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    // Stamp it back onto the request so the dispatch path records the same
    // id the client is handed — a log line the caller quotes has to find
    // the request they actually made.
    if let Ok(v) = HeaderValue::from_str(&id) {
        request.headers_mut().insert("x-request-id", v);
    }

    let mut response = next.run(request).await;
    if let Ok(v) = HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", v);
    }
    response
}

/// Gateway authentication: virtual keys (`ck-…`) verify against the key
/// table (constant-time); anything else checks the static `auth_keys`.
/// Anonymous access exists only when no static keys are configured and
/// `require_auth` is off.
async fn gateway_auth(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let config = state.config.load();

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
        })
        .map(str::to_owned);

    let gate_active = !config.server.auth_keys.is_empty() || config.server.require_auth;
    let mut ctx = VkCtx::default();
    let static_match = presented
        .as_deref()
        .is_some_and(|token| config.server.auth_keys.iter().any(|key| key.verify(token)));

    match presented.as_deref() {
        Some(_) if static_match => {}
        // A `ck-` token always means virtual-key semantics — enforced
        // when it is not also an explicitly configured static key.
        Some(token) if token.starts_with("ck-") => {
            match state.vkeys.load().verify(token, vkey::unix_now_ms()) {
                Ok(rt) => ctx.0 = Some(rt),
                Err(reason) => {
                    metrics::counter!("caret_vkey_rejects_total", "reason" => format!("{reason:?}"))
                        .increment(1);
                    return proxy::error_response(&GatewayError::new(
                        ErrorClass::Authentication,
                        "invalid virtual key",
                    ));
                }
            }
        }
        Some(_) => {
            if gate_active {
                return proxy::error_response(&GatewayError::new(
                    ErrorClass::Authentication,
                    "missing or invalid gateway API key",
                ));
            }
        }
        None => {
            if gate_active {
                return proxy::error_response(&GatewayError::new(
                    ErrorClass::Authentication,
                    "missing or invalid gateway API key",
                ));
            }
        }
    }

    request.extensions_mut().insert(ctx);
    next.run(request).await
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
