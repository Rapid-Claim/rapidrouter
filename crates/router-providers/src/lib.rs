//! Provider dialects and translation.
//!
//! Every wire format translates through the internal OpenAI-shaped model
//! ([`router_core::chat`]): inbound dialects parse into it, outbound
//! adapters build from it. Same-dialect traffic bypasses this crate
//! entirely (raw passthrough in the server).

pub mod anthropic;
pub mod bedrock;
pub mod gemini;
pub mod responses;
pub mod sigv4;
pub mod subscription;

use bytes::Bytes;
use router_core::chat::ChatRequest;
use router_core::config::ProviderKind;
use router_core::sse::SseEvent;
use router_core::{ErrorClass, GatewayError};
use serde_json::Value;

pub const ANTHROPIC_VERSION: &str = anthropic::VERSION_HEADER;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    OpenAi,
    Anthropic,
    Gemini,
    /// Outbound-only: Bedrock's Converse dialect. Never an inbound wire.
    Bedrock,
    /// Outbound-only: the Responses dialect as the ChatGPT Codex backend
    /// serves it. Inbound Responses traffic is a different path
    /// ([`crate::responses`]); this is the wire we *speak* to Codex.
    CodexResponses,
}

/// The wire dialect a provider speaks, or `None` for kinds whose
/// adapters have not shipped yet (Azure, Bedrock).
pub fn wire_dialect(kind: ProviderKind) -> Option<Dialect> {
    match kind {
        // Azure speaks the OpenAI dialect over its own URL/auth scheme,
        // which the transport layer handles.
        ProviderKind::OpenAi | ProviderKind::OpenAiCompat | ProviderKind::Azure => {
            Some(Dialect::OpenAi)
        }
        ProviderKind::Anthropic => Some(Dialect::Anthropic),
        // Vertex serves Gemini's wire dialect from its own URL scheme,
        // which the transport layer handles.
        ProviderKind::Gemini | ProviderKind::Vertex => Some(Dialect::Gemini),
        ProviderKind::Bedrock => Some(Dialect::Bedrock),
        // A subscription seat changes the credential and the headers, not
        // the wire: Claude subscription traffic is ordinary Messages API
        // traffic, and every Anthropic translation applies unchanged.
        ProviderKind::ClaudeSubscription => Some(Dialect::Anthropic),
        ProviderKind::CodexSubscription => Some(Dialect::CodexResponses),
    }
}

/// Whether a dialect needs documents rendered to images before it can be
/// built for.
///
/// Only the Codex backend does. Its content vocabulary is `input_text` and
/// `input_image` — the official client's own `ContentItem` enum has three
/// variants and none of them is a file — so a PDF has no representation on
/// that wire at all. Anthropic and Gemini both take a document natively and
/// must NOT be pre-rendered: rasterizing would throw away the extractable
/// text layer those APIs use.
pub fn needs_rasterized_documents(dialect: Dialect) -> bool {
    match dialect {
        Dialect::CodexResponses => true,
        Dialect::OpenAi | Dialect::Anthropic | Dialect::Gemini | Dialect::Bedrock => false,
    }
}

/// A fully translated upstream request body plus its metadata.
pub struct OutboundRequest {
    /// Path (and query) appended to the provider's base URL.
    pub path: String,
    pub body: Bytes,
    pub dropped_params: Vec<String>,
    /// Anthropic json_schema emulation is in force; response translation
    /// must fold the forced tool call back into content.
    pub json_schema_emulated: bool,
}

/// Build the chat request body for a foreign-dialect target.
pub fn build_outbound(
    dialect: Dialect,
    req: &ChatRequest,
    model: &str,
    stream: bool,
    codex: Option<&router_core::config::CodexSettings>,
    pin_claude_identity: bool,
) -> Result<OutboundRequest, GatewayError> {
    match dialect {
        Dialect::OpenAi => {
            let mut req = req.clone();
            req.model = model.to_owned();
            req.stream = if stream { Some(true) } else { None };
            // `metadata` rode in on `extra` and would be re-emitted with
            // the rest of it. It is the gateway's attribution channel,
            // already read off the request, and OpenAI accepts only
            // sixteen string pairs under that name — so forwarding what
            // callers actually send there is a 400, not a passthrough.
            // The foreign dialects drop `extra` wholesale already.
            req.extra.remove("metadata");
            let body = serde_json::to_vec(&req).expect("chat request serializes");
            Ok(OutboundRequest {
                path: "/chat/completions".into(),
                body: body.into(),
                dropped_params: Vec::new(),
                json_schema_emulated: false,
            })
        }
        Dialect::Anthropic => {
            let mut built = anthropic::build_request(req, model)?;
            // A subscription token is authorized for the Claude Code
            // identity, so the request must present it.
            if pin_claude_identity {
                subscription::pin_claude_identity(&mut built.body);
            }
            Ok(OutboundRequest {
                path: "/v1/messages".into(),
                body: serde_json::to_vec(&built.body)
                    .expect("value serializes")
                    .into(),
                dropped_params: built.dropped_params,
                json_schema_emulated: built.json_schema_emulated,
            })
        }
        Dialect::Gemini => {
            let built = gemini::build_request(req)?;
            let action = if stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            };
            Ok(OutboundRequest {
                path: format!("/v1beta/models/{model}:{action}"),
                body: serde_json::to_vec(&built.body)
                    .expect("value serializes")
                    .into(),
                dropped_params: built.dropped_params,
                json_schema_emulated: false,
            })
        }
        Dialect::Bedrock => {
            let built = bedrock::build_request(req)?;
            let action = if stream {
                "converse-stream"
            } else {
                "converse"
            };
            Ok(OutboundRequest {
                path: format!("/model/{}/{action}", sigv4::encode_path_segment(model)),
                body: serde_json::to_vec(&built.body)
                    .expect("value serializes")
                    .into(),
                dropped_params: built.dropped_params,
                json_schema_emulated: false,
            })
        }
        Dialect::CodexResponses => {
            // The Codex backend only speaks SSE — `stream: false` is not
            // an option it offers. A non-streaming caller is served by
            // aggregating the stream at our end
            // ([`subscription::aggregate_chunks`]), not by asking for a
            // whole body upstream.
            let _ = stream;
            let settings = codex.cloned().unwrap_or_default();
            let built = subscription::codex_request(req, model, &settings)?;
            Ok(OutboundRequest {
                path: "/backend-api/codex/responses".into(),
                body: serde_json::to_vec(&built.body)
                    .expect("value serializes")
                    .into(),
                dropped_params: built.dropped_params,
                json_schema_emulated: false,
            })
        }
    }
}

/// The upstream path for raw same-dialect passthrough.
pub fn passthrough_path(dialect: Dialect, model: &str, stream: bool) -> String {
    match dialect {
        Dialect::OpenAi => "/chat/completions".into(),
        Dialect::Anthropic => "/v1/messages".into(),
        Dialect::Gemini => {
            let action = if stream {
                "streamGenerateContent?alt=sse"
            } else {
                "generateContent"
            };
            format!("/v1beta/models/{model}:{action}")
        }
        Dialect::Bedrock => {
            let action = if stream {
                "converse-stream"
            } else {
                "converse"
            };
            format!("/model/{}/{action}", sigv4::encode_path_segment(model))
        }
        // Never reachable: passthrough is same-dialect forwarding, and no
        // inbound wire is Codex's private Responses dialect.
        Dialect::CodexResponses => "/backend-api/codex/responses".into(),
    }
}

/// Translate a complete upstream response into the internal OpenAI shape.
pub fn response_to_openai(
    dialect: Dialect,
    body: &[u8],
    model: &str,
    json_schema_emulated: bool,
) -> Result<Value, GatewayError> {
    // Codex answers only in SSE, even for a caller who wanted a whole
    // body, so it never has a JSON document to parse here.
    if dialect == Dialect::CodexResponses {
        return Ok(subscription::aggregate_sse(body, model));
    }
    let value: Value = serde_json::from_slice(body).map_err(|e| {
        GatewayError::new(
            ErrorClass::UpstreamError,
            format!("provider returned unparseable response: {e}"),
        )
    })?;
    Ok(match dialect {
        Dialect::OpenAi => value,
        Dialect::Anthropic => anthropic::response_to_openai(&value, json_schema_emulated),
        Dialect::Gemini => gemini::response_to_openai(&value, model),
        Dialect::Bedrock => bedrock::response_to_openai(&value, model),
        // A Codex response is never a whole body; it arrives as SSE even
        // for a non-streaming caller, and is aggregated from chunks.
        Dialect::CodexResponses => value,
    })
}

/// Render an internal (OpenAI-shaped) response in the inbound dialect.
pub fn render_response(inbound: Dialect, openai: &Value) -> Value {
    match inbound {
        Dialect::OpenAi | Dialect::Bedrock | Dialect::CodexResponses => openai.clone(),
        Dialect::Anthropic => anthropic::openai_response_to_anthropic(openai),
        Dialect::Gemini => gemini::openai_response_to_gemini(openai),
    }
}

/// Upstream SSE events -> internal OpenAI chunk objects.
pub enum UpstreamStream {
    /// OpenAI-dialect upstream: parse each `data:` payload, drop `[DONE]`.
    OpenAi,
    Anthropic(anthropic::StreamToOpenAi),
    Gemini(gemini::StreamToOpenAi),
    Bedrock(bedrock::StreamToOpenAi),
    Codex(subscription::CodexStreamToOpenAi),
}

impl UpstreamStream {
    pub fn new(dialect: Dialect, model: &str, json_schema_emulated: bool) -> Self {
        match dialect {
            Dialect::OpenAi => Self::OpenAi,
            Dialect::Anthropic => {
                Self::Anthropic(anthropic::StreamToOpenAi::new(json_schema_emulated))
            }
            Dialect::Gemini => Self::Gemini(gemini::StreamToOpenAi::new(model)),
            Dialect::Bedrock => Self::Bedrock(bedrock::StreamToOpenAi::new(model)),
            Dialect::CodexResponses => Self::Codex(subscription::CodexStreamToOpenAi::new(model)),
        }
    }

    pub fn on_event(&mut self, event: &SseEvent) -> Vec<Value> {
        match self {
            Self::OpenAi => {
                if event.data == "[DONE]" {
                    return Vec::new();
                }
                serde_json::from_str::<Value>(&event.data)
                    .map(|v| vec![v])
                    .unwrap_or_default()
            }
            Self::Anthropic(state) => state.on_event(event),
            Self::Gemini(state) => serde_json::from_str::<Value>(&event.data)
                .map(|v| state.on_chunk(&v))
                .unwrap_or_default(),
            Self::Bedrock(state) => state.on_event(event),
            Self::Codex(state) => state.on_event(event),
        }
    }
}

/// What shape the client is owed: a wire dialect's chat surface, or the
/// Responses API surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderTarget {
    Dialect(Dialect),
    Responses,
}

/// Render an internal (OpenAI-shaped) sync response for the client.
pub fn render_for(target: RenderTarget, openai: &Value) -> Value {
    match target {
        RenderTarget::Dialect(d) => render_response(d, openai),
        RenderTarget::Responses => responses::openai_response_to_responses(openai),
    }
}

/// Render an error for the client (Responses errors use the OpenAI shape).
pub fn render_error_for(target: RenderTarget, err: &GatewayError) -> Value {
    match target {
        RenderTarget::Dialect(d) => render_error(d, err),
        RenderTarget::Responses => err.to_openai_body(),
    }
}

/// Internal OpenAI chunks -> client-facing wire frames.
pub enum InboundStream {
    OpenAi,
    Anthropic(anthropic::OpenAiToAnthropicStream),
    Gemini(gemini::OpenAiToGeminiStream),
    Responses(Box<responses::ChunksToResponses>),
}

impl InboundStream {
    pub fn new(dialect: Dialect) -> Self {
        match dialect {
            Dialect::OpenAi | Dialect::Bedrock | Dialect::CodexResponses => Self::OpenAi,
            Dialect::Anthropic => Self::Anthropic(anthropic::OpenAiToAnthropicStream::new()),
            Dialect::Gemini => Self::Gemini(gemini::OpenAiToGeminiStream::new()),
        }
    }

    pub fn new_for(target: RenderTarget) -> Self {
        match target {
            RenderTarget::Dialect(d) => Self::new(d),
            RenderTarget::Responses => {
                Self::Responses(Box::new(responses::ChunksToResponses::new()))
            }
        }
    }

    pub fn on_chunk(&mut self, chunk: &Value) -> Vec<String> {
        match self {
            Self::OpenAi => vec![format!("data: {chunk}\n\n")],
            Self::Anthropic(state) => state
                .on_chunk(chunk)
                .into_iter()
                .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
                .collect(),
            Self::Gemini(state) => state
                .on_chunk(chunk)
                .map(|v| vec![format!("data: {v}\n\n")])
                .unwrap_or_default(),
            Self::Responses(state) => state.on_chunk(chunk),
        }
    }

    /// Terminal frames after the upstream stream ends.
    pub fn finish(&mut self) -> Vec<String> {
        match self {
            Self::OpenAi => vec!["data: [DONE]\n\n".to_owned()],
            Self::Anthropic(state) => state
                .finish()
                .into_iter()
                .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
                .collect(),
            Self::Gemini(_) => Vec::new(),
            Self::Responses(state) => state.finish(),
        }
    }
}

/// Render an error in the inbound dialect's error shape.
pub fn render_error(inbound: Dialect, err: &GatewayError) -> Value {
    match inbound {
        Dialect::OpenAi | Dialect::Bedrock | Dialect::CodexResponses => err.to_openai_body(),
        Dialect::Anthropic => serde_json::json!({
            "type": "error",
            "error": {
                "type": match err.class {
                    ErrorClass::InvalidRequest | ErrorClass::NotFound | ErrorClass::PayloadTooLarge => "invalid_request_error",
                    ErrorClass::Authentication => "authentication_error",
                    ErrorClass::Permission => "permission_error",
                    ErrorClass::RateLimited | ErrorClass::InsufficientQuota => "rate_limit_error",
                    ErrorClass::Timeout | ErrorClass::UpstreamError | ErrorClass::NoCapacity => "api_error",
                },
                "message": err.message,
            },
        }),
        Dialect::Gemini => serde_json::json!({
            "error": {
                "code": err.class.http_status(),
                "message": err.message,
                "status": match err.class {
                    ErrorClass::InvalidRequest | ErrorClass::PayloadTooLarge => "INVALID_ARGUMENT",
                    ErrorClass::Authentication => "UNAUTHENTICATED",
                    ErrorClass::Permission => "PERMISSION_DENIED",
                    ErrorClass::NotFound => "NOT_FOUND",
                    ErrorClass::RateLimited | ErrorClass::InsufficientQuota => "RESOURCE_EXHAUSTED",
                    ErrorClass::Timeout => "DEADLINE_EXCEEDED",
                    ErrorClass::UpstreamError | ErrorClass::NoCapacity => "UNAVAILABLE",
                },
            },
        }),
    }
}
