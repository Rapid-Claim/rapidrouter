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
use http_body_util::BodyExt;
use http_body_util::Full;
use router_core::breaker::Breaker;
use router_core::chat::ChatRequest;
use router_core::config::{AuthMode, RetryOn};
use router_core::router::{KeyRuntime, ResolvedRoute, RoutePlan};
use router_core::sse::SseParser;
use router_core::{ErrorClass, GatewayError, clock, json};
use router_providers::{Dialect, InboundStream, RenderTarget, UpstreamStream};
use serde_json::Value;

use crate::AppState;

/// Cap for buffered (non-streaming) translated response bodies.
const MAX_TRANSLATED_RESPONSE: usize = 64 * 1024 * 1024;

/// OpenAI-dialect relay endpoints with no translation surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Completions,
    Embeddings,
}

impl Endpoint {
    fn upstream_path(self) -> &'static str {
        match self {
            Self::Completions => "/completions",
            Self::Embeddings => "/embeddings",
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
) -> Response {
    let started = Instant::now();
    let dialect = inbound.dialect();
    match run_chat(&state, &inbound, &headers, started).await {
        Ok(response) => response,
        Err(err) => error_response_in(dialect, &err),
    }
}

async fn run_chat(
    state: &AppState,
    inbound: &InboundChat,
    headers: &HeaderMap,
    started: Instant,
) -> Result<Response, GatewayError> {
    let table = state.table.load();
    let plan = table.plan(inbound.model())?;
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
            let built =
                router_providers::build_outbound(out_dialect, req, &route.upstream_model, stream)?;
            (
                built.body,
                built.path,
                built.dropped_params,
                built.json_schema_emulated,
            )
        };
        for param in &dropped {
            metrics::counter!("caret_dropped_params_total", "param" => param.clone(), "provider" => route.provider.name.clone()).increment(1);
            tracing::debug!(provider = %route.provider.name, param, "dropped unsupported parameter");
        }

        for a_idx in 0..plan.max_attempts_per_target {
            let is_last_candidate =
                t_idx + 1 == n_targets && a_idx + 1 == plan.max_attempts_per_target;
            let now = clock::now_ms();
            let Some(choice) = route.provider.admit_key(&route.upstream_model, now) else {
                last_error = Some(
                    GatewayError::new(
                        ErrorClass::NoCapacity,
                        format!(
                            "no healthy key of provider `{}` for model `{}`",
                            route.provider.name, route.upstream_model
                        ),
                    )
                    .with_provider(&route.provider.name),
                );
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
                    finalize(&mut response, route, attempts, started);
                    if emulated {
                        response
                            .headers_mut()
                            .insert("x-caret-emulated", HeaderValue::from_static("json_schema"));
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
    request: http::Request<Full<Bytes>>,
    breaker: &Breaker,
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
    metrics::histogram!("caret_upstream_duration_seconds", "provider" => route.provider.name.clone())
        .record(upstream_started.elapsed().as_secs_f64());

    match result {
        Err(err) => {
            breaker.record_failure(clock::now_ms());
            metrics::counter!("caret_retries_total", "provider" => route.provider.name.clone())
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

            if retryable && !is_last_candidate {
                metrics::counter!("caret_retries_total", "provider" => route.provider.name.clone())
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

            if ctx.passthrough {
                return AttemptOutcome::Serve(forward_response(response, permit));
            }
            translated_response(route, response, permit, ctx).await
        }
    }
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
    let body = translated_stream_body(response.into_body(), permit, translator, formatter);
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .expect("static response parts");
    AttemptOutcome::Serve(response)
}

fn translated_stream_body(
    upstream: hyper::body::Incoming,
    permit: tokio::sync::OwnedSemaphorePermit,
    translator: UpstreamStream,
    formatter: InboundStream,
) -> Body {
    struct StreamState {
        upstream: hyper::body::Incoming,
        parser: SseParser,
        translator: UpstreamStream,
        formatter: InboundStream,
        done: bool,
        _permit: tokio::sync::OwnedSemaphorePermit,
    }

    let state = StreamState {
        upstream,
        parser: SseParser::new(),
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

fn finalize(response: &mut Response, route: &ResolvedRoute, attempts: u32, started: Instant) {
    let headers = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&route.provider.name) {
        headers.insert("x-caret-provider", v);
    }
    if let Ok(v) = HeaderValue::from_str(&route.upstream_model) {
        headers.insert("x-caret-model", v);
    }
    if let Ok(v) = HeaderValue::from_str(&attempts.to_string()) {
        headers.insert("x-caret-attempts", v);
    }
    let overhead = started.elapsed();
    if let Ok(v) = HeaderValue::from_str(&overhead.as_micros().to_string()) {
        headers.insert("x-caret-overhead-us", v);
    }
    record_request_metrics(&route.provider.name, response.status());
}

fn build_upstream_request(
    route: &ResolvedRoute,
    out_dialect: Dialect,
    path: &str,
    inbound: &HeaderMap,
    key: Option<&KeyRuntime>,
    body: Bytes,
) -> Result<http::Request<Full<Bytes>>, GatewayError> {
    let base = route.provider.base_url.as_deref().ok_or_else(|| {
        GatewayError::new(
            ErrorClass::UpstreamError,
            format!("provider `{}` has no base_url", route.provider.name),
        )
    })?;
    let url = format!("{}{}", base.trim_end_matches('/'), path);

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
        match out_dialect {
            Dialect::OpenAi => {
                builder = builder.header(
                    header::AUTHORIZATION,
                    sensitive(format!("Bearer {}", key.secret.expose()))?,
                );
            }
            Dialect::Anthropic => {
                builder = builder
                    .header("x-api-key", sensitive(key.secret.expose().to_owned())?)
                    .header("anthropic-version", router_providers::ANTHROPIC_VERSION);
                // Anthropic beta features requested by the client ride along.
                if let Some(beta) = inbound.get("anthropic-beta") {
                    builder = builder.header("anthropic-beta", beta);
                }
            }
            Dialect::Gemini => {
                builder =
                    builder.header("x-goog-api-key", sensitive(key.secret.expose().to_owned())?);
            }
        }
    }

    builder.body(Full::new(body)).map_err(|e| {
        GatewayError::new(
            ErrorClass::UpstreamError,
            format!("bad upstream request: {e}"),
        )
    })
}

// ---------------------------------------------------------------------------
// The OpenAI-only relay endpoints (completions, embeddings)
// ---------------------------------------------------------------------------

pub async fn handle_relay(
    state: Arc<AppState>,
    endpoint: Endpoint,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    match run_relay(&state, endpoint, &headers, body, started).await {
        Ok(response) => response,
        Err(err) => error_response(&err),
    }
}

async fn run_relay(
    state: &AppState,
    endpoint: Endpoint,
    headers: &HeaderMap,
    body: Bytes,
    started: Instant,
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
            match attempt(state, route, request, breaker, permit, &plan, is_last, ctx).await {
                AttemptOutcome::Serve(mut response) => {
                    finalize(&mut response, route, attempts, started);
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
        "caret_requests_total",
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
        response.headers_mut().insert("x-caret-provider", v);
    }
    metrics::counter!(
        "caret_requests_total",
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

pub async fn handle_responses(state: Arc<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let started = Instant::now();
    match run_responses(&state, &headers, body, started).await {
        Ok(response) => response,
        Err(err) => error_response_for(RenderTarget::Responses, &err),
    }
}

async fn run_responses(
    state: &AppState,
    headers: &HeaderMap,
    body: Bytes,
    started: Instant,
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

        // OpenAI-dialect targets relay the Responses surface natively;
        // everything else translates the stateless core.
        let relay = out_dialect == Dialect::OpenAi;
        let (out_body, path, emulated) = if relay {
            let rewritten = match json::probe(&body) {
                Some(probe) => json::splice_model(&body, probe.model_span, &route.upstream_model),
                None => {
                    let mut v = value.clone();
                    v["model"] = Value::String(route.upstream_model.clone());
                    Bytes::from(serde_json::to_vec(&v).expect("serializable"))
                }
            };
            (rewritten, "/responses".to_owned(), false)
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
                        metrics::counter!("caret_dropped_params_total",
                            "param" => param.clone(),
                            "provider" => route.provider.name.clone())
                        .increment(1);
                    }
                    internal.insert(parsed.internal)
                }
            };
            let built =
                router_providers::build_outbound(out_dialect, req, &route.upstream_model, stream)?;
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
                permit,
                &plan,
                is_last_candidate,
                ctx,
            )
            .await
            {
                AttemptOutcome::Serve(mut response) => {
                    finalize(&mut response, route, attempts, started);
                    if emulated {
                        response
                            .headers_mut()
                            .insert("x-caret-emulated", HeaderValue::from_static("json_schema"));
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
