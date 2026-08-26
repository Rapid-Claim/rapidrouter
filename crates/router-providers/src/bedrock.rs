//! Bedrock's Converse dialect: internal (OpenAI-shaped) requests and
//! responses translated to and from `Converse` / `ConverseStream`.
//! Streaming arrives as AWS event-stream frames, decoded upstream of this
//! module into `(event-type, JSON payload)` pairs.

use router_core::chat::{ChatRequest, Content, ContentPart, Message, ToolCall};
use router_core::sse::SseEvent;
use router_core::{ErrorClass, GatewayError};
use serde_json::{Map, Value, json};

#[derive(Debug)]
pub struct BuiltRequest {
    pub body: Value,
    pub dropped_params: Vec<String>,
}

pub fn build_request(req: &ChatRequest) -> Result<BuiltRequest, GatewayError> {
    if req.n.is_some_and(|n| n > 1) {
        return Err(GatewayError::new(
            ErrorClass::InvalidRequest,
            "`n > 1` is not supported by Bedrock models",
        )
        .with_param("n"));
    }
    if req.logprobs == Some(true) {
        return Err(GatewayError::new(
            ErrorClass::InvalidRequest,
            "`logprobs` is not supported by Bedrock models",
        )
        .with_param("logprobs"));
    }

    let mut dropped = Vec::new();
    let mut body = Map::new();

    let system: Vec<Value> = req
        .messages
        .iter()
        .filter(|m| m.role == "system" || m.role == "developer")
        .filter_map(|m| m.content.as_ref().map(Content::as_text))
        .filter(|t| !t.is_empty())
        .map(|t| json!({"text": t}))
        .collect();
    if !system.is_empty() {
        body.insert("system".into(), Value::Array(system));
    }

    body.insert(
        "messages".into(),
        Value::Array(translate_messages(&req.messages)?),
    );

    let mut inference = Map::new();
    if let Some(m) = req.effective_max_tokens() {
        inference.insert("maxTokens".into(), json!(m));
    }
    if let Some(t) = req.temperature {
        inference.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        inference.insert("topP".into(), json!(p));
    }
    if let Some(stop) = &req.stop {
        let sequences = match stop {
            Value::String(s) => json!([s]),
            other => other.clone(),
        };
        inference.insert("stopSequences".into(), sequences);
    }
    if !inference.is_empty() {
        body.insert("inferenceConfig".into(), Value::Object(inference));
    }

    if let Some(tools) = &req.tools
        && req.tool_choice != Some(json!("none"))
    {
        let specs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({"toolSpec": {
                    "name": t.function.name,
                    "description": t.function.description,
                    "inputSchema": {"json": t.function.parameters.clone().unwrap_or(json!({"type": "object"}))},
                }})
            })
            .collect();
        let mut tool_config = Map::new();
        tool_config.insert("tools".into(), Value::Array(specs));
        if let Some(choice) = &req.tool_choice {
            let mapped = match choice {
                Value::String(s) => match s.as_str() {
                    "required" => Some(json!({"any": {}})),
                    "auto" => Some(json!({"auto": {}})),
                    _ => None,
                },
                Value::Object(o) => o["function"]["name"]
                    .as_str()
                    .map(|name| json!({"tool": {"name": name}})),
                _ => None,
            };
            if let Some(mapped) = mapped {
                tool_config.insert("toolChoice".into(), mapped);
            }
        }
        body.insert("toolConfig".into(), Value::Object(tool_config));
    }

    for (param, present) in [
        ("presence_penalty", req.presence_penalty.is_some()),
        ("frequency_penalty", req.frequency_penalty.is_some()),
        ("seed", req.seed.is_some()),
        ("response_format", req.response_format.is_some()),
        ("stream_options", req.stream_options.is_some()),
        ("parallel_tool_calls", req.parallel_tool_calls.is_some()),
    ] {
        if present {
            dropped.push(param.to_owned());
        }
    }
    // `metadata` is not reported: the gateway consumed it as this
    // request's attribution, so it was taken deliberately rather than
    // lost to a dialect that had nowhere to put it.
    for key in req.extra.keys().filter(|k| k.as_str() != "metadata") {
        dropped.push(key.clone());
    }

    Ok(BuiltRequest {
        body: Value::Object(body),
        dropped_params: dropped,
    })
}

fn translate_messages(messages: &[Message]) -> Result<Vec<Value>, GatewayError> {
    let mut out: Vec<(String, Vec<Value>)> = Vec::new();
    let mut push = |role: &str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
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
            "system" | "developer" => {}
            "user" => push("user", content_blocks(message)?),
            "assistant" => {
                let mut blocks = content_blocks(message)?;
                for call in message.tool_calls.iter().flatten() {
                    blocks.push(tool_use_block(call)?);
                }
                push("assistant", blocks);
            }
            "tool" => {
                let id = message.tool_call_id.as_deref().unwrap_or_default();
                let text = message
                    .content
                    .as_ref()
                    .map(Content::as_text)
                    .unwrap_or_default();
                let content = match serde_json::from_str::<Value>(&text) {
                    Ok(v) if v.is_object() => json!([{"json": v}]),
                    _ => json!([{"text": text}]),
                };
                push(
                    "user",
                    vec![json!({"toolResult": {"toolUseId": id, "content": content}})],
                );
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
        .map(|(role, content)| json!({"role": role, "content": content}))
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
                Ok(vec![json!({"text": text})])
            }
        }
        Content::Parts(parts) => parts.iter().map(part_to_block).collect(),
    }
}

fn part_to_block(part: &ContentPart) -> Result<Value, GatewayError> {
    match part {
        ContentPart::Text { text } => Ok(json!({"text": text})),
        ContentPart::ImageUrl { image_url } => {
            let rest = image_url.url.strip_prefix("data:").ok_or_else(|| {
                GatewayError::new(
                    ErrorClass::InvalidRequest,
                    "Bedrock image parts require base64 data URIs (no remote URLs)",
                )
                .with_param("messages")
            })?;
            let (media_type, data) = rest.split_once(";base64,").ok_or_else(|| {
                GatewayError::new(ErrorClass::InvalidRequest, "image data URI must be base64")
                    .with_param("messages")
            })?;
            let format = media_type.strip_prefix("image/").unwrap_or("png");
            Ok(json!({"image": {"format": format, "source": {"bytes": data}}}))
        }
        ContentPart::File { file } => {
            let data = file["file_data"].as_str().unwrap_or_default();
            let data = data
                .strip_prefix("data:application/pdf;base64,")
                .unwrap_or(data);
            Ok(
                json!({"document": {"format": "pdf", "name": file["filename"].as_str().unwrap_or("document"),
                       "source": {"bytes": data}}}),
            )
        }
        ContentPart::InputAudio { .. } => Err(GatewayError::new(
            ErrorClass::InvalidRequest,
            "audio content parts are not supported by Bedrock Converse",
        )
        .with_param("messages")),
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
    Ok(json!({"toolUse": {"toolUseId": call.id, "name": call.function.name, "input": input}}))
}

// ---------------------------------------------------------------------------
// Converse response -> internal (OpenAI shape)
// ---------------------------------------------------------------------------

pub fn response_to_openai(body: &Value, model: &str) -> Value {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in body["output"]["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(t) = block["text"].as_str() {
            text.push_str(t);
        }
        if let Some(tool_use) = block.get("toolUse") {
            tool_calls.push(json!({
                "id": tool_use["toolUseId"],
                "type": "function",
                "function": {"name": tool_use["name"], "arguments": tool_use["input"].to_string()},
            }));
        }
    }

    let finish_reason = match body["stopReason"].as_str() {
        Some("tool_use") => "tool_calls",
        Some("max_tokens") => "length",
        Some("content_filtered") => "content_filter",
        _ => "stop",
    };
    let mut message = json!({"role": "assistant",
        "content": if text.is_empty() && !tool_calls.is_empty() { Value::Null } else { json!(text) }});
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }

    json!({
        "id": "chatcmpl-bedrock",
        "object": "chat.completion",
        "model": model,
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": {
            "prompt_tokens": body["usage"]["inputTokens"],
            "completion_tokens": body["usage"]["outputTokens"],
            "total_tokens": body["usage"]["totalTokens"],
        },
    })
}

// ---------------------------------------------------------------------------
// ConverseStream events -> internal (OpenAI chunks)
// ---------------------------------------------------------------------------

/// Bedrock block indices map to OpenAI tool indices exactly like the
/// Anthropic translator; text deltas carry no block bookkeeping we need.
#[derive(Default)]
pub struct StreamToOpenAi {
    model: String,
    role_sent: bool,
    tool_indices: std::collections::BTreeMap<u64, u64>,
    next_tool_index: u64,
    finish: Option<&'static str>,
    saw_tools: bool,
}

impl StreamToOpenAi {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            ..Default::default()
        }
    }

    pub fn on_event(&mut self, event: &SseEvent) -> Vec<Value> {
        let Ok(data) = serde_json::from_str::<Value>(&event.data) else {
            return Vec::new();
        };
        match event.event.as_deref().unwrap_or_default() {
            "messageStart" => {
                self.role_sent = true;
                vec![self.chunk(json!({"role": "assistant", "content": ""}), None)]
            }
            "contentBlockStart" => {
                let index = data["contentBlockIndex"].as_u64().unwrap_or(0);
                match data["start"].get("toolUse") {
                    Some(tool_use) => {
                        let tool_index = self.next_tool_index;
                        self.next_tool_index += 1;
                        self.saw_tools = true;
                        self.tool_indices.insert(index, tool_index);
                        vec![self.chunk(
                            json!({"tool_calls": [{
                                "index": tool_index,
                                "id": tool_use["toolUseId"],
                                "type": "function",
                                "function": {"name": tool_use["name"], "arguments": ""},
                            }]}),
                            None,
                        )]
                    }
                    None => Vec::new(),
                }
            }
            "contentBlockDelta" => {
                let index = data["contentBlockIndex"].as_u64().unwrap_or(0);
                let delta = &data["delta"];
                if let Some(text) = delta["text"].as_str() {
                    return vec![self.chunk(json!({"content": text}), None)];
                }
                if let Some(fragment) = delta["toolUse"]["input"].as_str()
                    && let Some(&tool_index) = self.tool_indices.get(&index)
                {
                    return vec![self.chunk(
                        json!({"tool_calls": [{"index": tool_index,
                               "function": {"arguments": fragment}}]}),
                        None,
                    )];
                }
                Vec::new()
            }
            "messageStop" => {
                self.finish = Some(match data["stopReason"].as_str() {
                    Some("tool_use") => "tool_calls",
                    Some("max_tokens") => "length",
                    _ if self.saw_tools => "tool_calls",
                    _ => "stop",
                });
                Vec::new() // usage arrives in the metadata frame
            }
            "metadata" => {
                let usage = &data["usage"];
                let mut chunk = self.chunk(json!({}), self.finish.or(Some("stop")));
                chunk["usage"] = json!({
                    "prompt_tokens": usage["inputTokens"],
                    "completion_tokens": usage["outputTokens"],
                    "total_tokens": usage["totalTokens"],
                });
                vec![chunk]
            }
            "exception" => {
                tracing_unavailable_note();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id": "chatcmpl-bedrock",
            "object": "chat.completion.chunk",
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    }
}

/// Providers crate has no tracing dependency; exceptions surface through
/// the stream ending early, which the server logs.
fn tracing_unavailable_note() {}
