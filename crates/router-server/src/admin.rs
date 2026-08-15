//! Embedded control-plane HTTP API used by the console and CLI.

use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::http::header::SET_COOKIE;
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use router_cluster::{Command, StoreError};
use router_core::config::{Config, Format};
use router_core::vkey::{self, Budget, RateLimit, VirtualKeyDef};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AppState;

pub fn router(state: Arc<AppState>) -> Router<Arc<AppState>> {
    let protected = Router::new()
        .route("/config", get(get_config).put(put_config))
        .route("/keys", get(list_keys).post(create_key))
        .route("/keys/{id}", put(update_key).delete(delete_key))
        .route("/keys/{id}/rotate", post(rotate_key))
        .route("/usage", get(usage))
        .route("/requests", get(requests))
        .route("/fleet", get(fleet))
        .route("/events", get(events))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admin_auth,
        ));
    Router::new()
        .route("/session", post(create_session))
        .merge(protected)
}

fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": { "message": message.into() } })),
    )
        .into_response()
}

#[allow(clippy::result_large_err)] // the Err *is* the response we serve
fn store_or_error(state: &AppState) -> Result<&router_cluster::Store, Response> {
    if state.file_managed {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "file mode is read-only; edit the source file and reload",
        ));
    }
    state.store.as_deref().ok_or_else(|| {
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "managed store is not configured",
        )
    })
}

fn map_store_error(err: StoreError) -> Response {
    match err {
        StoreError::CasConflict { expected, actual } => api_error(
            StatusCode::CONFLICT,
            format!("version conflict: expected {expected}, current version is {actual}"),
        ),
        other => api_error(StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    }
}

#[derive(Deserialize)]
struct SessionRequest {
    key: String,
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(input): Json<SessionRequest>,
) -> Response {
    let config = state.config.load();
    if !config.console.enabled()
        || !config
            .console
            .admin_keys
            .iter()
            .any(|key| key.verify(&input.key))
    {
        return api_error(StatusCode::UNAUTHORIZED, "invalid admin key");
    }
    let token = format!("cs_{}{}", uuid::Uuid::now_v7().simple(), fastrand::u64(..));
    let expires_ms =
        vkey::unix_now_ms() + config.console.session_ttl.as_millis().min(u64::MAX as u128) as u64;
    state
        .sessions
        .lock()
        .unwrap()
        .insert(token.clone(), expires_ms);
    let mut response = Json(json!({ "token": token, "expires_ms": expires_ms })).into_response();
    if let Ok(cookie) = format!(
        "caret_session={}; Path=/admin/api; HttpOnly; SameSite=Strict; Max-Age={}",
        token,
        config.console.session_ttl.as_secs(),
    )
    .parse()
    {
        response.headers_mut().insert(SET_COOKIE, cookie);
    }
    response
}

async fn admin_auth(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let config = state.config.load();
    if !config.console.enabled() {
        return api_error(StatusCode::NOT_FOUND, "console is disabled");
    }
    let bearer = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let cookie = request
        .headers()
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies
                .split(';')
                .map(str::trim)
                .find_map(|value| value.strip_prefix("caret_session="))
        });
    let valid_static = bearer.is_some_and(|token| {
        config
            .console
            .admin_keys
            .iter()
            .any(|key| key.verify(token))
    });
    let now = vkey::unix_now_ms();
    let valid_session = bearer.or(cookie).is_some_and(|token| {
        let mut sessions = state.sessions.lock().unwrap();
        sessions.retain(|_, expiry| *expiry >= now);
        sessions.get(token).is_some_and(|expiry| *expiry >= now)
    });
    if !valid_static && !valid_session {
        return api_error(StatusCode::UNAUTHORIZED, "missing or invalid admin session");
    }
    next.run(request).await
}

async fn get_config(State(state): State<Arc<AppState>>) -> Response {
    let (text, version) = state
        .store
        .as_deref()
        .map(|store| {
            let (snapshot, version) = store.read();
            (snapshot.config_text.unwrap_or_default(), version)
        })
        .unwrap_or_default();
    Json(json!({
        "mode": if state.file_managed { "file" } else { "managed" },
        "read_only": state.file_managed,
        "version": version,
        "text": text,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ConfigWrite {
    version: u64,
    text: String,
}

async fn put_config(
    State(state): State<Arc<AppState>>,
    Json(input): Json<ConfigWrite>,
) -> Response {
    let store = match store_or_error(&state) {
        Ok(store) => store,
        Err(response) => return response,
    };
    let env = |name: &str| {
        if let Some(secret) = name.strip_prefix("store.") {
            store.resolve_secret(secret)
        } else {
            std::env::var(name).ok()
        }
    };
    let config = match Config::from_str_with_env(&input.text, Format::Toml, &env) {
        Ok(config) => config,
        Err(err) => return api_error(StatusCode::UNPROCESSABLE_ENTITY, err.to_string()),
    };
    let version = match store.commit(Some(input.version), Command::PutConfig { text: input.text }) {
        Ok(version) => version,
        Err(err) => return map_store_error(err),
    };
    state.apply_config(config);
    Json(json!({ "version": version })).into_response()
}

#[derive(Debug, Deserialize)]
struct KeyInput {
    name: String,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default)]
    budget: Option<Budget>,
    #[serde(default)]
    rate: Option<RateLimit>,
    #[serde(default)]
    expires_ms: Option<u64>,
    #[serde(default)]
    tags: std::collections::BTreeMap<String, String>,
}

#[derive(Serialize)]
struct KeyView<'a> {
    id: &'a str,
    name: &'a str,
    models: &'a [String],
    budget: Option<Budget>,
    rate: Option<RateLimit>,
    expires_ms: Option<u64>,
    tags: &'a std::collections::BTreeMap<String, String>,
    enabled: bool,
    created_ms: u64,
}

fn key_view(def: &VirtualKeyDef) -> KeyView<'_> {
    KeyView {
        id: &def.id,
        name: &def.name,
        models: &def.models,
        budget: def.budget,
        rate: def.rate,
        expires_ms: def.expires_ms,
        tags: &def.tags,
        enabled: def.enabled,
        created_ms: def.created_ms,
    }
}

async fn list_keys(State(state): State<Arc<AppState>>) -> Response {
    let mut defs: Vec<VirtualKeyDef> = state.vkeys.load().iter().map(|rt| rt.def.clone()).collect();
    defs.sort_by(|a, b| a.name.cmp(&b.name));
    Json(json!({ "data": defs.iter().map(key_view).collect::<Vec<_>>() })).into_response()
}

async fn create_key(State(state): State<Arc<AppState>>, Json(input): Json<KeyInput>) -> Response {
    let store = match store_or_error(&state) {
        Ok(store) => store,
        Err(response) => return response,
    };
    if input.name.trim().is_empty() {
        return api_error(StatusCode::UNPROCESSABLE_ENTITY, "name must not be empty");
    }
    let generated = vkey::generate();
    let def = VirtualKeyDef {
        id: generated.id.clone(),
        name: input.name,
        secret_hash: vkey::hash_secret(&generated.secret),
        prev_secret: None,
        models: input.models,
        budget: input.budget,
        rate: input.rate,
        expires_ms: input.expires_ms,
        tags: input.tags,
        enabled: true,
        created_ms: vkey::unix_now_ms(),
    };
    let version = match store.commit(None, Command::PutVirtualKey { def: def.clone() }) {
        Ok(version) => version,
        Err(err) => return map_store_error(err),
    };
    state.refresh_vkeys();
    Json(json!({
        "key": generated.full(),
        "version": version,
        "data": key_view(&def),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct KeyUpdate {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    models: Option<Vec<String>>,
    #[serde(default)]
    budget: Option<Option<Budget>>,
    #[serde(default)]
    rate: Option<Option<RateLimit>>,
    #[serde(default)]
    expires_ms: Option<Option<u64>>,
    #[serde(default)]
    tags: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    enabled: Option<bool>,
}

async fn update_key(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(input): Json<KeyUpdate>,
) -> Response {
    let store = match store_or_error(&state) {
        Ok(store) => store,
        Err(response) => return response,
    };
    let (snapshot, version) = store.read();
    let Some(mut def) = snapshot.virtual_keys.get(&id).cloned() else {
        return api_error(StatusCode::NOT_FOUND, "virtual key not found");
    };
    if let Some(name) = input.name {
        def.name = name;
    }
    if let Some(models) = input.models {
        def.models = models;
    }
    if let Some(budget) = input.budget {
        def.budget = budget;
    }
    if let Some(rate) = input.rate {
        def.rate = rate;
    }
    if let Some(expires) = input.expires_ms {
        def.expires_ms = expires;
    }
    if let Some(tags) = input.tags {
        def.tags = tags;
    }
    if let Some(enabled) = input.enabled {
        def.enabled = enabled;
    }
    let new_version = match store.commit(Some(version), Command::PutVirtualKey { def: def.clone() })
    {
        Ok(version) => version,
        Err(err) => return map_store_error(err),
    };
    state.refresh_vkeys();
    Json(json!({ "version": new_version, "data": key_view(&def) })).into_response()
}

async fn rotate_key(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let store = match store_or_error(&state) {
        Ok(store) => store,
        Err(response) => return response,
    };
    let (snapshot, version) = store.read();
    let Some(mut def) = snapshot.virtual_keys.get(&id).cloned() else {
        return api_error(StatusCode::NOT_FOUND, "virtual key not found");
    };
    let secret = vkey::generate_secret();
    def.prev_secret = Some(vkey::PrevSecret {
        secret_hash: def.secret_hash.clone(),
        valid_until_ms: vkey::unix_now_ms() + 24 * 60 * 60 * 1000,
    });
    def.secret_hash = vkey::hash_secret(&secret);
    let new_version = match store.commit(Some(version), Command::PutVirtualKey { def: def.clone() })
    {
        Ok(version) => version,
        Err(err) => return map_store_error(err),
    };
    state.refresh_vkeys();
    Json(json!({
        "key": format!("ck-{}-{secret}", def.id),
        "version": new_version,
        "grace_until_ms": def.prev_secret.as_ref().unwrap().valid_until_ms,
    }))
    .into_response()
}

async fn delete_key(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let store = match store_or_error(&state) {
        Ok(store) => store,
        Err(response) => return response,
    };
    let version = store.version();
    let new_version = match store.commit(Some(version), Command::DeleteVirtualKey { id }) {
        Ok(version) => version,
        Err(err) => return map_store_error(err),
    };
    state.refresh_vkeys();
    Json(json!({ "version": new_version })).into_response()
}

#[derive(Deserialize)]
struct UsageQuery {
    #[serde(default = "default_window")]
    window: u64,
    #[serde(default)]
    by: Option<String>,
    #[serde(default)]
    key: Option<String>,
}

fn default_window() -> u64 {
    3600
}

async fn usage(State(state): State<Arc<AppState>>, Query(query): Query<UsageQuery>) -> Response {
    let by: Vec<&str> = query
        .by
        .as_deref()
        .unwrap_or("provider")
        .split(',')
        .filter(|v| matches!(*v, "provider" | "model" | "key"))
        .collect();
    Json(state.usage.agg.query(
        vkey::unix_now_ms(),
        query.window.clamp(60, 86_400),
        &by,
        query.key.as_deref(),
    ))
    .into_response()
}

#[derive(Deserialize)]
struct RequestsQuery {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    errors: bool,
}

fn default_limit() -> usize {
    100
}

async fn requests(
    State(state): State<Arc<AppState>>,
    Query(query): Query<RequestsQuery>,
) -> Response {
    Json(json!({
        "data": state.usage.recent(
            query.limit,
            query.key.as_deref(),
            query.errors.then_some(400),
            query.provider.as_deref(),
        )
    }))
    .into_response()
}

async fn fleet(State(state): State<Arc<AppState>>) -> Response {
    Json(json!({
        "mode": if state.file_managed { "file" } else { "managed" },
        "node": state.store.as_deref().map(|s| s.node_id()).unwrap_or("local"),
        "version": state.store.as_deref().map(|s| s.version()).unwrap_or(0),
        "role": "single",
        "members": 1,
        "quorum": true,
    }))
    .into_response()
}

async fn events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let receiver = state.events.subscribe();
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        loop {
            match receiver.recv().await {
                Ok(value) => {
                    let event = Event::default().json_data(value).unwrap_or_default();
                    return Some((Ok(event), receiver));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[allow(dead_code)]
fn _assert_json_is_send(_: Value) {}
