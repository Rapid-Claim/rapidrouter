//! The dispatch pipeline: parse the inbound dialect, plan the route
//! (primary + fallbacks), and walk candidates until one serves. Same-
//! dialect traffic forwards raw bytes; cross-dialect traffic translates
//! through the internal OpenAI-shaped model — sync and streaming.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use router_core::breaker::Breaker;
use router_core::chat::ChatRequest;
use router_core::config::{AuthMode, ProviderKind, RetryOn};
use router_core::quota;

use crate::refresh;
use router_core::eventstream::EventStreamParser;
use router_core::router::{KeyRuntime, ResolvedRoute, RoutePlan};
use router_core::sse::SseParser;
use router_core::vkey::{self, VkDeny, VkRuntime};
use router_core::{ErrorClass, GatewayError, clock, json};
use router_providers::{Dialect, InboundStream, RenderTarget, UpstreamStream};
use serde_json::Value;

use crate::AppState;
use crate::usage::{self, TokenUsage, UsageHook};

/// Enforce a virtual key's scope, rate limits, and budget — after model
/// extraction, before any upstream work.
fn vk_gate(vk: &VkRuntime, requested: &str, plan: &RoutePlan) -> Result<(), GatewayError> {
    let resolved = plan
        .targets
        .first()
        .map(|r| format!("{}/{}", r.provider.name, r.upstream_model));
    if !vk.allows_model(requested, resolved.as_deref()) {
        return Err(GatewayError::new(
            ErrorClass::Permission,
            format!(
                "virtual key `{}` does not allow model `{requested}`",
                vk.def.name
            ),
        )
        .with_param("model"));
    }
    let ordinal = vk.period_ordinal(vkey::unix_now_ms());
    match vk.admit(clock::now_ms(), ordinal) {
        Ok(()) => Ok(()),
        Err(VkDeny::RateLimited { .. }) => Err(GatewayError::new(
            ErrorClass::RateLimited,
            format!("virtual key `{}` rate limit exceeded", vk.def.name),
        )),
        Err(VkDeny::BudgetExhausted) => Err(GatewayError::new(
            ErrorClass::InsufficientQuota,
            format!(
                "virtual key `{}` has exhausted its budget for this period",
                vk.def.name
            ),
        )),
    }
}

/// Which credential served a request, carried from the attempt to the
/// meter as a response *extension* rather than a header.
///
/// It has to travel with the response because the token cost is only
/// known once the body is done — but it must not travel to the client: a
/// key name is operator-facing, and naming the credential that served you
/// in a response header hands every caller a map of the pool.
#[derive(Clone)]
pub struct SeatUsed {
    pub provider: Arc<router_core::router::ProviderRuntime>,
    pub key: String,
}

fn meter(response: Response, mut hook: UsageHook, dialect: Dialect, stream: bool) -> Response {
    hook.provider = response
        .headers()
        .get("x-rapid-provider")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    hook.model = response
        .headers()
        .get("x-rapid-model")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    hook.attempts = response
        .headers()
        .get("x-rapid-attempts")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    hook.overhead_us = response
        .headers()
        .get("x-rapid-overhead-us")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or_default();
    hook.stream = stream;
    hook.seat = response.extensions().get::<SeatUsed>().cloned();
    usage::meter_response(response, hook, dialect)
}

/// Attribute a request that failed before (or instead of) serving a
/// response body.
///
/// The parameter list is long because a usage record genuinely needs all
/// of it and this is the one place that assembles one for a failure;
/// bundling them into a struct here would only move the same fields
/// behind a name that adds nothing.
#[allow(clippy::too_many_arguments)]
fn record_failure(
    state: &AppState,
    vk: &Option<Arc<VkRuntime>>,
    endpoint: &'static str,
    requested: &str,
    err: &GatewayError,
    started: Instant,
    headers: &HeaderMap,
    input: Option<&[u8]>,
) {
    let mut hook = match input {
        Some(body) => build_hook_with_input(state, vk, endpoint, requested, headers, started, body),
        None => build_hook(state, vk, endpoint, requested, headers, started),
    };
    hook.provider = err.provider.clone().unwrap_or_default();
    hook.complete(err.class.http_status(), TokenUsage::default());
}

/// A hook that also remembers the request body, for the log drawer.
fn build_hook_with_input(
    state: &AppState,
    vk: &Option<Arc<VkRuntime>>,
    endpoint: &'static str,
    requested: &str,
    headers: &HeaderMap,
    started: Instant,
    input: &[u8],
) -> UsageHook {
    let mut hook = build_hook(state, vk, endpoint, requested, headers, started);
    // Only materialise the body when something will store it.
    if state.usage.capture_limit_for(200) > 0 {
        hook.input_body = Some(String::from_utf8_lossy(input).into_owned());
    }
    hook
}

fn build_hook(
    state: &AppState,
    vk: &Option<Arc<VkRuntime>>,
    endpoint: &'static str,
    requested: &str,
    headers: &HeaderMap,
    started: Instant,
) -> UsageHook {
    UsageHook {
        pipeline: state.usage.clone(),
        vkey: vk.clone(),
        pricing: state.pricing.load().as_ref().clone(),
        events: Some(state.events.clone()),
        // Stamped onto the request by the `request_id` middleware, so the
        // usage record and the client's `x-request-id` are the same value.
        request_id: headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
        endpoint,
        requested: requested.to_owned(),
        provider: String::new(),
        model: String::new(),
        stream: false,
        attempts: 0,
        started,
        overhead_us: 0,
        tag: headers
            .get("x-rapid-tag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        seat: None,
        input_body: None,
    }
}

/// Cap for buffered (non-streaming) translated response bodies.
const MAX_TRANSLATED_RESPONSE: usize = 64 * 1024 * 1024;

/// OpenAI-dialect relay endpoints with no translation surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Completions,
    Embeddings,
    AudioSpeech,
    ImagesGenerations,
}

impl Endpoint {
    fn upstream_path(self) -> &'static str {
        match self {
            Self::Completions => "/completions",
            Self::Embeddings => "/embeddings",
            Self::AudioSpeech => "/audio/speech",
            Self::ImagesGenerations => "/images/generations",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Completions => "completions",
            Self::Embeddings => "embeddings",
            Self::AudioSpeech => "audio_speech",
            Self::ImagesGenerations => "images_generations",
        }
    }
}

/// A parsed inbound chat request in whatever dialect it arrived.
pub enum InboundChat {
    OpenAi {
        body: Bytes,
        span: Option<(usize, usize)>,
        model: String,
        stream: bool,
    },
    Anthropic {
        value: Value,
        model: String,
        stream: bool,
    },
    Gemini {
        value: Value,
        model: String,
        stream: bool,
    },
}

impl InboundChat {
    /// The request as the caller sent it, for the log drawer.
    pub fn raw_body(&self) -> String {
        match self {
            Self::OpenAi { body, .. } => String::from_utf8_lossy(body).into_owned(),
            Self::Anthropic { value, .. } | Self::Gemini { value, .. } => value.to_string(),
        }
    }

    pub fn from_openai(body: Bytes) -> Result<Self, GatewayError> {
        if let Some(probe) = json::probe(&body) {
            let stream = probe.stream == Some(true);
            return Ok(Self::OpenAi {
                body,
                span: Some(probe.model_span),
                model: probe.model,
                stream,
            });
        }
        #[derive(serde::Deserialize)]
        struct Minimal {
            model: String,
            #[serde(default)]
            stream: bool,
        }
        let minimal: Minimal = serde_json::from_slice(&body).map_err(|err| {
            GatewayError::new(
                ErrorClass::InvalidRequest,
                format!("request body is not a valid request object: {err}"),
            )
        })?;
        Ok(Self::OpenAi {
            body,
            span: None,
            model: minimal.model,
            stream: minimal.stream,
        })
    }

    pub fn from_anthropic(body: &Bytes) -> Result<Self, GatewayError> {
        let value: Value = serde_json::from_slice(body).map_err(|err| {
            GatewayError::new(
                ErrorClass::InvalidRequest,
                format!("invalid request body: {err}"),
            )
        })?;
        let model = value["model"]
            .as_str()
            .ok_or_else(|| {
                GatewayError::new(ErrorClass::InvalidRequest, "`model` is required")
                    .with_param("model")
            })?
            .to_owned();
        let stream = value["stream"] == Value::Bool(true);
        Ok(Self::Anthropic {
            value,
            model,
            stream,
        })
    }

    pub fn from_gemini(body: &Bytes, model: String, stream: bool) -> Result<Self, GatewayError> {
        let value: Value = serde_json::from_slice(body).map_err(|err| {
            GatewayError::new(
                ErrorClass::InvalidRequest,
                format!("invalid request body: {err}"),
            )
        })?;
        Ok(Self::Gemini {
            value,
            model,
            stream,
        })
    }

    fn dialect(&self) -> Dialect {
        match self {
            Self::OpenAi { .. } => Dialect::OpenAi,
            Self::Anthropic { .. } => Dialect::Anthropic,
            Self::Gemini { .. } => Dialect::Gemini,
        }
    }

    fn model(&self) -> &str {
        match self {
            Self::OpenAi { model, .. }
            | Self::Anthropic { model, .. }
            | Self::Gemini { model, .. } => model,
        }
    }

    fn stream(&self) -> bool {
        match self {
            Self::OpenAi { stream, .. }
            | Self::Anthropic { stream, .. }
            | Self::Gemini { stream, .. } => *stream,
        }
    }

    /// Parse into the internal model, for cross-dialect targets.
    fn to_internal(&self) -> Result<ChatRequest, GatewayError> {
        match self {
            Self::OpenAi { body, .. } => ChatRequest::parse(body).map_err(|err| {
                GatewayError::new(
                    ErrorClass::InvalidRequest,
                    format!("invalid chat request: {err}"),
                )
            }),
            Self::Anthropic { value, .. } => {
                router_providers::anthropic::request_to_internal(value)
            }
            Self::Gemini {
                value,
                model,
                stream,
            } => router_providers::gemini::request_to_internal(value, model, *stream),
        }
    }

    /// Raw same-dialect passthrough body with the model rewritten.
    fn passthrough_body(&self, upstream_model: &str) -> Bytes {
        match self {
            Self::OpenAi { body, span, .. } => match span {
                Some(span) => json::splice_model(body, *span, upstream_model),
                None => rewrite_model_value(body, upstream_model),
            },
            Self::Anthropic { value, .. } => {
                let mut v = value.clone();
                v["model"] = Value::String(upstream_model.to_owned());
                Bytes::from(serde_json::to_vec(&v).expect("serializable"))
            }
            // Gemini carries the model in the URL; the body is untouched.
            Self::Gemini { value, .. } => {
                Bytes::from(serde_json::to_vec(value).expect("serializable"))
            }
        }
    }
}

fn rewrite_model_value(body: &Bytes, model: &str) -> Bytes {
    let mut value: Value = serde_json::from_slice(body).expect("validated by from_openai");
    value["model"] = Value::String(model.to_owned());
    Bytes::from(serde_json::to_vec(&value).expect("serializable"))
}

/// Time spent waiting on the upstream, carried via response extensions
/// so `finalize` can report gateway-added time rather than total time.
#[derive(Clone, Copy)]
struct UpstreamTime(std::time::Duration);

/// One upstream try, described precisely enough for the dispatch loop to
/// decide between forwarding, retrying, and advancing the chain.
enum AttemptOutcome {
    Serve(Response),
    Retry(GatewayError),
}

pub async fn handle_chat(
    state: Arc<AppState>,
    inbound: InboundChat,
    headers: HeaderMap,
    vk: Option<Arc<VkRuntime>>,
) -> Response {
    let started = Instant::now();
    let dialect = inbound.dialect();
    let requested = inbound.model().to_owned();
    let stream = inbound.stream();
    match run_chat(&state, &inbound, &headers, started, vk.as_deref()).await {
        Ok(response) => meter(
            response,
            build_hook_with_input(
                &state,
                &vk,
                "chat",
                &requested,
                &headers,
                started,
                inbound.raw_body().as_bytes(),
            ),
            dialect,
            stream,
        ),
        Err(err) => {
            record_failure(
                &state,
                &vk,
                "chat",
                &requested,
                &err,
                started,
                &headers,
                Some(inbound.raw_body().as_bytes()),
            );
            error_response_in(dialect, &err)
        }
    }
}

async fn run_chat(
    state: &AppState,
    inbound: &InboundChat,
    headers: &HeaderMap,
    started: Instant,
    vk: Option<&VkRuntime>,
) -> Result<Response, GatewayError> {
    let table = state.table.load();
    let plan = table.plan(inbound.model())?;
    if let Some(vk) = vk {
        vk_gate(vk, inbound.model(), &plan)?;
    }
    let in_dialect = inbound.dialect();
    let stream = inbound.stream();

    // Parsed lazily, at most once, only when some target needs translation.
    let mut internal: Option<ChatRequest> = None;

    let mut attempts: u32 = 0;
    let mut last_error: Option<GatewayError> = None;

    let n_targets = plan.targets.len();
    for (t_idx, route) in plan.targets.iter().enumerate() {
        let Some(out_dialect) = router_providers::wire_dialect(route.provider.kind) else {
            last_error = Some(
                GatewayError::new(
                    ErrorClass::InvalidRequest,
                    format!(
                        "provider `{}` ({:?}) is configured but its adapter is not available yet",
                        route.provider.name, route.provider.kind
                    ),
                )
                .with_param("model")
                .with_provider(&route.provider.name),
            );
            continue;
        };

        // Build the upstream body/path for this target (per-target:
        // chains may cross dialects). Capability errors (n>1, logprobs,
        // audio parts…) are the caller's to fix; surface immediately
        // rather than burning fallbacks.
        let (out_body, path, dropped, emulated) = if out_dialect == in_dialect {
            let body = inbound.passthrough_body(&route.upstream_model);
            let path =
                router_providers::passthrough_path(out_dialect, &route.upstream_model, stream);
            (body, path, Vec::new(), false)
        } else {
            let req = match &internal {
                Some(r) => r,
                None => internal.insert(inbound.to_internal()?),
            };
            let built = router_providers::build_outbound(
                out_dialect,
                req,
                &route.upstream_model,
                stream,
                route.provider.codex.as_ref(),
                route.provider.kind == ProviderKind::ClaudeSubscription,
            )?;
            (
                built.body,
                built.path,
                built.dropped_params,
                built.json_schema_emulated,
            )
        };
        for param in &dropped {
            metrics::counter!("rapid_dropped_params_total", "param" => param.clone(), "provider" => route.provider.name.clone()).increment(1);
            tracing::debug!(provider = %route.provider.name, param, "dropped unsupported parameter");
        }

        for a_idx in 0..plan.max_attempts_per_target {
            let is_last_candidate =
                t_idx + 1 == n_targets && a_idx + 1 == plan.max_attempts_per_target;
            let now = clock::now_ms();
            let Some(choice) = route.provider.admit_key(&route.upstream_model, now) else {
                // A subscription pool with every seat out of quota is rate
                // limited, not out of capacity. The distinction is what
                // the caller does next: a 503 invites an immediate retry,
                // a 429 tells them there is a window to wait for.
                last_error = Some(if route.provider.all_keys_benched(&route.upstream_model) {
                    GatewayError::new(
                        ErrorClass::RateLimited,
                        format!(
                            "every seat of provider `{}` is out of quota for model `{}`",
                            route.provider.name, route.upstream_model
                        ),
                    )
                    .with_provider(&route.provider.name)
                } else {
                    GatewayError::new(
                        ErrorClass::NoCapacity,
                        format!(
                            "no healthy key of provider `{}` for model `{}`",
                            route.provider.name, route.upstream_model
                        ),
                    )
                    .with_provider(&route.provider.name)
                });
                break;
            };
            let Ok(permit) = route.provider.semaphore.clone().try_acquire_owned() else {
                last_error = Some(
                    GatewayError::new(
                        ErrorClass::NoCapacity,
                        format!("provider `{}` is at max concurrency", route.provider.name),
                    )
                    .with_provider(&route.provider.name),
                );
                break;
            };

            attempts += 1;
            let breaker = route.provider.breaker_for(choice.key);
            // Renew a subscription seat that is about to expire, before
            // its token is put on a request. Doing it here rather than on
            // the 401 that would follow keeps the failure off the caller's
            // path; a seat that cannot renew simply carries on and is
            // handled reactively.
            if route.provider.kind.is_subscription()
                && let Some(key) = choice.key
            {
                refresh::refresh_if_stale(
                    &state.upstream,
                    &state.refreshes,
                    route.provider.kind,
                    key,
                    &key.source_path
                        .as_deref()
                        .map(|p| refresh::Persist::File(p.to_owned()))
                        .unwrap_or(refresh::Persist::None),
                    now_epoch_ms(),
                )
                .await;
            }
            let request = build_upstream_request(
                route,
                out_dialect,
                &path,
                headers,
                choice.key,
                out_body.clone(),
            )?;

            match attempt(
                state,
                route,
                request,
                breaker,
                choice.key,
                permit,
                &plan,
                is_last_candidate,
                TranslationCtx {
                    passthrough: out_dialect == in_dialect,
                    render: RenderTarget::Dialect(in_dialect),
                    out_dialect,
                    stream,
                    emulated,
                },
            )
            .await
            {
                AttemptOutcome::Serve(mut response) => {
                    finalize(&mut response, route, choice.key, attempts, started);
                    if emulated {
                        response
                            .headers_mut()
                            .insert("x-rapid-emulated", HeaderValue::from_static("json_schema"));
                    }
                    return Ok(response);
                }
                AttemptOutcome::Retry(err) => {
                    tracing::debug!(provider = %route.provider.name, %err, "attempt failed; retrying");
                    last_error = Some(err);
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        GatewayError::new(ErrorClass::NoCapacity, "no route candidates available")
    }))
}

#[derive(Clone, Copy)]
struct TranslationCtx {
    /// Raw forward: no translation on the response path.
    passthrough: bool,
    /// The shape the client is owed when translating.
    render: RenderTarget,
    out_dialect: Dialect,
    stream: bool,
    emulated: bool,
}

#[allow(clippy::too_many_arguments)]
async fn attempt(
    state: &AppState,
    route: &ResolvedRoute,
    request: http::Request<Body>,
    breaker: &Breaker,
    key: Option<&router_core::router::KeyRuntime>,
    permit: tokio::sync::OwnedSemaphorePermit,
    plan: &RoutePlan,
    is_last_candidate: bool,
    ctx: TranslationCtx,
) -> AttemptOutcome {
    let upstream_started = Instant::now();
    let result = state
        .upstream
        .send(&route.provider.name, request, route.provider.timeout)
        .await;
    let upstream_elapsed = upstream_started.elapsed();
    metrics::histogram!("rapid_upstream_duration_seconds", "provider" => route.provider.name.clone())
        .record(upstream_elapsed.as_secs_f64());

    match result {
        Err(err) => {
            breaker.record_failure(clock::now_ms());
            metrics::counter!("rapid_retries_total", "provider" => route.provider.name.clone())
                .increment(1);
            AttemptOutcome::Retry(err)
        }
        Ok(response) => {
            let status = response.status();
            let retryable = (status.as_u16() == 429 && plan.retry_on.contains(&RetryOn::Status429))
                || (status.is_server_error() && plan.retry_on.contains(&RetryOn::Status5xx));

            if status.is_server_error() || status.as_u16() == 429 {
                breaker.record_failure(clock::now_ms());
            } else {
                breaker.record_success(clock::now_ms());
            }

            // A subscription seat that is out of quota is out until the
            // provider's window rolls — minutes to days, not the breaker's
            // configured cooldown. Probing before then costs the caller a
            // retry attempt and earns another 429, so the seat is benched
            // for the window the provider itself reported.
            if route.provider.kind.is_subscription() {
                bench_exhausted_seat(route, breaker, key, response.headers(), status);

                // A seat whose credential was revoked or expired
                // out-of-band answers 401. Renewing it in place and
                // retrying is the difference between a transient blip and
                // a seat that stays broken until someone notices: the
                // proactive path only fires near a *known* expiry, and an
                // out-of-band revocation has no expiry to be near.
                if status.as_u16() == 401
                    && !is_last_candidate
                    && let Some(key) = key
                    && let Some(seat) = key.seat()
                    && let Some(path) = key.source_path.as_deref()
                    && refresh::refresh_now(
                        &state.upstream,
                        &state.refreshes,
                        route.provider.kind,
                        seat,
                        &refresh::Persist::File(path.to_owned()),
                        now_epoch_ms(),
                    )
                    .await
                {
                    metrics::counter!(
                        "rapid_seat_refresh_total",
                        "provider" => route.provider.name.clone(),
                    )
                    .increment(1);
                    return AttemptOutcome::Retry(
                        GatewayError::new(
                            ErrorClass::UpstreamError,
                            format!(
                                "seat credential of provider `{}` was renewed; retrying",
                                route.provider.name
                            ),
                        )
                        .with_provider(&route.provider.name),
                    );
                }
            }

            if retryable && !is_last_candidate {
                metrics::counter!("rapid_retries_total", "provider" => route.provider.name.clone())
                    .increment(1);
                return AttemptOutcome::Retry(
                    GatewayError::new(
                        ErrorClass::UpstreamError,
                        format!("provider `{}` returned {}", route.provider.name, status),
                    )
                    .with_provider(&route.provider.name)
                    .with_upstream_status(status.as_u16()),
                );
            }

            let mut served = if ctx.passthrough {
                AttemptOutcome::Serve(forward_response(response, permit))
            } else {
                translated_response(route, response, permit, ctx).await
            };
            if let AttemptOutcome::Serve(response) = &mut served {
                response
                    .extensions_mut()
                    .insert(UpstreamTime(upstream_elapsed));
            }
            served
        }
    }
}

/// Bench a subscription key for as long as the provider says, and record
/// what it told us about its remaining quota.
///
/// Only a `429` benches. The quota gauges are published on every response,
/// which is the point of doing this here: both providers report their
/// windows on *success* too, so an operator watches a pool approach the
/// edge instead of discovering it when traffic starts failing.
///
/// Header-only, deliberately. Both backends carry the reset window in a
/// header on the response that refuses the request — Anthropic in
/// `retry-after` (3311s on an exhausted 5h window) and Codex in
/// `x-codex-primary-reset-after-seconds` (380613s on an exhausted weekly
/// one), both verified live. Buffering an upstream body on the hot path to
/// learn the same number would cost every request to save a parse on the
/// rare one. [`quota::retry_after_body`] covers the nested-in-`error`
/// spelling for the paths that have the body in hand already.
fn bench_exhausted_seat(
    route: &ResolvedRoute,
    breaker: &Breaker,
    key: Option<&router_core::router::KeyRuntime>,
    headers: &HeaderMap,
    status: http::StatusCode,
) {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let quota = match route.provider.kind {
        ProviderKind::ClaudeSubscription => quota::anthropic_quota(header, now_epoch),
        _ => quota::codex_quota(header),
    };
    // Recorded on every subscription response, not only refusals: the
    // number an operator needs is how close a seat is to its ceiling, and
    // that is only visible while it is still serving.
    if let Some(key) = key {
        key.observe_quota(quota, clock::now_ms());
    }
    if let Some(peak) = quota.peak_utilization() {
        metrics::gauge!(
            "rapid_seat_quota_utilization",
            "provider" => route.provider.name.clone(),
        )
        .set(peak);
    }

    if status.as_u16() != 429 {
        return;
    }

    // `retry-after` when the provider sends one; otherwise the window the
    // rejecting quota view reported.
    let window = header("retry-after")
        .and_then(|v| quota::retry_after_header(&v))
        .or_else(|| {
            quota
                .primary
                .filter(|w| w.rejected)
                .and_then(|w| w.resets_in)
                .or_else(|| {
                    quota
                        .secondary
                        .filter(|w| w.rejected)
                        .and_then(|w| w.resets_in)
                })
        });

    let Some(window) = window else {
        // No window reported: leave the key to the breaker's own cooldown
        // rather than inventing a bench length.
        return;
    };
    let benched = quota::bench_for(window, fastrand::f64());
    breaker.bench_until(clock::now_ms() + benched.as_millis() as u64);
    metrics::gauge!(
        "rapid_seat_bench_seconds",
        "provider" => route.provider.name.clone(),
    )
    .set(benched.as_secs_f64());
    tracing::warn!(
        provider = %route.provider.name,
        seconds = benched.as_secs(),
        "subscription seat out of quota; benched until the provider's window rolls",
    );
}

/// Cross-dialect response handling: translate sync bodies whole, wrap
/// streams event-by-event, and map upstream error bodies into the
/// inbound dialect's error shape.
async fn translated_response(
    route: &ResolvedRoute,
    response: http::Response<hyper::body::Incoming>,
    permit: tokio::sync::OwnedSemaphorePermit,
    ctx: TranslationCtx,
) -> AttemptOutcome {
    let status = response.status();

    if !status.is_success() {
        let body = collect_capped(response.into_body(), 1024 * 1024)
            .await
            .unwrap_or_default();
        let message = extract_upstream_error(&body)
            .unwrap_or_else(|| format!("provider `{}` returned {status}", route.provider.name));
        let class = match status.as_u16() {
            400 | 404 | 413 | 422 => ErrorClass::InvalidRequest,
            401 | 403 => ErrorClass::UpstreamError, // our key, not the caller's fault
            429 => ErrorClass::RateLimited,
            _ => ErrorClass::UpstreamError,
        };
        let err = GatewayError::new(class, message)
            .with_provider(&route.provider.name)
            .with_upstream_status(status.as_u16());
        drop(permit);
        let mut response = error_response_for(ctx.render, &err);
        // Preserve the upstream's status; class mapping may coarsen it.
        *response.status_mut() = status;
        return AttemptOutcome::Serve(response);
    }

    if !ctx.stream {
        let body = match collect_capped(response.into_body(), MAX_TRANSLATED_RESPONSE).await {
            Ok(body) => body,
            Err(err) => {
                drop(permit);
                return AttemptOutcome::Serve(error_response_for(ctx.render, &err));
            }
        };
        drop(permit);
        let openai = match router_providers::response_to_openai(
            ctx.out_dialect,
            &body,
            &route.upstream_model,
            ctx.emulated,
        ) {
            Ok(v) => v,
            Err(err) => return AttemptOutcome::Serve(error_response_for(ctx.render, &err)),
        };
        let rendered = router_providers::render_for(ctx.render, &openai);
        return AttemptOutcome::Serve((StatusCode::OK, axum::Json(rendered)).into_response());
    }

    // Streaming: upstream SSE -> internal chunks -> inbound frames.
    let translator = UpstreamStream::new(ctx.out_dialect, &route.upstream_model, ctx.emulated);
    let formatter = InboundStream::new_for(ctx.render);
    let parser = WireParser::for_dialect(ctx.out_dialect);
    let body = translated_stream_body(response.into_body(), permit, parser, translator, formatter);
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .expect("static response parts");
    AttemptOutcome::Serve(response)
}

/// Upstream wire framing: SSE for most dialects, AWS event-stream for
/// Bedrock. Both produce the same event shape for the translators.
enum WireParser {
    Sse(SseParser),
    EventStream(EventStreamParser),
}

impl WireParser {
    fn for_dialect(dialect: Dialect) -> Self {
        match dialect {
            Dialect::Bedrock => Self::EventStream(EventStreamParser::new()),
            _ => Self::Sse(SseParser::new()),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> Vec<router_core::sse::SseEvent> {
        match self {
            Self::Sse(parser) => parser.push(chunk),
            Self::EventStream(parser) => parser.push(chunk),
        }
    }
}

fn translated_stream_body(
    upstream: hyper::body::Incoming,
    permit: tokio::sync::OwnedSemaphorePermit,
    parser: WireParser,
    translator: UpstreamStream,
    formatter: InboundStream,
) -> Body {
    struct StreamState {
        upstream: hyper::body::Incoming,
        parser: WireParser,
        translator: UpstreamStream,
        formatter: InboundStream,
        done: bool,
        _permit: tokio::sync::OwnedSemaphorePermit,
    }

    let state = StreamState {
        upstream,
        parser,
        translator,
        formatter,
        done: false,
        _permit: permit,
    };

    let stream = futures_util::stream::unfold(state, |mut st| async move {
        if st.done {
            return None;
        }
        loop {
            match st.upstream.frame().await {
                Some(Ok(frame)) => {
                    let Some(data) = frame.data_ref() else {
                        continue;
                    };
                    let mut out = String::new();
                    for event in st.parser.push(data) {
                        for chunk in st.translator.on_event(&event) {
                            for frame in st.formatter.on_chunk(&chunk) {
                                out.push_str(&frame);
                            }
                        }
                    }
                    if !out.is_empty() {
                        return Some((Ok::<_, std::convert::Infallible>(Bytes::from(out)), st));
                    }
                }
                Some(Err(err)) => {
                    tracing::warn!(%err, "upstream stream failed mid-flight");
                    st.done = true;
                    let tail: String = st.formatter.finish().concat();
                    return Some((Ok(Bytes::from(tail)), st));
                }
                None => {
                    st.done = true;
                    let tail: String = st.formatter.finish().concat();
                    if tail.is_empty() {
                        return None;
                    }
                    return Some((Ok(Bytes::from(tail)), st));
                }
            }
        }
    });
    Body::from_stream(stream)
}

async fn collect_capped(body: hyper::body::Incoming, cap: usize) -> Result<Bytes, GatewayError> {
    let limited = http_body_util::Limited::new(body, cap);
    match limited.collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(_) => Err(GatewayError::new(
            ErrorClass::UpstreamError,
            "upstream response exceeded the translation size cap",
        )),
    }
}

fn extract_upstream_error(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    for path in [&v["error"]["message"], &v["message"], &v["error"]] {
        if let Some(s) = path.as_str() {
            return Some(s.to_owned());
        }
    }
    None
}

fn finalize(
    response: &mut Response,
    route: &ResolvedRoute,
    key: Option<&router_core::router::KeyRuntime>,
    attempts: u32,
    started: Instant,
) {
    // Which credential served, for the token-limit debit once the body is
    // done. An extension, not a header — see [`SeatUsed`].
    if let Some(key) = key {
        response.extensions_mut().insert(SeatUsed {
            provider: route.provider.clone(),
            key: key.name.clone(),
        });
    }
    // Gateway-added time: total elapsed minus the upstream wait of the
    // serving attempt. (Earlier failed attempts' upstream waits count
    // against us — retries are our choice.)
    let upstream = response
        .extensions()
        .get::<UpstreamTime>()
        .map(|t| t.0)
        .unwrap_or_default();
    let overhead = started.elapsed().saturating_sub(upstream);

    let headers = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&route.provider.name) {
        headers.insert("x-rapid-provider", v);
    }
    if let Ok(v) = HeaderValue::from_str(&route.upstream_model) {
        headers.insert("x-rapid-model", v);
    }
    if let Ok(v) = HeaderValue::from_str(&attempts.to_string()) {
        headers.insert("x-rapid-attempts", v);
    }
    if let Ok(v) = HeaderValue::from_str(&overhead.as_micros().to_string()) {
        headers.insert("x-rapid-overhead-us", v);
    }
    metrics::histogram!("rapid_gateway_overhead_seconds").record(overhead.as_secs_f64());
    record_request_metrics(&route.provider.name, response.status());
}

pub(crate) fn build_upstream_request(
    route: &ResolvedRoute,
    out_dialect: Dialect,
    path: &str,
    inbound: &HeaderMap,
    key: Option<&KeyRuntime>,
    body: Bytes,
) -> Result<http::Request<Body>, GatewayError> {
    let base = route.provider.base_url.as_deref().ok_or_else(|| {
        GatewayError::new(
            ErrorClass::UpstreamError,
            format!("provider `{}` has no base_url", route.provider.name),
        )
    })?;

    // Azure keeps the OpenAI dialect but addresses deployments:
    // {endpoint}/openai/deployments/{deployment}{path}?api-version=…
    let url = if let Some(azure) = &route.provider.azure {
        let deployment = azure
            .deployments
            .get(&route.upstream_model)
            .map(String::as_str)
            .unwrap_or(&route.upstream_model);
        format!(
            "{}/openai/deployments/{deployment}{path}?api-version={}",
            base.trim_end_matches('/'),
            azure.api_version
        )
    } else if let Some(vertex) = &route.provider.vertex {
        // Vertex serves the Gemini dialect under project/location paths;
        // reuse the action (`generateContent` / `streamGenerateContent…`)
        // from the dialect path.
        let action = path
            .rsplit_once(':')
            .map(|(_, a)| a)
            .unwrap_or("generateContent");
        format!(
            "{}/v1/projects/{}/locations/{}/publishers/google/models/{}:{action}",
            base.trim_end_matches('/'),
            vertex.project,
            vertex.location,
            route.upstream_model
        )
    } else {
        format!("{}{}", base.trim_end_matches('/'), path)
    };

    let mut builder = http::Request::post(&url).header(header::CONTENT_TYPE, "application/json");
    if let Some(accept) = inbound.get(header::ACCEPT) {
        builder = builder.header(header::ACCEPT, accept);
    }

    if route.provider.auth == AuthMode::Key {
        let key = key.ok_or_else(|| {
            GatewayError::new(
                ErrorClass::NotFound,
                format!(
                    "no key of provider `{}` serves model `{}`",
                    route.provider.name, route.upstream_model
                ),
            )
            .with_param("model")
        })?;
        let sensitive = |value: String| -> Result<HeaderValue, GatewayError> {
            let mut v = HeaderValue::from_str(&value).map_err(|_| {
                GatewayError::new(ErrorClass::UpstreamError, "provider key is not header-safe")
            })?;
            v.set_sensitive(true);
            Ok(v)
        };

        // Read the credential ONCE per request. A subscription seat may be
        // renewed underneath us, and a request that signed its first byte
        // with one token must not finish with another.
        let token = key.token();
        if route.provider.kind == ProviderKind::ClaudeSubscription {
            // The subscription token is a bearer, not an `x-api-key`, and
            // is only admitted as an inference credential with the OAuth
            // beta flag. The caller's own betas ride along.
            let mut betas = vec![router_providers::subscription::CLAUDE_OAUTH_BETA.to_owned()];
            if let Some(requested) = inbound.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
                betas.extend(
                    requested
                        .split(',')
                        .map(str::trim)
                        .filter(|b| {
                            !b.is_empty() && *b != router_providers::subscription::CLAUDE_OAUTH_BETA
                        })
                        .map(str::to_owned),
                );
            }
            builder = builder
                .header(
                    header::AUTHORIZATION,
                    sensitive(format!("Bearer {}", token.expose()))?,
                )
                .header("anthropic-version", router_providers::ANTHROPIC_VERSION)
                .header("anthropic-beta", betas.join(","));
        } else if route.provider.kind == ProviderKind::CodexSubscription {
            let seat = key.seat().map(|s| s.current());
            let account_id = seat
                .as_ref()
                .and_then(|s| s.account_id.as_deref())
                .unwrap_or_default();
            let settings = route.provider.codex.clone().unwrap_or_default();
            let session_id = uuid::Uuid::now_v7().simple().to_string();
            for (name, value) in router_providers::subscription::codex_headers(
                token.expose(),
                account_id,
                &settings.version,
                &session_id,
            ) {
                // Content-type is already set above; the rest are the CLI's.
                if name == "content-type" {
                    continue;
                }
                builder = builder.header(name, sensitive(value)?);
            }
        } else if route.provider.azure.is_some() {
            builder = builder.header("api-key", sensitive(token.expose().to_owned())?);
        } else if route.provider.vertex.is_some() {
            // OAuth access token (or express-mode key) as a bearer.
            builder = builder.header(
                header::AUTHORIZATION,
                sensitive(format!("Bearer {}", token.expose()))?,
            );
        } else if let Some(bedrock) = &route.provider.bedrock {
            // SigV4: the signature covers host, date, and the payload hash.
            let uri: http::Uri = url
                .parse()
                .map_err(|_| GatewayError::new(ErrorClass::UpstreamError, "invalid bedrock url"))?;
            let host = uri
                .authority()
                .map(|a| a.as_str().to_owned())
                .unwrap_or_default();
            let amz_date = router_providers::sigv4::amz_date_now();
            let signature =
                router_providers::sigv4::sign(&router_providers::sigv4::SigningParams {
                    access_key_id: &bedrock.access_key_id,
                    secret_access_key: token.expose(),
                    region: &bedrock.region,
                    service: "bedrock",
                    amz_date: &amz_date,
                    host: &host,
                    method: "POST",
                    canonical_path: uri.path(),
                    query: uri.query().unwrap_or(""),
                    payload: &body,
                });
            builder = builder
                .header("host", host)
                .header("x-amz-date", amz_date)
                .header("x-amz-content-sha256", signature.amz_content_sha256)
                .header(header::AUTHORIZATION, sensitive(signature.authorization)?);
        } else {
            match out_dialect {
                Dialect::OpenAi => {
                    builder = builder.header(
                        header::AUTHORIZATION,
                        sensitive(format!("Bearer {}", token.expose()))?,
                    );
                }
                Dialect::Anthropic => {
                    builder = builder
                        .header("x-api-key", sensitive(token.expose().to_owned())?)
                        .header("anthropic-version", router_providers::ANTHROPIC_VERSION);
                    // Anthropic beta features requested by the client ride along.
                    if let Some(beta) = inbound.get("anthropic-beta") {
                        builder = builder.header("anthropic-beta", beta);
                    }
                }
                Dialect::Gemini => {
                    builder =
                        builder.header("x-goog-api-key", sensitive(token.expose().to_owned())?);
                }
                Dialect::Bedrock | Dialect::CodexResponses => {
                    unreachable!("handled above by provider kind")
                }
            }
        }
    }

    builder.body(Body::from(body)).map_err(|e| {
        GatewayError::new(
            ErrorClass::UpstreamError,
            format!("bad upstream request: {e}"),
        )
    })
}

// ---------------------------------------------------------------------------
// The OpenAI-only relay endpoints (completions, embeddings)
// ---------------------------------------------------------------------------

/// Send one minimal request through a named credential and report what
/// the provider said.
///
/// This is the console's "check now": a seat that has served no traffic
/// has never reported its plan windows, so the only way to know its
/// state is to ask. The request is deliberately the smallest valid one
/// for the dialect (one token, no streaming) and it goes through the
/// same builder as real traffic, so what it exercises is what production
/// exercises — base URL, auth, refresh, headers and all.
///
/// The reply's quota headers are recorded exactly as a real response's
/// would be, so a probe also refreshes the windows the console draws.
pub(crate) async fn probe_key(
    state: &AppState,
    provider: Arc<router_core::router::ProviderRuntime>,
    key_name: &str,
    model: &str,
) -> ProbeOutcome {
    let route = ResolvedRoute {
        provider: provider.clone(),
        upstream_model: model.to_owned(),
    };
    let key = provider.keys.iter().find(|k| k.name == key_name);
    let Some(dialect) = router_providers::wire_dialect(provider.kind) else {
        return ProbeOutcome {
            status: "unreachable".into(),
            detail: format!("`{}` has no wire dialect to probe", provider.name),
            http_status: None,
        };
    };
    let (path, body) = match dialect {
        Dialect::Anthropic => (
            "/v1/messages",
            serde_json::json!({
                "model": model,
                "max_tokens": 1,
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        ),
        Dialect::Gemini => (
            "/v1beta/models:generateContent",
            serde_json::json!({ "contents": [{ "parts": [{ "text": "hi" }] }] }),
        ),
        _ => (
            "/chat/completions",
            serde_json::json!({
                "model": model,
                "max_tokens": 1,
                "messages": [{ "role": "user", "content": "hi" }],
            }),
        ),
    };

    let empty = HeaderMap::new();
    let request = match build_upstream_request(
        &route,
        dialect,
        path,
        &empty,
        key,
        Bytes::from(body.to_string()),
    ) {
        Ok(request) => request,
        Err(err) => {
            return ProbeOutcome {
                status: "unreachable".into(),
                detail: err.to_string(),
                http_status: None,
            };
        }
    };

    let result = state
        .upstream
        .send(&provider.name, request, provider.timeout)
        .await;

    match result {
        Err(err) => ProbeOutcome {
            status: "unreachable".into(),
            detail: err.to_string(),
            http_status: None,
        },
        Ok(response) => {
            let http_status = response.status();
            let headers = response.headers().clone();
            // Record the windows exactly as a real response would, and
            // bench the seat if the provider says it is out — a probe
            // that learns a seat is exhausted should leave the router
            // knowing it too, not just the operator.
            let breaker = provider.breaker_for(key);
            bench_exhausted_seat(&route, breaker, key, &headers, http_status);

            let status = match http_status.as_u16() {
                429 => "rate_limited",
                401 | 403 => "unauthorized",
                s if s >= 500 => "provider_error",
                s if s >= 400 => "rejected",
                _ => "ok",
            };
            let detail = if http_status.is_success() {
                String::new()
            } else {
                let body = axum::body::to_bytes(Body::new(response.into_body()), 8192)
                    .await
                    .unwrap_or_default();
                extract_upstream_error(&body).unwrap_or_else(|| http_status.to_string())
            };
            ProbeOutcome {
                status: status.into(),
                detail,
                http_status: Some(http_status.as_u16()),
            }
        }
    }
}

/// What a probe learned about one credential.
pub(crate) struct ProbeOutcome {
    pub status: String,
    pub detail: String,
    pub http_status: Option<u16>,
}

pub async fn handle_relay(
    state: Arc<AppState>,
    endpoint: Endpoint,
    headers: HeaderMap,
    body: Bytes,
    vk: Option<Arc<VkRuntime>>,
) -> Response {
    let started = Instant::now();
    let requested = json::probe(&body)
        .map(|p| p.model)
        .unwrap_or_else(|| "unknown".to_owned());
    match run_relay(&state, endpoint, &headers, body, started, vk.as_deref()).await {
        Ok(response) => meter(
            response,
            build_hook(&state, &vk, endpoint.name(), &requested, &headers, started),
            Dialect::OpenAi,
            false,
        ),
        Err(err) => {
            record_failure(
                &state,
                &vk,
                endpoint.name(),
                &requested,
                &err,
                started,
                &headers,
                None,
            );
            error_response(&err)
        }
    }
}

async fn run_relay(
    state: &AppState,
    endpoint: Endpoint,
    headers: &HeaderMap,
    body: Bytes,
    started: Instant,
    vk: Option<&VkRuntime>,
) -> Result<Response, GatewayError> {
    let (model, span) = match json::probe(&body) {
        Some(probe) => (probe.model, Some(probe.model_span)),
        None => {
            #[derive(serde::Deserialize)]
            struct Minimal {
                model: String,
            }
            let minimal: Minimal = serde_json::from_slice(&body).map_err(|err| {
                GatewayError::new(
                    ErrorClass::InvalidRequest,
                    format!("request body is not a valid request object: {err}"),
                )
            })?;
            (minimal.model, None)
        }
    };

    let table = state.table.load();
    let plan = table.plan(&model)?;
    if let Some(vk) = vk {
        vk_gate(vk, &model, &plan)?;
    }
    let mut attempts = 0u32;
    let mut last_error: Option<GatewayError> = None;

    let n_targets = plan.targets.len();
    for (t_idx, route) in plan.targets.iter().enumerate() {
        if router_providers::wire_dialect(route.provider.kind) != Some(Dialect::OpenAi) {
            last_error = Some(
                GatewayError::new(
                    ErrorClass::InvalidRequest,
                    format!(
                        "`{}` is only available for OpenAI-compatible providers",
                        endpoint.upstream_path()
                    ),
                )
                .with_param("model")
                .with_provider(&route.provider.name),
            );
            continue;
        }
        for a_idx in 0..plan.max_attempts_per_target {
            let is_last = t_idx + 1 == n_targets && a_idx + 1 == plan.max_attempts_per_target;
            let now = clock::now_ms();
            let Some(choice) = route.provider.admit_key(&route.upstream_model, now) else {
                last_error = Some(
                    GatewayError::new(ErrorClass::NoCapacity, "no healthy key")
                        .with_provider(&route.provider.name),
                );
                break;
            };
            let Ok(permit) = route.provider.semaphore.clone().try_acquire_owned() else {
                last_error = Some(
                    GatewayError::new(ErrorClass::NoCapacity, "provider at max concurrency")
                        .with_provider(&route.provider.name),
                );
                break;
            };
            attempts += 1;
            let breaker = route.provider.breaker_for(choice.key);
            let upstream_body = match span {
                Some(span) => json::splice_model(&body, span, &route.upstream_model),
                None => rewrite_model_value(&body, &route.upstream_model),
            };
            let request = build_upstream_request(
                route,
                Dialect::OpenAi,
                endpoint.upstream_path(),
                headers,
                choice.key,
                upstream_body,
            )?;
            let ctx = TranslationCtx {
                passthrough: true,
                render: RenderTarget::Dialect(Dialect::OpenAi),
                out_dialect: Dialect::OpenAi,
                stream: false,
                emulated: false,
            };
            match attempt(
                state, route, request, breaker, choice.key, permit, &plan, is_last, ctx,
            )
            .await
            {
                AttemptOutcome::Serve(mut response) => {
                    finalize(&mut response, route, choice.key, attempts, started);
                    return Ok(response);
                }
                AttemptOutcome::Retry(err) => last_error = Some(err),
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        GatewayError::new(ErrorClass::NoCapacity, "no route candidates available")
    }))
}

// ---------------------------------------------------------------------------
// Streaming media and file relays
// ---------------------------------------------------------------------------

const MULTIPART_ROUTE_PREFIX_CAP: usize = 1024 * 1024;

/// Relay a multipart request without collecting the upload. For official
/// SDKs, the small `model` form field is discovered in the multipart
/// prefix. Callers can always provide `x-rapid-model` explicitly.
pub async fn handle_stream_relay(
    state: Arc<AppState>,
    path: &'static str,
    endpoint: &'static str,
    headers: HeaderMap,
    body: Body,
    vk: Option<Arc<VkRuntime>>,
    needs_model: bool,
) -> Response {
    let started = Instant::now();
    let model_header = headers
        .get("x-rapid-model")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let (model, body) = if needs_model && model_header.is_none() {
        match read_multipart_model_prefix(body).await {
            Ok(parts) => parts,
            Err(err) => {
                record_failure(
                    &state, &vk, endpoint, "unknown", &err, started, &headers, None,
                );
                return error_response(&err);
            }
        }
    } else {
        (model_header.unwrap_or_default(), body)
    };
    let requested = if model.is_empty() {
        headers
            .get("x-rapid-provider")
            .and_then(|v| v.to_str().ok())
            .map(|p| format!("{p}/*"))
            .unwrap_or_else(|| "default/*".to_owned())
    } else {
        model.clone()
    };
    match run_stream_relay(
        &state,
        http::Method::POST,
        path,
        &headers,
        body,
        &model,
        vk.as_deref(),
    )
    .await
    {
        Ok(response) => meter(
            response,
            build_hook(&state, &vk, endpoint, &requested, &headers, started),
            Dialect::OpenAi,
            false,
        ),
        Err(err) => {
            record_failure(
                &state, &vk, endpoint, &requested, &err, started, &headers, None,
            );
            error_response(&err)
        }
    }
}

/// Relay a provider-scoped GET without a request body.
pub async fn handle_provider_relay(
    state: Arc<AppState>,
    path: &str,
    endpoint: &'static str,
    headers: HeaderMap,
    vk: Option<Arc<VkRuntime>>,
) -> Response {
    let started = Instant::now();
    let provider = headers
        .get("x-rapid-provider")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let requested = if provider.is_empty() {
        "default/*".to_owned()
    } else {
        format!("{provider}/*")
    };
    match run_stream_relay(
        &state,
        http::Method::GET,
        path,
        &headers,
        Body::empty(),
        "",
        vk.as_deref(),
    )
    .await
    {
        Ok(response) => meter(
            response,
            build_hook(&state, &vk, endpoint, &requested, &headers, started),
            Dialect::OpenAi,
            false,
        ),
        Err(err) => {
            record_failure(
                &state, &vk, endpoint, &requested, &err, started, &headers, None,
            );
            error_response(&err)
        }
    }
}

async fn read_multipart_model_prefix(mut body: Body) -> Result<(String, Body), GatewayError> {
    let mut chunks = Vec::<Bytes>::new();
    let mut prefix = Vec::new();
    loop {
        match body.frame().await {
            Some(Ok(frame)) => {
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                prefix.extend_from_slice(&data);
                chunks.push(data);
                if let Some(model) = multipart_field(&prefix, "model") {
                    let replay =
                        futures_util::stream::iter(chunks.into_iter().map(Ok::<_, axum::Error>))
                            .chain(body.into_data_stream());
                    return Ok((model, Body::from_stream(replay)));
                }
                if prefix.len() > MULTIPART_ROUTE_PREFIX_CAP {
                    return Err(GatewayError::new(
                        ErrorClass::InvalidRequest,
                        "multipart model was not found in the first 1 MiB; send x-rapid-model",
                    )
                    .with_param("model"));
                }
            }
            Some(Err(err)) => {
                return Err(GatewayError::new(
                    ErrorClass::InvalidRequest,
                    format!("multipart upload failed while reading route metadata: {err}"),
                ));
            }
            None => {
                return Err(GatewayError::new(
                    ErrorClass::InvalidRequest,
                    "multipart request is missing the `model` field",
                )
                .with_param("model"));
            }
        }
    }
}

fn multipart_field(prefix: &[u8], field: &str) -> Option<String> {
    let needle = format!("name=\"{field}\"");
    let pos = memchr::memmem::find(prefix, needle.as_bytes())?;
    let rest = &prefix[pos + needle.len()..];
    let start = memchr::memmem::find(rest, b"\r\n\r\n")? + 4;
    let value = &rest[start..];
    let end = memchr::memmem::find(value, b"\r\n")?;
    let value = std::str::from_utf8(&value[..end]).ok()?.trim();
    (!value.is_empty() && value.len() <= 512).then(|| value.to_owned())
}

async fn run_stream_relay(
    state: &AppState,
    method: http::Method,
    path: &str,
    headers: &HeaderMap,
    body: Body,
    model: &str,
    vk: Option<&VkRuntime>,
) -> Result<Response, GatewayError> {
    let table = state.table.load();
    let (provider, upstream_model) = if !model.is_empty() {
        let plan = table.plan(model)?;
        if let Some(vk) = vk {
            vk_gate(vk, model, &plan)?;
        }
        let route = plan.targets.first().ok_or_else(|| {
            GatewayError::new(ErrorClass::NotFound, "no media route candidates available")
        })?;
        if router_providers::wire_dialect(route.provider.kind) != Some(Dialect::OpenAi) {
            return Err(GatewayError::new(
                ErrorClass::InvalidRequest,
                "this relay endpoint requires an OpenAI-compatible provider",
            )
            .with_provider(&route.provider.name));
        }
        (route.provider.clone(), route.upstream_model.clone())
    } else {
        let selected = headers
            .get("x-rapid-provider")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut candidates = table
            .providers()
            .filter(|p| router_providers::wire_dialect(p.kind) == Some(Dialect::OpenAi))
            .filter(|p| selected.as_ref().is_none_or(|name| p.name == *name));
        let provider = candidates.next().cloned().ok_or_else(|| {
            GatewayError::new(
                ErrorClass::NotFound,
                "no OpenAI-compatible provider available",
            )
        })?;
        if selected.is_none() && candidates.next().is_some() {
            return Err(GatewayError::new(
                ErrorClass::InvalidRequest,
                "multiple providers are available; send x-rapid-provider",
            ));
        }
        let scope = format!("{}/*", provider.name);
        if let Some(vk) = vk {
            if !vk.allows_model(&scope, Some(&scope)) {
                return Err(GatewayError::new(
                    ErrorClass::Permission,
                    "virtual key does not allow this provider",
                ));
            }
            vk.admit(clock::now_ms(), vk.period_ordinal(vkey::unix_now_ms()))
                .map_err(|deny| {
                    GatewayError::new(
                        if deny == VkDeny::BudgetExhausted {
                            ErrorClass::InsufficientQuota
                        } else {
                            ErrorClass::RateLimited
                        },
                        "virtual key limit exceeded",
                    )
                })?;
        }
        (provider, String::new())
    };

    let choice = provider
        .admit_key(&upstream_model, clock::now_ms())
        .ok_or_else(|| GatewayError::new(ErrorClass::NoCapacity, "no healthy provider key"))?;
    let permit = provider
        .semaphore
        .clone()
        .try_acquire_owned()
        .map_err(|_| GatewayError::new(ErrorClass::NoCapacity, "provider at max concurrency"))?;
    let base = provider
        .base_url
        .as_deref()
        .ok_or_else(|| GatewayError::new(ErrorClass::UpstreamError, "provider has no base_url"))?;
    let url = format!("{}{}", base.trim_end_matches('/'), path);
    let mut builder = http::Request::builder().method(method).uri(url);
    for name in [header::CONTENT_TYPE, header::ACCEPT] {
        if let Some(value) = headers.get(&name) {
            builder = builder.header(name, value);
        }
    }
    if provider.auth == AuthMode::Key
        && let Some(key) = choice.key
    {
        let token = key.token();
        let mut auth = if provider.azure.is_some() {
            HeaderValue::from_str(token.expose()).map_err(|_| {
                GatewayError::new(
                    ErrorClass::UpstreamError,
                    "provider key is not a valid header",
                )
            })?
        } else {
            HeaderValue::from_str(&format!("Bearer {}", token.expose())).map_err(|_| {
                GatewayError::new(
                    ErrorClass::UpstreamError,
                    "provider key is not a valid header",
                )
            })?
        };
        auth.set_sensitive(true);
        builder = if provider.azure.is_some() {
            builder.header("api-key", auth)
        } else {
            builder.header(header::AUTHORIZATION, auth)
        };
    }
    let request = builder.body(body).map_err(|err| {
        GatewayError::new(
            ErrorClass::InvalidRequest,
            format!("bad relay request: {err}"),
        )
    })?;
    let response = state
        .upstream
        .send(&provider.name, request, provider.timeout)
        .await?;
    let mut response = forward_response(response, permit);
    response.headers_mut().insert(
        "x-rapid-provider",
        HeaderValue::from_str(&provider.name).expect("validated provider name"),
    );
    if !upstream_model.is_empty()
        && let Ok(value) = HeaderValue::from_str(&upstream_model)
    {
        response.headers_mut().insert("x-rapid-model", value);
    }
    response
        .headers_mut()
        .insert("x-rapid-attempts", HeaderValue::from_static("1"));
    Ok(response)
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/// Hop-by-hop headers that must not be forwarded from the upstream
/// response; everything else passes through untouched.
const HOP_BY_HOP: &[HeaderName] = &[
    header::CONNECTION,
    header::TRANSFER_ENCODING,
    header::CONTENT_LENGTH,
    header::TE,
    header::TRAILER,
    header::UPGRADE,
];

fn forward_response(
    upstream: http::Response<hyper::body::Incoming>,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Response {
    let (parts, body) = upstream.into_parts();
    let body = PermitBody {
        inner: body,
        _permit: permit,
    };
    let mut response = Response::new(Body::new(body));
    *response.status_mut() = parts.status;
    for (name, value) in &parts.headers {
        if !HOP_BY_HOP.contains(name) {
            response.headers_mut().append(name, value.clone());
        }
    }
    response
}

struct PermitBody {
    inner: hyper::body::Incoming,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl http_body::Body for PermitBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

fn record_request_metrics(provider: &str, status: StatusCode) {
    let class = match status.as_u16() {
        200..=299 => "2xx",
        400..=499 => "4xx",
        _ => "5xx",
    };
    metrics::counter!(
        "rapid_requests_total",
        "provider" => provider.to_owned(),
        "status_class" => class,
    )
    .increment(1);
}

pub fn error_response(err: &GatewayError) -> Response {
    error_response_in(Dialect::OpenAi, err)
}

pub fn error_response_in(dialect: Dialect, err: &GatewayError) -> Response {
    error_response_for(RenderTarget::Dialect(dialect), err)
}

pub fn error_response_for(render: RenderTarget, err: &GatewayError) -> Response {
    let status =
        StatusCode::from_u16(err.class.http_status()).expect("taxonomy statuses are valid");
    let body = router_providers::render_error_for(render, err);
    let mut response = (status, axum::Json(body)).into_response();
    if err.class == ErrorClass::RateLimited || err.class == ErrorClass::NoCapacity {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    if let Some(provider) = &err.provider
        && let Ok(v) = HeaderValue::from_str(provider)
    {
        response.headers_mut().insert("x-rapid-provider", v);
    }
    metrics::counter!(
        "rapid_requests_total",
        "provider" => err.provider.clone().unwrap_or_else(|| "none".into()),
        "status_class" => if status.is_client_error() { "4xx" } else { "5xx" },
    )
    .increment(1);
    response
}

pub async fn models(State(state): State<Arc<AppState>>) -> Response {
    let table = state.table.load();
    let mut data = Vec::new();
    for provider in table.providers() {
        let mut listed: std::collections::BTreeSet<&str> = Default::default();
        for key in &provider.keys {
            for model in key.models.iter().flatten() {
                listed.insert(model);
            }
        }
        for model in listed {
            data.push(serde_json::json!({
                "id": format!("{}/{}", provider.name, model),
                "object": "model",
                "owned_by": provider.name,
            }));
        }
    }
    for (alias, target) in table.aliases() {
        data.push(serde_json::json!({
            "id": alias,
            "object": "model",
            "owned_by": target.provider,
        }));
    }
    axum::Json(serde_json::json!({ "object": "list", "data": data })).into_response()
}

// ---------------------------------------------------------------------------
// The Responses API endpoint
// ---------------------------------------------------------------------------

pub async fn handle_responses(
    state: Arc<AppState>,
    headers: HeaderMap,
    body: Bytes,
    vk: Option<Arc<VkRuntime>>,
) -> Response {
    let started = Instant::now();
    let value = serde_json::from_slice::<Value>(&body).ok();
    let requested = value
        .as_ref()
        .and_then(|v| v["model"].as_str())
        .unwrap_or("unknown")
        .to_owned();
    let stream = value
        .as_ref()
        .is_some_and(|v| v["stream"] == Value::Bool(true));
    // Cloning a `Bytes` is a refcount bump, not a copy.
    let sent = body.clone();
    match run_responses(&state, &headers, body, started, vk.as_deref()).await {
        Ok(response) => meter(
            response,
            build_hook_with_input(
                &state,
                &vk,
                "responses",
                &requested,
                &headers,
                started,
                &sent,
            ),
            Dialect::OpenAi,
            stream,
        ),
        Err(err) => {
            record_failure(
                &state,
                &vk,
                "responses",
                &requested,
                &err,
                started,
                &headers,
                Some(&sent),
            );
            error_response_for(RenderTarget::Responses, &err)
        }
    }
}

async fn run_responses(
    state: &AppState,
    headers: &HeaderMap,
    body: Bytes,
    started: Instant,
    vk: Option<&VkRuntime>,
) -> Result<Response, GatewayError> {
    let value: Value = serde_json::from_slice(&body).map_err(|err| {
        GatewayError::new(
            ErrorClass::InvalidRequest,
            format!("invalid request body: {err}"),
        )
    })?;
    let model = value["model"]
        .as_str()
        .ok_or_else(|| {
            GatewayError::new(ErrorClass::InvalidRequest, "`model` is required").with_param("model")
        })?
        .to_owned();
    let stream = value["stream"] == Value::Bool(true);
    let wants_state =
        value["store"] == Value::Bool(true) || !value["previous_response_id"].is_null();

    let table = state.table.load();
    let plan = table.plan(&model)?;
    if let Some(vk) = vk {
        vk_gate(vk, &model, &plan)?;
    }

    // Parsed lazily, at most once, only when some target needs translation.
    let mut internal: Option<router_core::chat::ChatRequest> = None;

    let mut attempts: u32 = 0;
    let mut last_error: Option<GatewayError> = None;

    let n_targets = plan.targets.len();
    for (t_idx, route) in plan.targets.iter().enumerate() {
        let Some(out_dialect) = router_providers::wire_dialect(route.provider.kind) else {
            last_error = Some(
                GatewayError::new(
                    ErrorClass::InvalidRequest,
                    format!(
                        "provider `{}` ({:?}) is configured but its adapter is not available yet",
                        route.provider.name, route.provider.kind
                    ),
                )
                .with_param("model")
                .with_provider(&route.provider.name),
            );
            continue;
        };

        // Providers that speak Responses on the wire relay the surface
        // natively; everything else translates the stateless core.
        //
        // Codex counts: its backend *is* the Responses API, so relaying
        // is both the faithful thing and the only way newer surface
        // features survive the trip. It answers in SSE and nothing else,
        // so a caller who wants a whole body still goes the translated
        // route, where the stream is aggregated at our end.
        let codex_relay = out_dialect == Dialect::CodexResponses && stream && !wants_state;
        let relay = out_dialect == Dialect::OpenAi || codex_relay;
        let (out_body, path, emulated) = if relay {
            let rewritten = if codex_relay {
                Bytes::from(
                    serde_json::to_vec(&router_providers::subscription::codex_relay_body(
                        &value,
                        &route.upstream_model,
                    ))
                    .expect("serializable"),
                )
            } else {
                match json::probe(&body) {
                    Some(probe) => {
                        json::splice_model(&body, probe.model_span, &route.upstream_model)
                    }
                    None => {
                        let mut v = value.clone();
                        v["model"] = Value::String(route.upstream_model.clone());
                        Bytes::from(serde_json::to_vec(&v).expect("serializable"))
                    }
                }
            };
            let path = if codex_relay {
                "/backend-api/codex/responses".to_owned()
            } else {
                "/responses".to_owned()
            };
            (rewritten, path, false)
        } else {
            if wants_state {
                return Err(GatewayError::new(
                    ErrorClass::InvalidRequest,
                    "`store` and `previous_response_id` require a provider that relays the \
                     Responses API natively; this target is translated statelessly",
                )
                .with_param("store")
                .with_provider(&route.provider.name));
            }
            let req = match &internal {
                Some(r) => r,
                None => {
                    let parsed = router_providers::responses::request_to_internal(&value)?;
                    for param in &parsed.dropped_params {
                        metrics::counter!("rapid_dropped_params_total",
                            "param" => param.clone(),
                            "provider" => route.provider.name.clone())
                        .increment(1);
                    }
                    internal.insert(parsed.internal)
                }
            };
            let built = router_providers::build_outbound(
                out_dialect,
                req,
                &route.upstream_model,
                stream,
                route.provider.codex.as_ref(),
                route.provider.kind == ProviderKind::ClaudeSubscription,
            )?;
            (built.body, built.path, built.json_schema_emulated)
        };

        for a_idx in 0..plan.max_attempts_per_target {
            let is_last_candidate =
                t_idx + 1 == n_targets && a_idx + 1 == plan.max_attempts_per_target;
            let now = clock::now_ms();
            let Some(choice) = route.provider.admit_key(&route.upstream_model, now) else {
                last_error = Some(
                    GatewayError::new(ErrorClass::NoCapacity, "no healthy key")
                        .with_provider(&route.provider.name),
                );
                break;
            };
            let Ok(permit) = route.provider.semaphore.clone().try_acquire_owned() else {
                last_error = Some(
                    GatewayError::new(ErrorClass::NoCapacity, "provider at max concurrency")
                        .with_provider(&route.provider.name),
                );
                break;
            };
            attempts += 1;
            let breaker = route.provider.breaker_for(choice.key);
            // Renew a subscription seat that is about to expire, before
            // its token is put on a request. Doing it here rather than on
            // the 401 that would follow keeps the failure off the caller's
            // path; a seat that cannot renew simply carries on and is
            // handled reactively.
            if route.provider.kind.is_subscription()
                && let Some(key) = choice.key
            {
                refresh::refresh_if_stale(
                    &state.upstream,
                    &state.refreshes,
                    route.provider.kind,
                    key,
                    &key.source_path
                        .as_deref()
                        .map(|p| refresh::Persist::File(p.to_owned()))
                        .unwrap_or(refresh::Persist::None),
                    now_epoch_ms(),
                )
                .await;
            }
            let request = build_upstream_request(
                route,
                out_dialect,
                &path,
                headers,
                choice.key,
                out_body.clone(),
            )?;
            let ctx = TranslationCtx {
                passthrough: relay,
                render: RenderTarget::Responses,
                out_dialect,
                stream,
                emulated,
            };
            match attempt(
                state,
                route,
                request,
                breaker,
                choice.key,
                permit,
                &plan,
                is_last_candidate,
                ctx,
            )
            .await
            {
                AttemptOutcome::Serve(mut response) => {
                    finalize(&mut response, route, choice.key, attempts, started);
                    if emulated {
                        response
                            .headers_mut()
                            .insert("x-rapid-emulated", HeaderValue::from_static("json_schema"));
                    }
                    return Ok(response);
                }
                AttemptOutcome::Retry(err) => last_error = Some(err),
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        GatewayError::new(ErrorClass::NoCapacity, "no route candidates available")
    }))
}

// ---------------------------------------------------------------------------
// Passthrough: verbatim forward with gateway-managed auth
// ---------------------------------------------------------------------------

/// `ANY /passthrough/{provider}/{path…}` — the escape hatch: new provider
/// features work the day they ship. The gateway injects auth, meters the
/// request, and forwards everything else untouched (single attempt, no
/// retries, no translation).
#[allow(clippy::too_many_arguments)] // these are the parts of the request
pub async fn handle_passthrough(
    state: Arc<AppState>,
    provider_name: String,
    rest: String,
    method: http::Method,
    query: Option<String>,
    headers: HeaderMap,
    body: Bytes,
    vk: Option<Arc<VkRuntime>>,
) -> Response {
    let started = Instant::now();
    let requested = format!("{provider_name}/*");
    if let Some(key) = vk.as_deref() {
        if !key.allows_model(&requested, Some(&requested)) {
            let err = GatewayError::new(
                ErrorClass::Permission,
                format!(
                    "virtual key `{}` does not allow provider passthrough",
                    key.def.name
                ),
            );
            record_failure(
                &state,
                &vk,
                "passthrough",
                &requested,
                &err,
                started,
                &headers,
                None,
            );
            return error_response(&err);
        }
        if let Err(deny) = key.admit(clock::now_ms(), key.period_ordinal(vkey::unix_now_ms())) {
            let class = if deny == VkDeny::BudgetExhausted {
                ErrorClass::InsufficientQuota
            } else {
                ErrorClass::RateLimited
            };
            let err = GatewayError::new(class, "virtual key limit exceeded");
            record_failure(
                &state,
                &vk,
                "passthrough",
                &requested,
                &err,
                started,
                &headers,
                None,
            );
            return error_response(&err);
        }
    }
    match run_passthrough(&state, &provider_name, &rest, method, query, &headers, body).await {
        Ok(response) => {
            let dialect = state
                .table
                .load()
                .providers()
                .find(|p| p.name == provider_name)
                .and_then(|p| router_providers::wire_dialect(p.kind))
                .unwrap_or(Dialect::OpenAi);
            let stream = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("text/event-stream"));
            meter(
                response,
                build_hook(&state, &vk, "passthrough", &requested, &headers, started),
                dialect,
                stream,
            )
        }
        Err(err) => {
            record_failure(
                &state,
                &vk,
                "passthrough",
                &requested,
                &err,
                started,
                &headers,
                None,
            );
            error_response(&err)
        }
    }
}

async fn run_passthrough(
    state: &AppState,
    provider_name: &str,
    rest: &str,
    method: http::Method,
    query: Option<String>,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response, GatewayError> {
    let table = state.table.load();
    let provider = table
        .providers()
        .find(|p| p.name == provider_name)
        .cloned()
        .ok_or_else(|| {
            GatewayError::new(
                ErrorClass::NotFound,
                format!("unknown provider `{provider_name}`"),
            )
        })?;

    let base = provider
        .base_url
        .as_deref()
        .ok_or_else(|| GatewayError::new(ErrorClass::UpstreamError, "provider has no base_url"))?;
    let mut url = format!("{}/{}", base.trim_end_matches('/'), rest);
    if let Some(q) = &query {
        url.push('?');
        url.push_str(q);
    }

    let Ok(permit) = provider.semaphore.clone().try_acquire_owned() else {
        return Err(
            GatewayError::new(ErrorClass::NoCapacity, "provider at max concurrency")
                .with_provider(provider_name),
        );
    };

    let mut builder = http::Request::builder().method(method).uri(&url);
    for name in [header::CONTENT_TYPE, header::ACCEPT] {
        if let Some(value) = headers.get(&name) {
            builder = builder.header(name, value);
        }
    }
    // Auth by provider kind, same material the routed paths use.
    if provider.auth == AuthMode::Key
        && let Some(key) = provider.keys.first()
    {
        let sensitive = |value: String| {
            let mut v = HeaderValue::from_str(&value).expect("key material is ascii");
            v.set_sensitive(true);
            v
        };
        let relay_token = key.token();
        if provider.azure.is_some() {
            builder = builder.header("api-key", sensitive(relay_token.expose().to_owned()));
        } else if let Some(bedrock) = &provider.bedrock {
            let uri: http::Uri = url.parse().map_err(|_| {
                GatewayError::new(ErrorClass::InvalidRequest, "invalid passthrough path")
            })?;
            let host = uri
                .authority()
                .map(|a| a.as_str().to_owned())
                .unwrap_or_default();
            let amz_date = router_providers::sigv4::amz_date_now();
            let method = builder.method_ref().expect("set above").as_str().to_owned();
            let signature =
                router_providers::sigv4::sign(&router_providers::sigv4::SigningParams {
                    access_key_id: &bedrock.access_key_id,
                    secret_access_key: relay_token.expose(),
                    region: &bedrock.region,
                    service: "bedrock",
                    amz_date: &amz_date,
                    host: &host,
                    method: &method,
                    canonical_path: uri.path(),
                    query: uri.query().unwrap_or(""),
                    payload: &body,
                });
            builder = builder
                .header("host", host)
                .header("x-amz-date", amz_date)
                .header("x-amz-content-sha256", signature.amz_content_sha256)
                .header(header::AUTHORIZATION, sensitive(signature.authorization));
        } else {
            match provider.kind {
                router_core::config::ProviderKind::Anthropic => {
                    builder = builder
                        .header("x-api-key", sensitive(relay_token.expose().to_owned()))
                        .header("anthropic-version", router_providers::ANTHROPIC_VERSION);
                }
                router_core::config::ProviderKind::Gemini => {
                    builder = builder
                        .header("x-goog-api-key", sensitive(relay_token.expose().to_owned()));
                }
                _ => {
                    builder = builder.header(
                        header::AUTHORIZATION,
                        sensitive(format!("Bearer {}", relay_token.expose())),
                    );
                }
            }
        }
    }

    let request = builder.body(Body::from(body)).map_err(|e| {
        GatewayError::new(
            ErrorClass::InvalidRequest,
            format!("bad passthrough request: {e}"),
        )
    })?;

    let response = state
        .upstream
        .send(provider_name, request, provider.timeout)
        .await?;
    metrics::counter!(
        "rapid_passthrough_total",
        "provider" => provider_name.to_owned(),
        "status_class" => if response.status().is_success() { "2xx" } else { "4xx5xx" },
    )
    .increment(1);
    let mut response = forward_response(response, permit);
    if let Ok(v) = HeaderValue::from_str(provider_name) {
        response.headers_mut().insert("x-rapid-provider", v);
    }
    Ok(response)
}

/// Wall-clock milliseconds, for comparing against a credential's expiry.
///
/// Distinct from `clock::now_ms`, which is a monotonic process epoch: a
/// token's `exp` is a real date and cannot be compared against uptime.
fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
