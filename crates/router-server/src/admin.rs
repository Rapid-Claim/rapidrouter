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
use router_core::config::{Config, Format};
use router_core::vkey::{self, Budget, RateLimit, VirtualKeyDef};
use router_store::{Command, ControlPlaneError};
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
        .route("/providers", get(providers))
        .route("/history", get(history))
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
    if state.store.is_some() {
        return Ok(());
    }
    Err(api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "managed store is not configured",
    ))
}

/// Turn a commit failure into the operator-facing distinction that
/// matters: an edit that raced someone else, a store that is down, or a
/// configuration problem only a human can fix.
fn commit_error(err: ControlPlaneError) -> Response {
    let status = match err {
        ControlPlaneError::Conflict { .. } => StatusCode::CONFLICT,
        ControlPlaneError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        ControlPlaneError::Fault(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    api_error(status, err.to_string())
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
    // Validate before committing, so a bad config is rejected instead of
    // stored and then rejected by every node that reads it.
    if let Err(err) = Config::from_str_with_env(&input.text, Format::Toml, &env) {
        return api_error(StatusCode::UNPROCESSABLE_ENTITY, err.to_string());
    }
    let version = match state
        .commit(Some(input.version), Command::PutConfig { text: input.text })
        .await
    {
        Ok(version) => version,
        Err(err) => return commit_error(err),
    };
    // Adopt through the same path a remote change takes, so there is one
    // place where a config becomes live on a node.
    state.adopt_store_state();
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

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default = "default_days")]
    days: u32,
    #[serde(default)]
    by: Option<String>,
}

fn default_days() -> u32 {
    30
}

/// Daily spend and volume for the Usage page's time ramp.
///
/// The in-memory aggregate only spans 24 hours, so anything longer is
/// read back from the flushed usage files. Capped at a year: the read
/// walks every record in range, and an unbounded range on a busy gateway
/// is a way to make the console the slowest thing in the process.
async fn history(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HistoryQuery>,
) -> Response {
    let by = query.by.as_deref().unwrap_or("");
    let by = if matches!(by, "provider" | "model" | "key") {
        by
    } else {
        ""
    };
    Json(json!({ "data": state.usage.history(query.days.clamp(1, 365), by) })).into_response()
}

/// Per-provider, per-credential state for the console's Providers page.
///
/// The interesting column is the ceiling each credential is working
/// against, and the two kinds are genuinely different. A metered API key
/// is limited by what we configured (`rpm`/`tpm`), and the remaining
/// allowance is ours to compute. A subscription seat is limited by the
/// plan, on windows only the provider knows — Claude's 5h and 7d,
/// Codex's primary and secondary — so what is reported here is the last
/// thing the provider actually told us, with the time it said it.
///
/// Both are per credential rather than per provider, because that is how
/// they are enforced: one exhausted seat must not read as an exhausted
/// pool.
async fn providers(State(state): State<Arc<AppState>>) -> Response {
    let now = router_core::clock::now_ms();
    let now_unix = vkey::unix_now_ms();
    let table = state.table.load();

    let window = |w: Option<router_core::quota::Window>| {
        w.map(|w| {
            json!({
                "utilization": w.utilization,
                "resets_in_s": w.resets_in.map(|d| d.as_secs()),
                "length_s": w.length.map(|d| d.as_secs()),
                "rejected": w.rejected,
            })
        })
    };

    let data: Vec<Value> = table
        .providers()
        .map(|p| {
            let keys: Vec<Value> = p
                .keys
                .iter()
                .map(|k| {
                    let (rpm_left, tpm_left) = k.rate_headroom();
                    let seat = k.credential.seat().map(|s| s.current());
                    json!({
                        "name": k.name,
                        "weight": k.weight,
                        "models": k.models,
                        "health": k.breaker.health(now),
                        // Reported as a wall-clock instant, not a
                        // duration: the console renders a countdown, and a
                        // duration computed here would be stale by the
                        // time it is drawn.
                        "benched_until_ms": k.breaker.benched_until_ms().map(|until| {
                            now_unix.saturating_add(until.saturating_sub(now))
                        }),
                        "limits": {
                            "rpm": k.rpm.as_ref().map(|_| json!({ "remaining": rpm_left })),
                            "tpm": k.tpm.as_ref().map(|_| json!({ "remaining": tpm_left })),
                        },
                        "quota": k.quota().map(|snapshot| json!({
                            "observed_ms": now_unix.saturating_sub(now.saturating_sub(snapshot.observed_ms)),
                            "peak_utilization": snapshot.quota.peak_utilization(),
                            "primary": window(snapshot.quota.primary),
                            "secondary": window(snapshot.quota.secondary),
                        })),
                        // A seat's credential expires; a metered key's
                        // does not. `null` for "unknown" is meaningful —
                        // an opaque token carries no readable expiry.
                        "credential": seat.as_ref().map(|s| json!({
                            "expires_at_ms": s.expires_at_ms,
                            "can_refresh": s.refresh_token.is_some(),
                            "expired": s.is_expired(now_unix),
                        })),
                        "source_path": k.source_path,
                    })
                })
                .collect();
            json!({
                "name": p.name,
                "kind": format!("{:?}", p.kind),
                "subscription": p.kind.is_subscription(),
                "base_url": p.base_url,
                "keys": keys,
            })
        })
        .collect();
    Json(json!({ "data": data })).into_response()
}

/// What the console's Fleet page reads: which store is authoritative,
/// and who else is currently serving traffic against it.
///
/// There are no roles, no terms and no quorum here, because there is no
/// consensus — every node is identical and the list is simply whoever has
/// heartbeated recently.
async fn fleet(State(state): State<Arc<AppState>>) -> Response {
    let mode = if state.file_managed {
        "file"
    } else {
        "managed"
    };
    let Some(store) = state.store.as_deref() else {
        return Json(json!({
            "mode": mode,
            "node": "local",
            "backend": "none",
            "version": 0,
            "live": 1,
            "shares": 1,
            "nodes": [],
        }))
        .into_response();
    };

    let window = state.liveness_window;
    let (nodes, reachable) = match store.peers(window).await {
        Ok(peers) => (peers, true),
        // The store being unreachable is worth showing, but it is not an
        // error for this endpoint: this node is still serving.
        Err(err) => {
            tracing::warn!(%err, "could not list the fleet");
            (Vec::new(), false)
        }
    };

    let now = router_store::backend::now_ms_for_tests();
    let listed: Vec<serde_json::Value> = nodes
        .iter()
        .map(|beat| {
            json!({
                "id": beat.id,
                "addr": beat.addr,
                "age_ms": beat.age(now).as_millis() as u64,
                "self": beat.id == store.node_id(),
            })
        })
        .collect();

    Json(json!({
        "mode": mode,
        "node": store.node_id(),
        "backend": store.describe(),
        "reachable": reachable,
        "version": store.version(),
        "live": store.live_nodes(),
        "shares": state.live_nodes(),
        "nodes": listed,
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
