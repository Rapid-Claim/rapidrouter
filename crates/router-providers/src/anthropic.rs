//! Anthropic Messages dialect: internal (OpenAI-shaped) requests and
//! responses translated to and from Anthropic's wire format, in both
//! directions, sync and streaming.

use router_core::chat::{ChatRequest, Content, ContentPart, Message, ToolCall};
use router_core::config::presets;
use router_core::sse::SseEvent;
use router_core::{ErrorClass, GatewayError};
use serde_json::{Map, Value, json};

pub const VERSION_HEADER: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u64 = 4096;
/// The tool name used to emulate `response_format: json_schema`.
const JSON_SCHEMA_TOOL: &str = "__rapid_json_output";

// ---------------------------------------------------------------------------
// internal -> Anthropic request
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct BuiltRequest {
    pub body: Value,
    pub dropped_params: Vec<String>,
    /// Set when json_schema output is emulated via a forced tool; the
    /// response translator uses it to fold the tool call back into
    /// content.
    pub json_schema_emulated: bool,
}

pub fn build_request(req: &ChatRequest, model: &str) -> Result<BuiltRequest, GatewayError> {
    if req.n.is_some_and(|n| n > 1) {
        return Err(GatewayError::new(
            ErrorClass::InvalidRequest,
            "`n > 1` is not supported by Anthropic models",
        )
        .with_param("n"));
    }
    if req.logprobs == Some(true) {
        return Err(GatewayError::new(
            ErrorClass::InvalidRequest,
            "`logprobs` is not supported by Anthropic models",
        )
        .with_param("logprobs"));
    }

    let mut dropped = Vec::new();
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert(
        "max_tokens".into(),
        json!(req.effective_max_tokens().unwrap_or(DEFAULT_MAX_TOKENS)),
    );

    // System prompt: every system/developer message, concatenated.
    let system: Vec<String> = req
        .messages
        .iter()
        .filter(|m| m.role == "system" || m.role == "developer")
        .filter_map(|m| m.content.as_ref().map(Content::as_text))
        .collect();
    if !system.is_empty() {
        body.insert("system".into(), json!(system.join("\n\n")));
    }

    body.insert(
        "messages".into(),
        Value::Array(translate_messages(&req.messages)?),
    );

    if let Some(t) = req.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        body.insert("top_p".into(), json!(p));
    }
    if let Some(stop) = &req.stop {
        let sequences = match stop {
            Value::String(s) => json!([s]),
            other => other.clone(),
        };
        body.insert("stop_sequences".into(), sequences);
    }
    if req.stream == Some(true) {
        body.insert("stream".into(), json!(true));
    }

    let reasoning_effort = req.extra.get("reasoning_effort").and_then(Value::as_str);
    let reasoning_effort_supported =
        reasoning_effort.is_some_and(|effort| presets::reasoning_profile(model).supports(effort));
    if reasoning_effort_supported {
        body.insert(
            "output_config".into(),
            json!({"effort": reasoning_effort.expect("checked above")}),
        );
    }

    let mut tools = req.tools.as_ref().map(|tools| {
        tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.function.name,
                    "description": t.function.description,
                    "input_schema": t.function.parameters.clone().unwrap_or(json!({"type": "object"})),
                })
            })
            .collect::<Vec<_>>()
    });

    let mut tool_choice = req.tool_choice.as_ref().and_then(|choice| match choice {
        Value::String(s) => match s.as_str() {
            "auto" => Some(json!({"type": "auto"})),
            "none" => None, // omit the tools entirely below
            "required" => Some(json!({"type": "any"})),
            _ => Some(json!({"type": "auto"})),
        },
        Value::Object(o) => o["function"]["name"]
            .as_str()
            .map(|name| json!({"type": "tool", "name": name})),
        _ => None,
    });
    if req.tool_choice == Some(json!("none")) {
        tools = None;
    }
    if req.parallel_tool_calls == Some(false)
        && let Some(Value::Object(tc)) = tool_choice.as_mut()
    {
        tc.insert("disable_parallel_tool_use".into(), json!(true));
    }

    // json_schema response_format: emulate with a forced tool whose input
    // schema is the requested schema.
    let mut json_schema_emulated = false;
    if let Some(format) = &req.response_format {
        match format["type"].as_str() {
            Some("json_schema") => {
                let schema = format["json_schema"]["schema"].clone();
                let tool = json!({
                    "name": JSON_SCHEMA_TOOL,
                    "description": "Emit the response as structured output matching the schema.",
                    "input_schema": schema,
                });
                tools.get_or_insert_with(Vec::new).push(tool);
                tool_choice = Some(json!({"type": "tool", "name": JSON_SCHEMA_TOOL}));
                json_schema_emulated = true;
            }
            Some("json_object") | Some("text") | None => {}
            Some(other) => {
                return Err(GatewayError::new(
                    ErrorClass::InvalidRequest,
                    format!("unsupported response_format type `{other}` for Anthropic models"),
                )
                .with_param("response_format"));
            }
        }
    }

    if let Some(tools) = tools {
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = tool_choice {
        body.insert("tool_choice".into(), choice);
    }

    for (param, present) in [
        ("presence_penalty", req.presence_penalty.is_some()),
        ("frequency_penalty", req.frequency_penalty.is_some()),
        ("seed", req.seed.is_some()),
        ("stream_options", req.stream_options.is_some()),
    ] {
        if present {
            dropped.push(param.to_owned());
        }
    }
    for key in req.extra.keys() {
        if key == "reasoning_effort" && reasoning_effort_supported {
            continue;
        }
        // `metadata` is not reported either: the gateway consumed it as
        // this request's attribution, so it was taken deliberately
        // rather than lost to a dialect with nowhere to put it.
        if key == "metadata" {
            continue;
        }
        dropped.push(key.clone());
    }

    Ok(BuiltRequest {
        body: Value::Object(body),
        dropped_params: dropped,
        json_schema_emulated,
    })
}

/// OpenAI messages -> Anthropic messages. Tool results fold into user
/// turns as `tool_result` blocks; assistant tool calls become `tool_use`
/// blocks; consecutive same-role turns merge (Anthropic requires strict
/// alternation).
fn translate_messages(messages: &[Message]) -> Result<Vec<Value>, GatewayError> {
    let mut out: Vec<(String, Vec<Value>)> = Vec::new();

    let mut push = |role: &str, blocks: Vec<Value>| {
        if let Some((last_role, last_blocks)) = out.last_mut()
            && last_role == role
        {
            last_blocks.extend(blocks);
        } else {
            out.push((role.to_owned(), blocks));
        }
    };

    for message in messages {
        match message.role.as_str() {
            "system" | "developer" => {} // hoisted into `system`
            "user" => push("user", content_blocks(message)?),
            "assistant" => {
                let mut blocks = content_blocks(message)?;
                for call in message.tool_calls.iter().flatten() {
                    blocks.push(tool_use_block(call)?);
                }
                push("assistant", blocks);
            }
            "tool" => {
                let id = message.tool_call_id.as_deref().ok_or_else(|| {
                    GatewayError::new(
                        ErrorClass::InvalidRequest,
                        "tool message missing tool_call_id",
                    )
                    .with_param("messages")
                })?;
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": message.content.as_ref().map(Content::as_text).unwrap_or_default(),
                });
                push("user", vec![block]);
            }
            other => {
                return Err(GatewayError::new(
                    ErrorClass::InvalidRequest,
                    format!("unsupported message role `{other}`"),
                )
                .with_param("messages"));
            }
        }
    }

    Ok(out
        .into_iter()
        .filter(|(_, blocks)| !blocks.is_empty())
        .map(|(role, blocks)| json!({"role": role, "content": blocks}))
        .collect())
}

fn content_blocks(message: &Message) -> Result<Vec<Value>, GatewayError> {
    let Some(content) = &message.content else {
        return Ok(Vec::new());
    };
    match content {
        Content::Text(text) => {
            if text.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec![json!({"type": "text", "text": text})])
            }
        }
        Content::Parts(parts) => parts.iter().map(part_to_block).collect(),
    }
}

fn part_to_block(part: &ContentPart) -> Result<Value, GatewayError> {
    match part {
        ContentPart::Text { text } => Ok(json!({"type": "text", "text": text})),
        ContentPart::ImageUrl { image_url } => {
            if let Some(rest) = image_url.url.strip_prefix("data:") {
                let (media_type, data) = rest.split_once(";base64,").ok_or_else(|| {
                    GatewayError::new(
                        ErrorClass::InvalidRequest,
                        "image data URI must be base64-encoded",
                    )
                    .with_param("messages")
                })?;
                Ok(json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": media_type, "data": data}
                }))
            } else {
                Ok(json!({
                    "type": "image",
                    "source": {"type": "url", "url": image_url.url}
                }))
            }
        }
        ContentPart::InputAudio { .. } => Err(GatewayError::new(
            ErrorClass::InvalidRequest,
            "audio content parts are not supported by Anthropic models",
        )
        .with_param("messages")),
        ContentPart::File { file } => {
            // PDF documents: base64 file_data maps to a document block.
            let data = file["file_data"].as_str().unwrap_or_default();
            let data = data
                .strip_prefix("data:application/pdf;base64,")
                .unwrap_or(data);
            Ok(json!({
                "type": "document",
                "source": {"type": "base64", "media_type": "application/pdf", "data": data}
            }))
        }
    }
}

fn tool_use_block(call: &ToolCall) -> Result<Value, GatewayError> {
    let input: Value = if call.function.arguments.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&call.function.arguments).map_err(|e| {
            GatewayError::new(
                ErrorClass::InvalidRequest,
                format!("tool call `{}` has non-JSON arguments: {e}", call.id),
            )
            .with_param("messages")
        })?
    };
    Ok(json!({
        "type": "tool_use",
        "id": call.id,
        "name": call.function.name,
        "input": input,
    }))
}

// ---------------------------------------------------------------------------
// Anthropic response -> internal (OpenAI shape)
// ---------------------------------------------------------------------------

pub fn response_to_openai(body: &Value, json_schema_emulated: bool) -> Value {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in body["content"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
            Some("tool_use") => {
                let name = block["name"].as_str().unwrap_or_default();
                let arguments = block["input"].to_string();
                if json_schema_emulated && name == JSON_SCHEMA_TOOL {
                    text.push_str(&arguments);
                } else {
                    tool_calls.push(json!({
                        "id": block["id"],
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }));
                }
            }
            _ => {}
        }
    }

    let finish_reason = match body["stop_reason"].as_str() {
        _ if json_schema_emulated => "stop",
        Some("tool_use") => "tool_calls",
        Some("max_tokens") => "length",
        _ => "stop",
    };

    let mut message = json!({"role": "assistant", "content": if text.is_empty() && !tool_calls.is_empty() { Value::Null } else { json!(text) }});
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }

    json!({
        "id": body["id"],
        "object": "chat.completion",
        "model": body["model"],
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": {
            "prompt_tokens": body["usage"]["input_tokens"],
            "completion_tokens": body["usage"]["output_tokens"],
            "total_tokens": body["usage"]["input_tokens"].as_u64().unwrap_or(0)
                + body["usage"]["output_tokens"].as_u64().unwrap_or(0),
        },
    })
}

// ---------------------------------------------------------------------------
// Anthropic stream -> internal (OpenAI chunks)
// ---------------------------------------------------------------------------

/// Translates Anthropic's event sequence into OpenAI chunk objects.
/// Carries the cross-event context each Anthropic event omits: block
/// index -> tool-call index mapping and the accumulated usage.
#[derive(Default)]
pub struct StreamToOpenAi {
    id: String,
    model: String,
    /// Anthropic content-block index -> OpenAI tool_calls index.
    tool_indices: std::collections::BTreeMap<u64, u64>,
    next_tool_index: u64,
    role_sent: bool,
    input_tokens: u64,
    finish: Option<&'static str>,
    json_schema_emulated: bool,
}

impl StreamToOpenAi {
    pub fn new(json_schema_emulated: bool) -> Self {
        Self {
            json_schema_emulated,
            ..Default::default()
        }
    }

    /// Feed one upstream SSE event; returns zero or more OpenAI chunks.
    pub fn on_event(&mut self, event: &SseEvent) -> Vec<Value> {
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else {
            return Vec::new();
        };
        let kind = event
            .event
            .as_deref()
            .or(data["type"].as_str())
            .unwrap_or_default();

        match kind {
            "message_start" => {
                self.id = data["message"]["id"].as_str().unwrap_or("msg").to_owned();
                self.model = data["message"]["model"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                self.input_tokens = data["message"]["usage"]["input_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                self.role_sent = true;
                vec![self.chunk(json!({"role": "assistant", "content": ""}), None)]
            }
            "content_block_start" => {
                let index = data["index"].as_u64().unwrap_or(0);
                let block = &data["content_block"];
                match block["type"].as_str() {
                    Some("tool_use") if !self.emulated_tool(block) => {
                        let tool_index = self.next_tool_index;
                        self.tool_indices.insert(index, tool_index);
                        self.next_tool_index += 1;
                        vec![self.chunk(
                            json!({"tool_calls": [{
                                "index": tool_index,
                                "id": block["id"],
                                "type": "function",
                                "function": {"name": block["name"], "arguments": ""},
                            }]}),
                            None,
                        )]
                    }
                    _ => Vec::new(),
                }
            }
            "content_block_delta" => {
                let index = data["index"].as_u64().unwrap_or(0);
                let delta = &data["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        vec![self.chunk(json!({"content": delta["text"]}), None)]
                    }
                    Some("input_json_delta") => {
                        let fragment = delta["partial_json"].as_str().unwrap_or_default();
                        if let Some(&tool_index) = self.tool_indices.get(&index) {
                            vec![self.chunk(
                                json!({"tool_calls": [{
                                    "index": tool_index,
                                    "function": {"arguments": fragment},
                                }]}),
                                None,
                            )]
                        } else if self.json_schema_emulated {
                            // Emulated json_schema: arguments stream as content.
                            vec![self.chunk(json!({"content": fragment}), None)]
                        } else {
                            Vec::new()
                        }
                    }
                    _ => Vec::new(),
                }
            }
            "message_delta" => {
                self.finish = match data["delta"]["stop_reason"].as_str() {
                    _ if self.json_schema_emulated => Some("stop"),
                    Some("tool_use") => Some("tool_calls"),
                    Some("max_tokens") => Some("length"),
                    _ => Some("stop"),
                };
                let output = data["usage"]["output_tokens"].as_u64().unwrap_or(0);
                let usage = json!({
                    "prompt_tokens": self.input_tokens,
                    "completion_tokens": output,
                    "total_tokens": self.input_tokens + output,
                });
                vec![self.chunk_with_usage(json!({}), self.finish, Some(usage))]
            }
            _ => Vec::new(), // ping, message_stop, content_block_stop
        }
    }

    fn emulated_tool(&mut self, block: &Value) -> bool {
        self.json_schema_emulated && block["name"].as_str() == Some(JSON_SCHEMA_TOOL)
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        self.chunk_with_usage(delta, finish, None)
    }

    fn chunk_with_usage(&self, delta: Value, finish: Option<&str>, usage: Option<Value>) -> Value {
        let mut chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        });
        if let Some(usage) = usage {
            chunk["usage"] = usage;
        }
        chunk
    }
}

// ---------------------------------------------------------------------------
// Inbound: Anthropic request -> internal
// ---------------------------------------------------------------------------

pub fn request_to_internal(body: &Value) -> Result<ChatRequest, GatewayError> {
    let bad = |msg: String| GatewayError::new(ErrorClass::InvalidRequest, msg);

    let model = body["model"]
        .as_str()
        .ok_or_else(|| bad("`model` is required".into()).with_param("model"))?
        .to_owned();

    let mut messages: Vec<Message> = Vec::new();
    if let Some(system) = body.get("system") {
        let text = match system {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
            _ => String::new(),
        };
        if !text.is_empty() {
            messages.push(Message {
                role: "system".into(),
                content: Some(Content::Text(text)),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
    }

    for m in body["messages"].as_array().into_iter().flatten() {
        let role = m["role"].as_str().unwrap_or("user");
        match &m["content"] {
            Value::String(text) => messages.push(simple_message(role, text.clone())),
            Value::Array(blocks) => {
                let mut parts: Vec<ContentPart> = Vec::new();
                let mut tool_calls: Vec<ToolCall> = Vec::new();
                for block in blocks {
                    match block["type"].as_str() {
                        Some("text") => parts.push(ContentPart::Text {
                            text: block["text"].as_str().unwrap_or_default().to_owned(),
                        }),
                        Some("image") => {
                            let source = &block["source"];
                            let url = match source["type"].as_str() {
                                Some("base64") => format!(
                                    "data:{};base64,{}",
                                    source["media_type"].as_str().unwrap_or("image/png"),
                                    source["data"].as_str().unwrap_or_default()
                                ),
                                _ => source["url"].as_str().unwrap_or_default().to_owned(),
                            };
                            parts.push(ContentPart::ImageUrl {
                                image_url: router_core::chat::ImageUrl { url, detail: None },
                            });
                        }
                        // A PDF from a Claude-dialect caller. The outbound
                        // side has always built these; without this arm
                        // nothing could read one back, so a document was
                        // dropped before translation even began.
                        Some("document") => {
                            let source = &block["source"];
                            let media = source["media_type"]
                                .as_str()
                                .unwrap_or("application/pdf")
                                .to_owned();
                            let file = match source["type"].as_str() {
                                Some("base64") => json!({
                                    "filename": block["title"],
                                    "file_data": format!(
                                        "data:{media};base64,{}",
                                        source["data"].as_str().unwrap_or_default()
                                    ),
                                }),
                                _ => json!({
                                    "filename": block["title"],
                                    "file_url": source["url"],
                                }),
                            };
                            parts.push(ContentPart::File { file });
                        }
                        Some("tool_use") => tool_calls.push(ToolCall {
                            id: block["id"].as_str().unwrap_or_default().to_owned(),
                            call_type: "function".into(),
                            function: router_core::chat::FunctionCall {
                                name: block["name"].as_str().unwrap_or_default().to_owned(),
                                arguments: block["input"].to_string(),
                            },
                        }),
                        Some("tool_result") => {
                            // Text AND images: a tool that answers with a
                            // screenshot (browser, computer-use, chart
                            // render) carries its whole answer in an image
                            // block, and keeping only `text` handed the
                            // model an empty result it could not act on.
                            let content = match &block["content"] {
                                Value::String(s) => Content::Text(s.clone()),
                                Value::Array(inner) => {
                                    let result: Vec<ContentPart> =
                                        inner.iter().filter_map(result_part).collect();
                                    match result.as_slice() {
                                        [] => Content::Text(String::new()),
                                        [ContentPart::Text { text }] => Content::Text(text.clone()),
                                        _ => Content::Parts(result),
                                    }
                                }
                                _ => Content::Text(String::new()),
                            };
                            messages.push(Message {
                                role: "tool".into(),
                                content: Some(content),
                                tool_calls: None,
                                tool_call_id: block["tool_use_id"].as_str().map(str::to_owned),
                                name: None,
                            });
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() || !tool_calls.is_empty() {
                    messages.push(Message {
                        role: role.into(),
                        content: if parts.is_empty() {
                            None
                        } else {
                            Some(Content::Parts(parts))
                        },
                        tool_calls: if tool_calls.is_empty() {
                            None
                        } else {
                            Some(tool_calls)
                        },
                        tool_call_id: None,
                        name: None,
                    });
                }
            }
            _ => {}
        }
    }

    let tools = body["tools"].as_array().map(|tools| {
        tools
            .iter()
            .map(|t| router_core::chat::Tool {
                tool_type: "function".into(),
                function: router_core::chat::FunctionDef {
                    name: t["name"].as_str().unwrap_or_default().to_owned(),
                    description: t["description"].as_str().map(str::to_owned),
                    parameters: Some(t["input_schema"].clone()),
                    strict: None,
                },
            })
            .collect::<Vec<_>>()
    });

    let tool_choice = body
        .get("tool_choice")
        .and_then(|choice| match choice["type"].as_str() {
            Some("auto") => Some(json!("auto")),
            Some("any") => Some(json!("required")),
            Some("tool") => Some(json!({"type": "function", "function": {"name": choice["name"]}})),
            _ => None,
        });

    Ok(ChatRequest {
        model,
        messages,
        tools,
        tool_choice,
        parallel_tool_calls: None,
        temperature: body["temperature"].as_f64(),
        top_p: body["top_p"].as_f64(),
        max_tokens: body["max_tokens"].as_u64(),
        max_completion_tokens: None,
        stop: body.get("stop_sequences").cloned(),
        n: None,
        stream: body["stream"].as_bool(),
        stream_options: None,
        response_format: None,
        logprobs: None,
        presence_penalty: None,
        frequency_penalty: None,
        seed: None,
        user: None,
        extra: Default::default(),
    })
}

/// One block of a `tool_result`'s content array, in the internal model.
///
/// Text and images only: those are the block types the Messages API
/// permits inside a tool result, and anything else is left out rather than
/// guessed at.
fn result_part(block: &Value) -> Option<ContentPart> {
    match block["type"].as_str() {
        Some("text") => Some(ContentPart::Text {
            text: block["text"].as_str().unwrap_or_default().to_owned(),
        }),
        Some("image") => {
            let source = &block["source"];
            let url = match source["type"].as_str() {
                Some("base64") => format!(
                    "data:{};base64,{}",
                    source["media_type"].as_str().unwrap_or("image/png"),
                    source["data"].as_str().unwrap_or_default()
                ),
                _ => source["url"].as_str().unwrap_or_default().to_owned(),
            };
            Some(ContentPart::ImageUrl {
                image_url: router_core::chat::ImageUrl { url, detail: None },
            })
        }
        _ => None,
    }
}

fn simple_message(role: &str, text: String) -> Message {
    Message {
        role: role.into(),
        content: Some(Content::Text(text)),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    }
}

// ---------------------------------------------------------------------------
// Inbound: internal (OpenAI shape) response -> Anthropic
// ---------------------------------------------------------------------------

pub fn openai_response_to_anthropic(body: &Value) -> Value {
    let message = &body["choices"][0]["message"];
    let mut content = Vec::new();
    if let Some(text) = message["content"].as_str()
        && !text.is_empty()
    {
        content.push(json!({"type": "text", "text": text}));
    }
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        let input: Value = call["function"]["arguments"]
            .as_str()
            .and_then(|a| serde_json::from_str(a).ok())
            .unwrap_or(json!({}));
        content.push(json!({
            "type": "tool_use",
            "id": call["id"],
            "name": call["function"]["name"],
            "input": input,
        }));
    }

    let stop_reason = match body["choices"][0]["finish_reason"].as_str() {
        Some("tool_calls") => "tool_use",
        Some("length") => "max_tokens",
        _ => "end_turn",
    };

    json!({
        "id": body["id"],
        "type": "message",
        "role": "assistant",
        "model": body["model"],
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": body["usage"]["prompt_tokens"],
            "output_tokens": body["usage"]["completion_tokens"],
        },
    })
}

// ---------------------------------------------------------------------------
// Inbound streaming: OpenAI chunks -> Anthropic events
// ---------------------------------------------------------------------------

/// Translates OpenAI chunks into Anthropic's event sequence
/// (message_start, content_block_start/delta/stop, message_delta,
/// message_stop), tracking block indices the way Anthropic clients
/// expect.
#[derive(Default)]
pub struct OpenAiToAnthropicStream {
    started: bool,
    /// Anthropic block index currently open: (index, is_tool).
    open_block: Option<(u64, bool)>,
    next_block_index: u64,
    /// OpenAI tool index -> anthropic block index.
    tool_blocks: std::collections::BTreeMap<u64, u64>,
    finish: Option<String>,
    usage: Option<Value>,
}

impl OpenAiToAnthropicStream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one OpenAI chunk; returns Anthropic `(event, data)` pairs.
    pub fn on_chunk(&mut self, chunk: &Value) -> Vec<(String, Value)> {
        let mut events = Vec::new();
        let choice = &chunk["choices"][0];
        let delta = &choice["delta"];

        if !self.started {
            self.started = true;
            events.push((
                "message_start".to_owned(),
                json!({"type": "message_start", "message": {
                    "id": chunk["id"], "type": "message", "role": "assistant",
                    "model": chunk["model"], "content": [],
                    "stop_reason": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                }}),
            ));
        }

        if let Some(text) = delta["content"].as_str()
            && !text.is_empty()
        {
            match self.open_block {
                Some((_, false)) => {}
                _ => {
                    self.close_open_block(&mut events);
                    let index = self.next_block_index;
                    self.next_block_index += 1;
                    self.open_block = Some((index, false));
                    events.push((
                        "content_block_start".to_owned(),
                        json!({"type": "content_block_start", "index": index,
                               "content_block": {"type": "text", "text": ""}}),
                    ));
                }
            }
            let index = self.open_block.expect("opened above").0;
            events.push((
                "content_block_delta".to_owned(),
                json!({"type": "content_block_delta", "index": index,
                       "delta": {"type": "text_delta", "text": text}}),
            ));
        }

        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let tool_index = call["index"].as_u64().unwrap_or(0);
            if let Some(name) = call["function"]["name"].as_str() {
                self.close_open_block(&mut events);
                let block_index = self.next_block_index;
                self.next_block_index += 1;
                self.tool_blocks.insert(tool_index, block_index);
                self.open_block = Some((block_index, true));
                events.push((
                    "content_block_start".to_owned(),
                    json!({"type": "content_block_start", "index": block_index,
                           "content_block": {"type": "tool_use",
                               "id": call["id"], "name": name, "input": {}}}),
                ));
            }
            if let Some(fragment) = call["function"]["arguments"].as_str()
                && !fragment.is_empty()
                && let Some(&block_index) = self.tool_blocks.get(&tool_index)
            {
                events.push((
                    "content_block_delta".to_owned(),
                    json!({"type": "content_block_delta", "index": block_index,
                           "delta": {"type": "input_json_delta", "partial_json": fragment}}),
                ));
            }
        }

        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(usage.clone());
        }
        if let Some(finish) = choice["finish_reason"].as_str() {
            self.finish = Some(finish.to_owned());
        }
        events
    }

    /// End of the OpenAI stream: close blocks and emit the terminal
    /// events.
    pub fn finish(&mut self) -> Vec<(String, Value)> {
        let mut events = Vec::new();
        self.close_open_block(&mut events);
        let stop_reason = match self.finish.as_deref() {
            Some("tool_calls") => "tool_use",
            Some("length") => "max_tokens",
            _ => "end_turn",
        };
        let output_tokens = self
            .usage
            .as_ref()
            .and_then(|u| u["completion_tokens"].as_u64())
            .unwrap_or(0);
        events.push((
            "message_delta".to_owned(),
            json!({"type": "message_delta",
                   "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                   "usage": {"output_tokens": output_tokens}}),
        ));
        events.push(("message_stop".to_owned(), json!({"type": "message_stop"})));
        events
    }

    fn close_open_block(&mut self, events: &mut Vec<(String, Value)>) {
        if let Some((index, _)) = self.open_block.take() {
            events.push((
                "content_block_stop".to_owned(),
                json!({"type": "content_block_stop", "index": index}),
            ));
        }
    }
}
