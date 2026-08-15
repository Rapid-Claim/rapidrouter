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
        .route("/cluster/token", get(cluster_token))
        .route("/cluster/remove", post(cluster_remove))
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

/// Control-plane writes need a control plane: a store, or a cluster.
/// Control-plane writes need a control plane, and file mode does not have
/// one: the file your deploy tool distributes is the source of truth, so
/// the console must not write over it.
#[allow(clippy::result_large_err)] // the Err *is* the response we serve
fn writable(state: &AppState) -> Result<(), Response> {
    if state.file_managed {
        return Err(api_error(
            StatusCode::FORBIDDEN,
            "file mode is read-only; edit the source file and reload",
        ));
    }
    if state.cluster.get().is_some() || state.store.is_some() {
        return Ok(());
    }
    Err(api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "managed store is not configured",
    ))
}

/// Turn a commit failure into the operator-facing distinction that
/// matters: a visible conflict, a quorum problem, or a real fault.
fn commit_error(message: String) -> Response {
    let lower = message.to_lowercase();
    let status = if lower.contains("conflict") {
        StatusCode::CONFLICT
    } else if lower.contains("quorum") || lower.contains("leader") {
        StatusCode::SERVICE_UNAVAILABLE
    } else if lower.contains("read-only") {
        StatusCode::FORBIDDEN
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    api_error(status, message)
}

#[allow(dead_code)]
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
        .store_read()
        .map(|(snapshot, version)| (snapshot.config_text.unwrap_or_default(), version))
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
    if let Err(response) = writable(&state) {
        return response;
    }
    let env = |name: &str| {
        if let Some(secret) = name.strip_prefix("store.") {
            state
                .store
                .as_deref()
                .and_then(|s| s.resolve_secret(secret))
        } else {
            std::env::var(name).ok()
        }
    };
    let config = match Config::from_str_with_env(&input.text, Format::Toml, &env) {
        Ok(config) => config,
        Err(err) => return api_error(StatusCode::UNPROCESSABLE_ENTITY, err.to_string()),
    };
    let version = match state
        .commit(Some(input.version), Command::PutConfig { text: input.text })
        .await
    {
        Ok(version) => version,
        Err(err) => return commit_error(err),
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
    if let Err(response) = writable(&state) {
        return response;
    }
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
    let version = match state
        .commit(None, Command::PutVirtualKey { def: def.clone() })
        .await
    {
        Ok(version) => version,
        Err(err) => return commit_error(err),
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
    if let Err(response) = writable(&state) {
        return response;
    }
    let Some((snapshot, version)) = state.store_read() else {
        return api_error(StatusCode::CONFLICT, "no control-plane store on this node");
    };
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
    let new_version = match state
        .commit(Some(version), Command::PutVirtualKey { def: def.clone() })
        .await
    {
        Ok(version) => version,
        Err(err) => return commit_error(err),
    };
    state.refresh_vkeys();
    Json(json!({ "version": new_version, "data": key_view(&def) })).into_response()
}

async fn rotate_key(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if let Err(response) = writable(&state) {
        return response;
    }
    let Some((snapshot, version)) = state.store_read() else {
        return api_error(StatusCode::CONFLICT, "no control-plane store on this node");
    };
    let Some(mut def) = snapshot.virtual_keys.get(&id).cloned() else {
        return api_error(StatusCode::NOT_FOUND, "virtual key not found");
    };
    let secret = vkey::generate_secret();
    def.prev_secret = Some(vkey::PrevSecret {
        secret_hash: def.secret_hash.clone(),
        valid_until_ms: vkey::unix_now_ms() + 24 * 60 * 60 * 1000,
    });
    def.secret_hash = vkey::hash_secret(&secret);
    let new_version = match state
        .commit(Some(version), Command::PutVirtualKey { def: def.clone() })
        .await
    {
        Ok(version) => version,
        Err(err) => return commit_error(err),
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
    if let Err(response) = writable(&state) {
        return response;
    }
    let version = state.store_read().map(|(_, v)| v).unwrap_or(0);
    let new_version = match state
        .commit(Some(version), Command::DeleteVirtualKey { id })
        .await
    {
        Ok(version) => version,
        Err(err) => return commit_error(err),
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
    let mode = if state.file_managed {
        "file"
    } else {
        "managed"
    };
    let Some(cluster) = state.cluster.get() else {
        // Single node: a cluster of one, described in the same shape so
        // the console renders one code path.
        let version = state.store_read().map(|(_, v)| v).unwrap_or(0);
        return Json(json!({
            "mode": mode,
            "node": state.store.as_deref().map(|s| s.node_id()).unwrap_or("local"),
            "version": version,
            "role": "single",
            "members": 1,
            "live": 1,
            "quorum": true,
            "shares": state.live_nodes(),
            "member_list": [],
        }))
        .into_response();
    };

    let fleet = cluster.fleet().await;
    Json(json!({
        "mode": mode,
        "node": cluster.id.to_string(),
        "version": fleet.applied,
        "role": if fleet.leader == Some(cluster.id) { "leader" } else { "follower" },
        "term": fleet.term,
        "members": fleet.voters,
        "live": fleet.live,
        "quorum": fleet.quorum,
        "shares": state.live_nodes(),
        "leader": fleet.leader.map(|l| l.to_string()),
        "member_list": fleet.members,
    }))
    .into_response()
}

/// The join token, for the console's Cluster page. Admin-only, and the
/// page states plainly that it is a credential.
async fn cluster_token(State(state): State<Arc<AppState>>) -> Response {
    let Some(store) = state.store.as_deref() else {
        return api_error(
            StatusCode::CONFLICT,
            "this node has no store, so it cannot be part of a cluster",
        );
    };
    match store.join_token() {
        Ok(token) => Json(json!({
            "token": token.encode(),
            "cluster": token.cluster_id(),
            "note": "Anyone holding this token can join the cluster.",
        }))
        .into_response(),
        Err(err) => api_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

#[derive(serde::Deserialize)]
struct RemoveNode {
    id: u64,
}

/// Remove a node that is never coming back. Membership is replicated, so
/// this must run on the leader; a follower says where to go.
async fn cluster_remove(
    State(state): State<Arc<AppState>>,
    Json(body): Json<RemoveNode>,
) -> Response {
    let Some(cluster) = state.cluster.get() else {
        return api_error(
            StatusCode::CONFLICT,
            "this node is not clustered — there is no membership to change",
        );
    };
    match cluster.remove_voter(body.id).await {
        Ok(()) => Json(json!({ "removed": body.id })).into_response(),
        Err(router_cluster::raft::ClusterError::NotLeader { .. }) => {
            let leader = cluster.leader_addr().await;
            (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": { "message": "membership changes must go through the leader" },
                    "leader": leader,
                })),
            )
                .into_response()
        }
        Err(err) => api_error(StatusCode::SERVICE_UNAVAILABLE, err.to_string()),
    }
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
