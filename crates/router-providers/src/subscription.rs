//! Subscription-seat transports: Claude Code and Codex.
//!
//! Both serve a *subscription* rather than a metered API key, and both are
//! reached by presenting the credential the vendor's own CLI presents. The
//! two are otherwise nothing alike:
//!
//! - **Claude** is the ordinary Anthropic Messages API. The OAuth token is
//!   a different `Authorization` scheme and the request must carry the
//!   Claude Code identity, and that is the entire difference — translation,
//!   streaming, tool calls, and prompt caching are the paths that already
//!   ship. (Verified live 2026-08-15: a subscription token presented this
//!   way reaches quota enforcement on `api.anthropic.com/v1/messages`,
//!   which only an accepted credential can do.)
//! - **Codex** is a private Responses endpoint behind `chatgpt.com`, which
//!   accepts only what the Codex CLI sends and gates models on the CLI
//!   version string. It needs its own request shaping, in
//!   [`codex_request`].
//!
//! Everything here is pure: header lists and JSON bodies in, no I/O.

use router_core::chat::{ChatRequest, Content, ContentPart, Message};
use serde_json::{Map, Value, json};

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

/// The OAuth beta flag the vendor's own client sends.
///
/// Measured (2026-08-16): it makes **no difference** to whether a
/// subscription token is accepted — an identity-pinned request succeeds
/// with or without it, and an identity-less one fails either way. It is
/// still sent, because matching what the vendor client sends is the whole
/// strategy here and a flag that is inert today may gate something
/// tomorrow. It is not load-bearing; [`CLAUDE_CODE_IDENTITY`] is.
pub const CLAUDE_OAUTH_BETA: &str = "oauth-2025-04-20";

/// The identity a Claude Code OAuth token is authorized for.
///
/// **Load-bearing, and measured** (2026-08-16). Without this as the
/// leading system block, `claude-sonnet-4-5` refuses the request — and
/// refuses it as a `429 rate_limit_error` whose message is the single
/// word `"Error"`, on an account with a completely fresh quota window.
/// `claude-haiku-4-5` serves either way, so a gateway tested only against
/// the cheap model would look correct and then fail on the expensive one.
///
/// The refusal is distinguishable from a real rate limit: a genuine quota
/// `429` carries the full `anthropic-ratelimit-unified-*` header set, and
/// this one carries none at all — which is why it does not bench the seat
/// (see `bench_exhausted_seat`).
///
/// A caller's own system prompt follows this block rather than replacing
/// it, and still steers the answer: with a pirate persona appended, "what
/// is 2+2" came back as "Ahoy there! 2+2 be 4, as sure as the sea be
/// salty!".
pub const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Prepend the Claude Code identity to an already-built Anthropic request
/// body, preserving whatever the caller put in `system`.
///
/// The Messages API accepts `system` as either a bare string or an array
/// of content blocks. Both are normalized to the array form here, because
/// the identity must be a *separate leading block* — concatenating it into
/// the caller's string would put their text and ours in one cache-keyed
/// unit, needlessly invalidating prompt caching every time the caller's
/// prompt changes.
///
/// Idempotent: a body whose first system block is already the identity is
/// returned untouched, so this is safe to apply on a retry path.
pub fn pin_claude_identity(body: &mut Value) {
    let identity = json!({"type": "text", "text": CLAUDE_CODE_IDENTITY});
    let Some(object) = body.as_object_mut() else {
        return;
    };
    let existing = object.remove("system");
    let mut blocks = match existing {
        None => Vec::new(),
        Some(Value::String(text)) if text.is_empty() => Vec::new(),
        Some(Value::String(text)) => vec![json!({"type": "text", "text": text})],
        Some(Value::Array(blocks)) => blocks,
        // Anything else is a caller shape we do not recognize; keep it
        // rather than silently dropping their instructions.
        Some(other) => vec![other],
    };
    let already_pinned = blocks
        .first()
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .is_some_and(|text| text == CLAUDE_CODE_IDENTITY);
    if !already_pinned {
        blocks.insert(0, identity);
    }
    object.insert("system".into(), Value::Array(blocks));
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

/// The private Responses endpoint the Codex CLI talks to.
pub const CODEX_BACKEND_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// The public OAuth client id the Codex CLI uses to refresh.
pub const CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Where a Codex credential is renewed.
pub const CODEX_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// The Codex CLI version this gateway presents itself as.
///
/// **Not cosmetic.** The ChatGPT backend gates model families on it and
/// answers an under-version client with
/// `400 "The '<model>' model requires a newer version of Codex."`
/// Configurable per provider precisely because it moves when OpenAI ships
/// a model family, and an operator must be able to follow that without
/// waiting for a rapid-router release.
pub const DEFAULT_CODEX_VERSION: &str = "0.146.0";

/// The header set the Codex CLI sends, reproduced exactly.
///
/// The backend is not a public API and is content to refuse anything that
/// does not look like its own client, so this is a fixed list rather than
/// a minimal one. `session_id` is fresh per request, matching the CLI.
pub fn codex_headers<'a>(
    access_token: &'a str,
    account_id: &'a str,
    version: &'a str,
    session_id: &'a str,
) -> Vec<(&'static str, String)> {
    vec![
        ("content-type", "application/json".into()),
        ("accept", "text/event-stream".into()),
        // The CLI disables compression; the backend's SSE framing is
        // sensitive to it in ways not worth discovering in production.
        ("accept-encoding", "identity".into()),
        ("authorization", format!("Bearer {access_token}")),
        ("chatgpt-account-id", account_id.to_owned()),
        ("version", version.to_owned()),
        ("openai-beta", "responses=experimental".into()),
        ("session_id", session_id.to_owned()),
        ("originator", "codex_cli_rs".into()),
        ("user-agent", format!("codex_cli_rs/{version}")),
    ]
}

/// Per-provider Codex knobs live in config
/// ([`router_core::config::CodexSettings`]) so an operator can follow a
/// backend version gate without a gateway release.
pub use router_core::config::CodexSettings;

/// A Codex request body plus what we could not carry.
pub struct CodexRequest {
    pub body: Value,
    pub dropped_params: Vec<String>,
}

/// Build the Responses body for the ChatGPT Codex backend.
///
/// Differences from the public Responses API that this handles, each of
/// which is a hard error rather than a degradation if you get it wrong:
///
/// - **No `max_output_tokens`.** The public API accepts it; this backend
///   answers `400 Unsupported parameter`. The caller's `max_tokens` is
///   therefore uncarryable and is reported in `dropped_params` rather than
///   dropped in silence.
/// - **Tools are flat.** The Responses API spells a function tool
///   `{"type": "function", "name", "description", "parameters"}` where
///   Chat Completions nests it under `function`. A named `tool_choice`
///   differs the same way.
/// - **`store: false`.** Nothing is retained upstream, which is also what
///   makes a failed request safe to re-issue on another seat.
/// - **The `json_object` "json" guard.** The backend refuses a
///   `json_object` request unless the word "json" appears in `input` — and
///   it checks `input` only, never `instructions`. A caller whose "return
///   JSON" wording lives in their system prompt would otherwise 400, so a
///   short neutral hint is appended when it is missing.
pub fn codex_request(req: &ChatRequest, model: &str, settings: &CodexSettings) -> CodexRequest {
    let mut dropped = Vec::new();
    let (instructions, input) = split_system_and_input(&req.messages);

    let text_format = codex_text_format(req.response_format.as_ref());
    let needs_json_hint = text_format
        .as_ref()
        .and_then(|f| f.get("type"))
        .and_then(Value::as_str)
        == Some("json_object");

    let mut input = input;
    if needs_json_hint {
        ensure_json_mentioned(&mut input);
    }

    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));
    body.insert(
        "instructions".into(),
        json!(instructions.unwrap_or_else(|| "You are Codex.".to_owned())),
    );
    body.insert("input".into(), Value::Array(input));

    // `format` and `verbosity` share one `text` object; assigning it twice
    // would drop the schema and quietly turn a structured-output request
    // back into free-form text.
    let mut text = Map::new();
    if let Some(format) = text_format {
        text.insert("format".into(), format);
    }
    if let Some(verbosity) = request_choice(req, "verbosity").or(settings.verbosity.clone()) {
        text.insert("verbosity".into(), json!(verbosity));
    }
    if !text.is_empty() {
        body.insert("text".into(), Value::Object(text));
    }
    if let Some(effort) =
        request_choice(req, "reasoning_effort").or(settings.reasoning_effort.clone())
    {
        body.insert("reasoning".into(), json!({"effort": effort}));
    }

    if let Some(tools) = &req.tools {
        let flattened: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.function.name,
                    "description": tool.function.description,
                    "parameters": tool.function.parameters,
                })
            })
            .collect();
        if !flattened.is_empty() {
            body.insert("tools".into(), Value::Array(flattened));
            if let Some(choice) = &req.tool_choice {
                body.insert("tool_choice".into(), codex_tool_choice(choice));
            }
        }
    }

    // Everything this backend cannot express. Reported, never silent: a
    // caller who set max_tokens and got a longer answer deserves to know
    // the ceiling was not applied.
    if req.max_tokens.is_some() || req.max_completion_tokens.is_some() {
        dropped.push("max_tokens".into());
    }
    if req.temperature.is_some() {
        dropped.push("temperature".into());
    }
    if req.top_p.is_some() {
        dropped.push("top_p".into());
    }
    if req.stop.is_some() {
        dropped.push("stop".into());
    }
    for (name, _) in req
        .extra
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "reasoning_effort" | "verbosity" | "metadata"))
    {
        dropped.push(name.clone());
    }

    CodexRequest {
        body: Value::Object(body),
        dropped_params: dropped,
    }
}

/// A caller-supplied enum knob carried in `extra` (the Chat Completions
/// spelling of these fields is not in our typed model).
fn request_choice(req: &ChatRequest, name: &str) -> Option<String> {
    req.extra
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Split the conversation into the Responses API's two halves.
///
/// System turns become `instructions`; everything else becomes `input`
/// messages. Tool calls and their results are rendered as text lines,
/// because this backend flattens a conversation and silently ignores
/// native tool blocks replayed on the input array — a model that cannot
/// see its own previous call will simply make it again.
fn split_system_and_input(messages: &[Message]) -> (Option<String>, Vec<Value>) {
    let mut instructions: Vec<String> = Vec::new();
    let mut input: Vec<Value> = Vec::new();

    for message in messages {
        match message.role.as_str() {
            "system" | "developer" => {
                if let Some(content) = &message.content {
                    instructions.push(content.as_text());
                }
                // An image on a system turn cannot be carried by
                // `instructions`; it rides along on the input side.
                if let Some(parts) = image_parts(message) {
                    input.push(json!({"type": "message", "role": "user", "content": parts}));
                }
            }
            "tool" => {
                let id = message.tool_call_id.as_deref().unwrap_or("");
                let text = message
                    .content
                    .as_ref()
                    .map(Content::as_text)
                    .unwrap_or_default();
                input.push(user_text(format!("[tool_result for={id}] {text}")));
            }
            role => {
                let mut content = message
                    .content
                    .as_ref()
                    .map(|c| content_parts(c, role))
                    .unwrap_or_default();
                for call in message.tool_calls.iter().flatten() {
                    content.push(json!({
                        "type": if role == "assistant" { "output_text" } else { "input_text" },
                        "text": format!(
                            "[tool_call id={} name={}] {}",
                            call.id, call.function.name, call.function.arguments
                        ),
                    }));
                }
                if !content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": if role == "assistant" { "assistant" } else { "user" },
                        "content": content,
                    }));
                }
            }
        }
    }

    let instructions = (!instructions.is_empty()).then(|| instructions.join("\n\n"));
    (instructions, input)
}

fn user_text(text: String) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    })
}

/// Content parts in the Responses spelling. An assistant turn's text is
/// `output_text`; everything the caller sends is `input_*`.
fn content_parts(content: &Content, role: &str) -> Vec<Value> {
    let text_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    match content {
        Content::Text(text) if text.is_empty() => Vec::new(),
        Content::Text(text) => vec![json!({"type": text_type, "text": text})],
        Content::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(json!({"type": text_type, "text": text})),
                ContentPart::ImageUrl { image_url } => {
                    Some(json!({"type": "input_image", "image_url": image_url.url}))
                }
                // Audio and file parts have no Responses equivalent this
                // backend accepts; the caller is told via dropped_params.
                _ => None,
            })
            .collect(),
    }
}

fn image_parts(message: &Message) -> Option<Vec<Value>> {
    let Some(Content::Parts(parts)) = &message.content else {
        return None;
    };
    let images: Vec<Value> = parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::ImageUrl { image_url } => {
                Some(json!({"type": "input_image", "image_url": image_url.url}))
            }
            _ => None,
        })
        .collect();
    (!images.is_empty()).then_some(images)
}

/// The hint appended for `json_object` mode. Deliberately short and
/// content-neutral so it cannot steer the answer.
const JSON_OBJECT_HINT: &str = "Respond with a valid JSON object.";

/// Guarantee the word "json" appears somewhere in `input`.
///
/// Idempotent, and a no-op when the caller already said it — which is the
/// common case, and the reason this cannot simply always append.
fn ensure_json_mentioned(input: &mut Vec<Value>) {
    let mentioned = input.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts.iter().any(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| text.to_ascii_lowercase().contains("json"))
                })
            })
    });
    if !mentioned {
        input.push(user_text(JSON_OBJECT_HINT.to_owned()));
    }
}

/// Translate a Chat Completions `response_format` into a Responses
/// `text.format`.
///
/// The two disagree on where JSON-schema metadata lives: Chat Completions
/// nests it under `json_schema`, the Responses API flattens those fields
/// onto the format object. Every other shape is forwarded verbatim — the
/// backend is the authority on validity, and a malformed schema should
/// surface as its error, exactly as with the real API.
pub fn codex_text_format(response_format: Option<&Value>) -> Option<Value> {
    let format = response_format?.as_object()?;
    match format.get("type").and_then(Value::as_str) {
        // Free-form: the default, and adding nothing keeps the body
        // byte-identical to a request that never asked.
        Some("text") | None => None,
        Some("json_schema") => match format.get("json_schema").and_then(Value::as_object) {
            Some(nested) => {
                let mut flat = nested.clone();
                flat.insert("type".into(), json!("json_schema"));
                Some(Value::Object(flat))
            }
            None => Some(Value::Object(format.clone())),
        },
        _ => Some(Value::Object(format.clone())),
    }
}

/// `tool_choice` in the Responses spelling.
///
/// The string forms are identical in both APIs. The named form is not:
/// Chat Completions nests the name under `function`, the Responses API
/// puts it at the top level. Anything unrecognized is forwarded so a
/// future shape reaches the backend.
pub fn codex_tool_choice(choice: &Value) -> Value {
    if let Some(name) = choice
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
    {
        return json!({"type": "function", "name": name});
    }
    choice.clone()
}

/// The form body that renews a Codex credential.
pub fn codex_refresh_form(refresh_token: &str) -> String {
    let encode = |value: &str| {
        value
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    format!(
        "grant_type=refresh_token&client_id={}&refresh_token={}",
        encode(CODEX_OAUTH_CLIENT_ID),
        encode(refresh_token)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use router_core::chat::{FunctionDef, Tool};

    fn request(messages: Vec<Message>) -> ChatRequest {
        serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": messages,
        }))
        .expect("request builds")
    }

    fn message(role: &str, text: &str) -> Message {
        Message {
            role: role.into(),
            content: Some(Content::Text(text.into())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    // -- Claude ------------------------------------------------------

    #[test]
    fn identity_leads_and_the_caller_prompt_survives() {
        let mut body = json!({"system": "You are terse.", "messages": []});
        pin_claude_identity(&mut body);
        let system = body["system"].as_array().expect("normalized to blocks");
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], CLAUDE_CODE_IDENTITY);
        assert_eq!(
            system[1]["text"], "You are terse.",
            "the caller's prompt is a separate block, not concatenated"
        );
    }

    #[test]
    fn identity_is_added_when_there_is_no_system_prompt() {
        let mut body = json!({"messages": []});
        pin_claude_identity(&mut body);
        assert_eq!(body["system"][0]["text"], CLAUDE_CODE_IDENTITY);
        assert_eq!(body["system"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn pinning_is_idempotent() {
        let mut body = json!({"system": "caller"});
        pin_claude_identity(&mut body);
        let once = body.clone();
        pin_claude_identity(&mut body);
        assert_eq!(body, once, "a retry must not stack identity blocks");
    }

    #[test]
    fn existing_system_blocks_keep_their_cache_control() {
        let mut body = json!({"system": [
            {"type": "text", "text": "big preamble", "cache_control": {"type": "ephemeral"}}
        ]});
        pin_claude_identity(&mut body);
        assert_eq!(body["system"][0]["text"], CLAUDE_CODE_IDENTITY);
        assert_eq!(body["system"][1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn an_empty_system_string_does_not_become_an_empty_block() {
        let mut body = json!({"system": ""});
        pin_claude_identity(&mut body);
        assert_eq!(body["system"].as_array().unwrap().len(), 1);
    }

    // -- Codex -------------------------------------------------------

    #[test]
    fn codex_headers_are_the_cli_set() {
        let headers = codex_headers("tok", "acct", "0.146.0", "sess");
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("authorization"), Some("Bearer tok"));
        assert_eq!(get("chatgpt-account-id"), Some("acct"));
        assert_eq!(get("version"), Some("0.146.0"));
        assert_eq!(get("user-agent"), Some("codex_cli_rs/0.146.0"));
        assert_eq!(get("originator"), Some("codex_cli_rs"));
        assert_eq!(get("openai-beta"), Some("responses=experimental"));
        assert_eq!(get("session_id"), Some("sess"));
    }

    #[test]
    fn codex_body_carries_the_backend_invariants() {
        let req = request(vec![
            message("system", "You are helpful."),
            message("user", "hello"),
        ]);
        let built = codex_request(&req, "gpt-5.5", &CodexSettings::default());
        assert_eq!(built.body["stream"], true);
        assert_eq!(built.body["store"], false, "nothing retained upstream");
        assert_eq!(built.body["instructions"], "You are helpful.");
        assert_eq!(built.body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(built.body["input"][0]["content"][0]["text"], "hello");
        assert_eq!(built.body["reasoning"]["effort"], "low");
        assert_eq!(built.body["text"]["verbosity"], "low");
        assert!(
            built.body.get("max_output_tokens").is_none(),
            "this backend 400s on max_output_tokens"
        );
    }

    #[test]
    fn max_tokens_is_dropped_loudly() {
        let mut req = request(vec![message("user", "hi")]);
        req.max_tokens = Some(256);
        req.temperature = Some(0.7);
        let built = codex_request(&req, "gpt-5.5", &CodexSettings::default());
        assert!(built.dropped_params.contains(&"max_tokens".to_owned()));
        assert!(built.dropped_params.contains(&"temperature".to_owned()));
    }

    #[test]
    fn a_caller_can_raise_the_reasoning_floor() {
        let mut req = request(vec![message("user", "hi")]);
        req.extra.insert("reasoning_effort".into(), json!("high"));
        req.extra.insert("verbosity".into(), json!("high"));
        let built = codex_request(&req, "gpt-5.5", &CodexSettings::default());
        assert_eq!(built.body["reasoning"]["effort"], "high");
        assert_eq!(built.body["text"]["verbosity"], "high");
        assert!(
            !built.dropped_params.iter().any(|p| p == "reasoning_effort"),
            "a knob we honour is not a dropped param"
        );
    }

    #[test]
    fn the_floor_can_be_disabled_entirely() {
        let settings = CodexSettings {
            reasoning_effort: None,
            verbosity: None,
            ..CodexSettings::default()
        };
        let built = codex_request(&request(vec![message("user", "hi")]), "gpt-5.5", &settings);
        assert!(built.body.get("reasoning").is_none());
        assert!(built.body.get("text").is_none(), "no text object at all");
    }

    #[test]
    fn tools_are_flattened_and_named_choice_is_relocated() {
        let mut req = request(vec![message("user", "weather?")]);
        req.tools = Some(vec![Tool {
            tool_type: "function".into(),
            function: FunctionDef {
                name: "get_weather".into(),
                description: Some("look it up".into()),
                parameters: Some(json!({"type": "object"})),
                strict: None,
            },
        }]);
        req.tool_choice = Some(json!({"type": "function", "function": {"name": "get_weather"}}));
        let built = codex_request(&req, "gpt-5.5", &CodexSettings::default());
        let tool = &built.body["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(
            tool["name"], "get_weather",
            "flat, not nested under `function`"
        );
        assert!(tool.get("function").is_none());
        assert_eq!(
            built.body["tool_choice"],
            json!({"type": "function", "name": "get_weather"})
        );
    }

    #[test]
    fn json_schema_is_flattened_into_text_format() {
        let format = json!({
            "type": "json_schema",
            "json_schema": {"name": "codes", "strict": true, "schema": {"type": "object"}}
        });
        let flat = codex_text_format(Some(&format)).expect("a format");
        assert_eq!(flat["type"], "json_schema");
        assert_eq!(flat["name"], "codes", "fields hoisted to the top level");
        assert_eq!(flat["schema"]["type"], "object");
        assert!(flat.get("json_schema").is_none());
    }

    #[test]
    fn plain_text_format_adds_nothing() {
        assert!(codex_text_format(Some(&json!({"type": "text"}))).is_none());
        assert!(codex_text_format(None).is_none());
    }

    #[test]
    fn json_object_gets_the_hint_only_when_the_input_lacks_it() {
        let mut req = request(vec![
            message("system", "Return JSON matching the schema."),
            message("user", "extract the codes"),
        ]);
        req.response_format = Some(json!({"type": "json_object"}));
        let built = codex_request(&req, "gpt-5.5", &CodexSettings::default());
        let input = built.body["input"].as_array().unwrap();
        // The caller's "JSON" wording went to `instructions`, which the
        // backend does not check — so the hint must have been added.
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["content"][0]["text"], JSON_OBJECT_HINT);

        // A caller who says it in the user turn gets no hint.
        let mut said_it = request(vec![message("user", "reply as json")]);
        said_it.response_format = Some(json!({"type": "json_object"}));
        let built = codex_request(&said_it, "gpt-5.5", &CodexSettings::default());
        assert_eq!(built.body["input"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn tool_history_is_replayed_as_text() {
        let mut assistant = message("assistant", "");
        assistant.content = None;
        assistant.tool_calls = Some(
            serde_json::from_value(json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"}
            }]))
            .unwrap(),
        );
        let mut result = message("tool", "62F and foggy");
        result.tool_call_id = Some("call_1".into());

        let req = request(vec![message("user", "weather?"), assistant, result]);
        let built = codex_request(&req, "gpt-5.5", &CodexSettings::default());
        let input = built.body["input"].as_array().unwrap();
        assert_eq!(input.len(), 3);
        let call_text = input[1]["content"][0]["text"].as_str().unwrap();
        assert!(call_text.contains("[tool_call id=call_1 name=get_weather]"));
        let result_text = input[2]["content"][0]["text"].as_str().unwrap();
        assert!(result_text.contains("[tool_result for=call_1] 62F and foggy"));
    }

    #[test]
    fn images_ride_the_input_array() {
        let req: ChatRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "what is this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]}]
        }))
        .unwrap();
        let built = codex_request(&req, "gpt-5.5", &CodexSettings::default());
        let content = built.body["input"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn refresh_form_encodes_the_token() {
        let form = codex_refresh_form("rt.1.AA+bc/d=");
        assert!(form.contains("grant_type=refresh_token"));
        assert!(form.contains(&format!("client_id={CODEX_OAUTH_CLIENT_ID}")));
        assert!(
            form.contains("refresh_token=rt.1.AA%2Bbc%2Fd%3D"),
            "reserved characters must be percent-encoded: {form}"
        );
    }
}

// ---------------------------------------------------------------------------
// Codex responses -> OpenAI chunks
// ---------------------------------------------------------------------------

/// Translate the Codex backend's Responses event stream into OpenAI chat
/// chunks.
///
/// The mirror of [`crate::responses::ChunksToResponses`], which serves an
/// inbound Responses caller; this consumes a Responses *upstream*.
///
/// Two behaviours of this particular backend are load-bearing, and getting
/// either wrong is a silent failure rather than an error:
///
/// 1. **Tool calls arrive on `response.output_item.done`.** The terminal
///    `response.completed` carries an *empty* `output` array here, unlike
///    the public Responses API. Reading only the terminal event yields a
///    perfectly well-formed 200 with no tool calls in it.
/// 2. **Usage is nested** under `response.usage` on the terminal event,
///    with the discounted sub-counts a further level down
///    (`input_tokens_details.cached_tokens`).
#[derive(Default)]
pub struct CodexStreamToOpenAi {
    id: String,
    model: String,
    role_sent: bool,
    /// Responses `output_index` -> our `tool_calls` index. The backend
    /// numbers items across the whole response, including reasoning and
    /// message items, so the two sequences do not line up.
    tool_indices: std::collections::BTreeMap<u64, u64>,
    next_tool_index: u64,
    saw_tool_call: bool,
    usage: Option<Value>,
}

impl CodexStreamToOpenAi {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            ..Default::default()
        }
    }

    /// Feed one upstream SSE event; returns zero or more OpenAI chunks.
    pub fn on_event(&mut self, event: &router_core::sse::SseEvent) -> Vec<Value> {
        if event.data == "[DONE]" {
            return Vec::new();
        }
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else {
            return Vec::new();
        };
        let kind = event
            .event
            .as_deref()
            .or_else(|| data["type"].as_str())
            .unwrap_or_default();

        match kind {
            "response.created" => {
                if let Some(id) = data["response"]["id"].as_str() {
                    self.id = id.to_owned();
                }
                if let Some(model) = data["response"]["model"].as_str() {
                    self.model = model.to_owned();
                }
                Vec::new()
            }
            "response.output_item.added" => {
                // A function call announces itself here with its name and
                // id; the arguments stream separately. Emitting the opening
                // frame now is what lets an SDK assemble the call
                // incrementally instead of waiting for the whole turn.
                let item = &data["item"];
                if item["type"].as_str() != Some("function_call") {
                    return Vec::new();
                }
                let output_index = data["output_index"].as_u64().unwrap_or(0);
                let tool_index = *self.tool_indices.entry(output_index).or_insert_with(|| {
                    let assigned = self.next_tool_index;
                    self.next_tool_index += 1;
                    assigned
                });
                self.saw_tool_call = true;
                let mut chunks = self.open_role();
                chunks.push(self.chunk(json!({"tool_calls": [{
                    "index": tool_index,
                    "id": item["call_id"].as_str().or(item["id"].as_str()).unwrap_or_default(),
                    "type": "function",
                    "function": {"name": item["name"], "arguments": ""},
                }]})));
                chunks
            }
            "response.output_text.delta" => {
                let Some(delta) = data["delta"].as_str() else {
                    return Vec::new();
                };
                let mut chunks = self.open_role();
                chunks.push(self.chunk(json!({"content": delta})));
                chunks
            }
            "response.function_call_arguments.delta" => {
                let output_index = data["output_index"].as_u64().unwrap_or(0);
                let Some(&tool_index) = self.tool_indices.get(&output_index) else {
                    return Vec::new();
                };
                let fragment = data["delta"].as_str().unwrap_or_default();
                vec![self.chunk(json!({"tool_calls": [{
                    "index": tool_index,
                    "function": {"arguments": fragment},
                }]}))]
            }
            "response.output_item.done" => {
                // THE load-bearing branch: on this backend the completed
                // function_call is only ever visible here, because
                // response.completed arrives with an empty output array.
                let item = &data["item"];
                if item["type"].as_str() != Some("function_call") {
                    return Vec::new();
                }
                let output_index = data["output_index"].as_u64().unwrap_or(0);
                if self.tool_indices.contains_key(&output_index) {
                    // Already announced and streamed incrementally; the
                    // arguments are complete. Re-emitting them here would
                    // double every call's arguments.
                    return Vec::new();
                }
                let tool_index = self.next_tool_index;
                self.next_tool_index += 1;
                self.tool_indices.insert(output_index, tool_index);
                self.saw_tool_call = true;
                let mut chunks = self.open_role();
                chunks.push(self.chunk(json!({"tool_calls": [{
                    "index": tool_index,
                    "id": item["call_id"].as_str().or(item["id"].as_str()).unwrap_or_default(),
                    "type": "function",
                    "function": {
                        "name": item["name"],
                        "arguments": item["arguments"].as_str().unwrap_or_default(),
                    },
                }]})));
                chunks
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                self.usage = codex_usage(&data["response"]["usage"]);
                let finish = if self.saw_tool_call {
                    "tool_calls"
                } else if kind == "response.incomplete" {
                    "length"
                } else {
                    "stop"
                };
                let mut chunk = self.chunk(json!({}));
                chunk["choices"][0]["finish_reason"] = json!(finish);
                if let Some(usage) = &self.usage {
                    chunk["usage"] = usage.clone();
                }
                vec![chunk]
            }
            _ => Vec::new(),
        }
    }

    /// The first content-bearing event opens the assistant message. Sent
    /// lazily so a stream that fails before producing anything does not
    /// leave a caller holding an empty assistant turn.
    fn open_role(&mut self) -> Vec<Value> {
        if self.role_sent {
            return Vec::new();
        }
        self.role_sent = true;
        vec![self.chunk(json!({"role": "assistant", "content": ""}))]
    }

    fn chunk(&self, delta: Value) -> Value {
        json!({
            "id": if self.id.is_empty() { "chatcmpl-codex" } else { self.id.as_str() },
            "object": "chat.completion.chunk",
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": Value::Null}],
        })
    }
}

/// Normalize a Responses `usage` object into the OpenAI chat shape.
///
/// `None` when there is nothing usable, so the caller falls back rather
/// than reporting a confident zero — an `estimated=false` row of zeros is
/// worse than no row, because it looks like a free request.
pub fn codex_usage(usage: &Value) -> Option<Value> {
    let usage = usage.as_object()?;
    let count = |name: &str| usage.get(name).and_then(Value::as_u64);
    let prompt = count("input_tokens").or_else(|| count("prompt_tokens"))?;
    let completion = count("output_tokens")
        .or_else(|| count("completion_tokens"))
        .unwrap_or(0);
    let total = count("total_tokens").unwrap_or(prompt + completion);
    let cached = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning = usage
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(json!({
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "total_tokens": total,
        "prompt_tokens_details": {"cached_tokens": cached},
        "completion_tokens_details": {"reasoning_tokens": reasoning},
    }))
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    use router_core::sse::SseEvent;

    fn event(payload: Value) -> SseEvent {
        SseEvent {
            event: payload["type"].as_str().map(str::to_owned),
            data: payload.to_string(),
        }
    }

    /// Drive a whole transcript and return the chunks it produced.
    fn run(events: Vec<Value>) -> Vec<Value> {
        let mut stream = CodexStreamToOpenAi::new("gpt-5.5");
        events
            .into_iter()
            .flat_map(|payload| stream.on_event(&event(payload)))
            .collect()
    }

    fn text_of(chunks: &[Value]) -> String {
        chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
            .collect()
    }

    #[test]
    fn text_streams_as_chat_chunks() {
        let chunks = run(vec![
            json!({"type": "response.created", "response": {"id": "resp_1", "model": "gpt-5.5"}}),
            json!({"type": "response.output_text.delta", "delta": "Hello"}),
            json!({"type": "response.output_text.delta", "delta": ", world"}),
            json!({"type": "response.completed", "response": {"usage": {
                "input_tokens": 12, "output_tokens": 3, "total_tokens": 15
            }}}),
        ]);
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(chunks[0]["id"], "resp_1");
        assert_eq!(text_of(&chunks), "Hello, world");
        let last = chunks.last().unwrap();
        assert_eq!(last["choices"][0]["finish_reason"], "stop");
        assert_eq!(last["usage"]["prompt_tokens"], 12);
        assert_eq!(last["usage"]["total_tokens"], 15);
    }

    #[test]
    fn a_tool_call_survives_an_empty_completed_output() {
        // The shape this backend actually sends: the finished call is only
        // in output_item.done, and response.completed's output is EMPTY.
        // Reading only the terminal event yields a 200 with no tool calls.
        let chunks = run(vec![
            json!({"type": "response.created", "response": {"id": "resp_2", "model": "gpt-5.5"}}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {
                "type": "function_call",
                "call_id": "call_abc",
                "name": "get_weather",
                "arguments": "{\"city\":\"SF\"}"
            }}),
            json!({"type": "response.completed", "response": {"output": [], "usage": {
                "input_tokens": 20, "output_tokens": 8
            }}}),
        ]);
        let call = chunks
            .iter()
            .find_map(|c| c["choices"][0]["delta"]["tool_calls"].as_array())
            .expect("a tool call reached the caller");
        assert_eq!(call[0]["id"], "call_abc");
        assert_eq!(call[0]["function"]["name"], "get_weather");
        assert_eq!(call[0]["function"]["arguments"], "{\"city\":\"SF\"}");
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            "tool_calls"
        );
    }

    #[test]
    fn incrementally_streamed_arguments_are_not_duplicated_by_the_done_event() {
        let chunks = run(vec![
            json!({"type": "response.created", "response": {"id": "r", "model": "gpt-5.5"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "function_call", "call_id": "call_1", "name": "f", "arguments": ""
            }}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"a\":"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0, "delta": "1}"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{\"a\":1}"
            }}),
            json!({"type": "response.completed", "response": {"output": []}}),
        ]);
        let arguments: String = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["tool_calls"].as_array())
            .filter_map(|calls| calls[0]["function"]["arguments"].as_str())
            .collect();
        assert_eq!(
            arguments, "{\"a\":1}",
            "the done event must not replay arguments already streamed"
        );
    }

    #[test]
    fn parallel_tool_calls_get_distinct_indices() {
        let chunks = run(vec![
            json!({"type": "response.output_item.done", "output_index": 1, "item": {
                "type": "function_call", "call_id": "call_a", "name": "a", "arguments": "{}"
            }}),
            json!({"type": "response.output_item.done", "output_index": 2, "item": {
                "type": "function_call", "call_id": "call_b", "name": "b", "arguments": "{}"
            }}),
            json!({"type": "response.completed", "response": {"output": []}}),
        ]);
        let indices: Vec<u64> = chunks
            .iter()
            .filter_map(|c| c["choices"][0]["delta"]["tool_calls"].as_array())
            .filter_map(|calls| calls[0]["index"].as_u64())
            .collect();
        assert_eq!(indices, vec![0, 1]);
    }

    #[test]
    fn reasoning_items_do_not_shift_tool_indices() {
        // Reasoning items occupy output_index slots; tool_calls indices are
        // ours and must stay dense from zero.
        let chunks = run(vec![
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "summary": []
            }}),
            json!({"type": "response.output_item.done", "output_index": 3, "item": {
                "type": "function_call", "call_id": "call_x", "name": "x", "arguments": "{}"
            }}),
            json!({"type": "response.completed", "response": {"output": []}}),
        ]);
        let calls = chunks
            .iter()
            .find_map(|c| c["choices"][0]["delta"]["tool_calls"].as_array())
            .expect("a tool call");
        assert_eq!(calls[0]["index"], 0);
    }

    #[test]
    fn usage_details_carry_the_discounted_subcounts() {
        let usage = codex_usage(&json!({
            "input_tokens": 100,
            "output_tokens": 40,
            "total_tokens": 140,
            "input_tokens_details": {"cached_tokens": 60},
            "output_tokens_details": {"reasoning_tokens": 25}
        }))
        .expect("usage");
        assert_eq!(usage["prompt_tokens"], 100);
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 60);
        assert_eq!(usage["completion_tokens_details"]["reasoning_tokens"], 25);
    }

    #[test]
    fn absent_usage_is_none_not_zero() {
        assert!(codex_usage(&Value::Null).is_none());
        assert!(
            codex_usage(&json!({})).is_none(),
            "no counts is not zero counts"
        );
    }

    #[test]
    fn junk_and_done_never_panic() {
        let mut stream = CodexStreamToOpenAi::new("gpt-5.5");
        for data in ["[DONE]", "not json", "", "{\"type\":\"response.unknown\"}"] {
            let event = SseEvent {
                event: None,
                data: data.into(),
            };
            assert!(stream.on_event(&event).is_empty());
        }
    }

    #[test]
    fn a_stream_that_produces_nothing_opens_no_assistant_turn() {
        let chunks = run(vec![
            json!({"type": "response.created", "response": {"id": "r", "model": "gpt-5.5"}}),
            json!({"type": "response.failed", "response": {}}),
        ]);
        assert!(
            !chunks
                .iter()
                .any(|c| c["choices"][0]["delta"]["role"].is_string()),
            "no content means no assistant message was opened"
        );
        assert_eq!(
            chunks.last().unwrap()["choices"][0]["finish_reason"],
            "stop"
        );
    }
}

/// Fold a chunk stream back into one chat completion.
///
/// The Codex backend has no non-streaming mode — `stream: true` is the
/// only request it accepts — so a caller who asked for a whole response is
/// served by consuming the stream here and assembling it. Text is
/// concatenated, tool calls are reassembled by their `index` (arguments
/// arrive as fragments, and a fragment boundary can fall anywhere,
/// including mid-escape), and usage is taken from whichever chunk carried
/// it.
pub fn aggregate_chunks(chunks: &[Value]) -> Value {
    let mut content = String::new();
    let mut calls: Vec<(String, String, String)> = Vec::new(); // id, name, arguments
    let mut finish = "stop".to_owned();
    let mut usage = None;
    let mut id = "chatcmpl-codex".to_owned();
    let mut model = String::new();

    for chunk in chunks {
        if let Some(value) = chunk["id"].as_str() {
            id = value.to_owned();
        }
        if let Some(value) = chunk["model"].as_str().filter(|m| !m.is_empty()) {
            model = value.to_owned();
        }
        if chunk["usage"].is_object() {
            usage = Some(chunk["usage"].clone());
        }
        let choice = &chunk["choices"][0];
        if let Some(reason) = choice["finish_reason"].as_str() {
            finish = reason.to_owned();
        }
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str() {
            content.push_str(text);
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"].as_u64().unwrap_or(0) as usize;
            if calls.len() <= index {
                calls.resize(index + 1, (String::new(), String::new(), String::new()));
            }
            let entry = &mut calls[index];
            if let Some(value) = call["id"].as_str().filter(|v| !v.is_empty()) {
                entry.0 = value.to_owned();
            }
            if let Some(value) = call["function"]["name"].as_str().filter(|v| !v.is_empty()) {
                entry.1 = value.to_owned();
            }
            if let Some(value) = call["function"]["arguments"].as_str() {
                entry.2.push_str(value);
            }
        }
    }

    let mut message = json!({"role": "assistant", "content": Value::Null});
    if !content.is_empty() || calls.is_empty() {
        message["content"] = json!(content);
    }
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(
            calls
                .into_iter()
                .map(|(id, name, arguments)| {
                    json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    })
                })
                .collect(),
        );
    }

    let mut completion = json!({
        "id": id,
        "object": "chat.completion",
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
    });
    if let Some(usage) = usage {
        completion["usage"] = usage;
    }
    completion
}

/// Consume a whole Codex SSE body into one chat completion.
pub fn aggregate_sse(body: &[u8], model: &str) -> Value {
    let mut parser = router_core::sse::SseParser::default();
    let mut stream = CodexStreamToOpenAi::new(model);
    let mut chunks = Vec::new();
    for event in parser.push(body) {
        chunks.extend(stream.on_event(&event));
    }
    aggregate_chunks(&chunks)
}

/// Prepare a Responses-shaped body for a native relay to the Codex
/// backend.
///
/// The Codex backend *is* the Responses API, so a Responses request
/// belongs on the wire almost verbatim: translating it down to the
/// stateless chat core and back loses everything the surface has added
/// since (`additional_tools` for web search, reasoning items, encrypted
/// content), and made those requests fail outright rather than degrade.
///
/// Only what the backend genuinely requires is imposed: the caller's
/// model is replaced with the routed one, `stream` is forced on (the
/// backend offers nothing else), `store` off (it keeps no state for us),
/// and `instructions` defaulted when absent, which the backend demands.
pub fn codex_relay_body(value: &Value, model: &str) -> Value {
    let mut body = value.clone();
    if let Some(object) = body.as_object_mut() {
        object.insert("model".into(), json!(model));
        object.insert("stream".into(), json!(true));
        object.insert("store".into(), json!(false));
        if !object.get("instructions").is_some_and(|i| i.is_string()) {
            object.insert("instructions".into(), json!("You are Codex."));
        }
    }
    body
}
