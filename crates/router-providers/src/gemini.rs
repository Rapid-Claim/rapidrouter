//! Google Gemini dialect: internal (OpenAI-shaped) requests and
//! responses translated to and from `generateContent` /
//! `streamGenerateContent`, in both directions.

use router_core::chat::{ChatRequest, Content, ContentPart, Message, ToolCall};
use router_core::{ErrorClass, GatewayError};
use serde_json::{Map, Value, json};

// ---------------------------------------------------------------------------
// internal -> Gemini request
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct BuiltRequest {
    pub body: Value,
    pub dropped_params: Vec<String>,
}

pub fn build_request(req: &ChatRequest) -> Result<BuiltRequest, GatewayError> {
    if req.logprobs == Some(true) {
        return Err(GatewayError::new(
            ErrorClass::InvalidRequest,
            "`logprobs` is not supported for Gemini models",
        )
        .with_param("logprobs"));
    }

    let mut dropped = Vec::new();
    let mut body = Map::new();

    let system: Vec<String> = req
        .messages
        .iter()
        .filter(|m| m.role == "system" || m.role == "developer")
        .filter_map(|m| m.content.as_ref().map(Content::as_text))
        .collect();
    if !system.is_empty() {
        body.insert(
            "systemInstruction".into(),
            json!({"parts": [{"text": system.join("\n\n")}]}),
        );
    }

    body.insert(
        "contents".into(),
        Value::Array(translate_messages(&req.messages)?),
    );

    let mut generation = Map::new();
    if let Some(t) = req.temperature {
        generation.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.top_p {
        generation.insert("topP".into(), json!(p));
    }
    if let Some(m) = req.effective_max_tokens() {
        generation.insert("maxOutputTokens".into(), json!(m));
    }
    if let Some(stop) = &req.stop {
        let sequences = match stop {
            Value::String(s) => json!([s]),
            other => other.clone(),
        };
        generation.insert("stopSequences".into(), sequences);
    }
    if let Some(n) = req.n {
        generation.insert("candidateCount".into(), json!(n));
    }
    if let Some(format) = &req.response_format {
        match format["type"].as_str() {
            Some("json_schema") => {
                generation.insert("responseMimeType".into(), json!("application/json"));
                let mut schema = format["json_schema"]["schema"].clone();
                strip_unsupported_schema_fields(&mut schema);
                generation.insert("responseSchema".into(), schema);
            }
            Some("json_object") => {
                generation.insert("responseMimeType".into(), json!("application/json"));
            }
            _ => {}
        }
    }
    if !generation.is_empty() {
        body.insert("generationConfig".into(), Value::Object(generation));
    }

    if let Some(tools) = &req.tools {
        let declarations: Vec<Value> = tools
            .iter()
            .map(|t| {
                let mut parameters = t
                    .function
                    .parameters
                    .clone()
                    .unwrap_or(json!({"type": "object"}));
                strip_unsupported_schema_fields(&mut parameters);
                json!({
                    "name": t.function.name,
                    "description": t.function.description,
                    "parameters": parameters,
                })
            })
            .collect();
        body.insert(
            "tools".into(),
            json!([{"functionDeclarations": declarations}]),
        );
    }

    if let Some(choice) = &req.tool_choice {
        let mode = match choice {
            Value::String(s) => match s.as_str() {
                "none" => json!({"mode": "NONE"}),
                "required" => json!({"mode": "ANY"}),
                _ => json!({"mode": "AUTO"}),
            },
            Value::Object(o) => json!({
                "mode": "ANY",
                "allowedFunctionNames": [o["function"]["name"]],
            }),
            _ => json!({"mode": "AUTO"}),
        };
        body.insert("toolConfig".into(), json!({"functionCallingConfig": mode}));
    }

    for (param, present) in [
        ("presence_penalty", req.presence_penalty.is_some()),
        ("frequency_penalty", req.frequency_penalty.is_some()),
        ("seed", req.seed.is_some()),
        ("stream_options", req.stream_options.is_some()),
        ("parallel_tool_calls", req.parallel_tool_calls.is_some()),
    ] {
        if present {
            dropped.push(param.to_owned());
        }
    }
    for key in req.extra.keys() {
        dropped.push(key.clone());
    }

    Ok(BuiltRequest {
        body: Value::Object(body),
        dropped_params: dropped,
    })
}

/// Gemini function declarations reject some JSON-Schema keywords.
fn strip_unsupported_schema_fields(schema: &mut Value) {
    match schema {
        Value::Object(o) => {
            o.remove("additionalProperties");
            o.remove("$schema");
            for value in o.values_mut() {
                strip_unsupported_schema_fields(value);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_unsupported_schema_fields(item);
            }
        }
        _ => {}
    }
}

/// Gemini has no tool-call ids: requests carry calls/responses by
/// function *name*. Ids are synthesized on the way out and looked up by
/// name on the way in.
fn translate_messages(messages: &[Message]) -> Result<Vec<Value>, GatewayError> {
    // tool_call_id -> function name, learned from assistant turns.
    let mut call_names: std::collections::BTreeMap<&str, &str> = Default::default();
    for m in messages {
        for call in m.tool_calls.iter().flatten() {
            call_names.insert(&call.id, &call.function.name);
        }
    }

    let mut out: Vec<Value> = Vec::new();
    let mut push = |role: &str, parts: Vec<Value>| {
        if parts.is_empty() {
            return;
        }
        if let Some(last) = out.last_mut()
            && last["role"] == role
        {
            last["parts"]
                .as_array_mut()
                .expect("built as array")
                .extend(parts);
        } else {
            out.push(json!({"role": role, "parts": parts}));
        }
    };

    for message in messages {
        match message.role.as_str() {
            "system" | "developer" => {}
            "user" => push("user", content_parts(message)?),
            "assistant" => {
                let mut parts = content_parts(message)?;
                for call in message.tool_calls.iter().flatten() {
                    parts.push(function_call_part(call)?);
                }
                push("model", parts);
            }
            "tool" => {
                let id = message.tool_call_id.as_deref().unwrap_or_default();
                let name = call_names.get(id).copied().unwrap_or(id);
                let text = message
                    .content
                    .as_ref()
                    .map(Content::as_text)
                    .unwrap_or_default();
                let response: Value = serde_json::from_str(&text)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({"result": text}));
                push(
                    "user",
                    vec![json!({"functionResponse": {"name": name, "response": response}})],
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
    Ok(out)
}

fn content_parts(message: &Message) -> Result<Vec<Value>, GatewayError> {
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
        Content::Parts(parts) => parts.iter().map(part_to_gemini).collect(),
    }
}

fn part_to_gemini(part: &ContentPart) -> Result<Value, GatewayError> {
    match part {
        ContentPart::Text { text } => Ok(json!({"text": text})),
        ContentPart::ImageUrl { image_url } => {
            if let Some(rest) = image_url.url.strip_prefix("data:") {
                let (mime, data) = rest.split_once(";base64,").ok_or_else(|| {
                    GatewayError::new(
                        ErrorClass::InvalidRequest,
                        "image data URI must be base64-encoded",
                    )
                    .with_param("messages")
                })?;
                Ok(json!({"inlineData": {"mimeType": mime, "data": data}}))
            } else {
                Ok(json!({"fileData": {"fileUri": image_url.url}}))
            }
        }
        ContentPart::InputAudio { input_audio } => {
            let data = input_audio["data"].as_str().unwrap_or_default();
            let format = input_audio["format"].as_str().unwrap_or("wav");
            Ok(json!({"inlineData": {"mimeType": format!("audio/{format}"), "data": data}}))
        }
        ContentPart::File { file } => {
            let data = file["file_data"].as_str().unwrap_or_default();
            let data = data
                .strip_prefix("data:application/pdf;base64,")
                .unwrap_or(data);
            Ok(json!({"inlineData": {"mimeType": "application/pdf", "data": data}}))
        }
    }
}

fn function_call_part(call: &ToolCall) -> Result<Value, GatewayError> {
    let args: Value = if call.function.arguments.trim().is_empty() {
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
    Ok(json!({"functionCall": {"name": call.function.name, "args": args}}))
}

// ---------------------------------------------------------------------------
// Gemini response -> internal (OpenAI shape)
// ---------------------------------------------------------------------------

pub fn response_to_openai(body: &Value, model: &str) -> Value {
    let candidate = &body["candidates"][0];
    let (message, has_tools) = candidate_to_message(candidate);

    let finish_reason = match candidate["finishReason"].as_str() {
        _ if has_tools => "tool_calls",
        Some("MAX_TOKENS") => "length",
        Some("SAFETY") | Some("PROHIBITED_CONTENT") => "content_filter",
        _ => "stop",
    };

    let usage = &body["usageMetadata"];
    json!({
        "id": format!("chatcmpl-{}", body["responseId"].as_str().unwrap_or("gemini")),
        "object": "chat.completion",
        "model": body["modelVersion"].as_str().unwrap_or(model),
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": {
            "prompt_tokens": usage["promptTokenCount"].as_u64().unwrap_or(0),
            "completion_tokens": usage["candidatesTokenCount"].as_u64().unwrap_or(0),
            "total_tokens": usage["totalTokenCount"].as_u64().unwrap_or(0),
        },
    })
}

fn candidate_to_message(candidate: &Value) -> (Value, bool) {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for (i, part) in candidate["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        if let Some(t) = part["text"].as_str() {
            text.push_str(t);
        }
        if let Some(call) = part.get("functionCall") {
            tool_calls.push(json!({
                "id": format!("call_{i}"),
                "type": "function",
                "function": {
                    "name": call["name"],
                    "arguments": call["args"].to_string(),
                },
            }));
        }
    }
    let has_tools = !tool_calls.is_empty();
    let mut message = json!({
        "role": "assistant",
        "content": if text.is_empty() && has_tools { Value::Null } else { json!(text) },
    });
    if has_tools {
        message["tool_calls"] = json!(tool_calls);
    }
    (message, has_tools)
}

// ---------------------------------------------------------------------------
// Gemini stream -> internal (OpenAI chunks)
// ---------------------------------------------------------------------------

/// Gemini streams whole `GenerateContentResponse` objects; each becomes
/// one OpenAI chunk. Function calls arrive complete, so their arguments
/// emit as a single delta. The final chunk carries finish + usage.
#[derive(Default)]
pub struct StreamToOpenAi {
    role_sent: bool,
    tool_count: u64,
    saw_tools: bool,
    model: String,
}

impl StreamToOpenAi {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.to_owned(),
            ..Default::default()
        }
    }

    pub fn on_chunk(&mut self, data: &Value) -> Vec<Value> {
        let candidate = &data["candidates"][0];
        let mut chunks = Vec::new();

        if !self.role_sent {
            self.role_sent = true;
            chunks.push(self.chunk(data, json!({"role": "assistant", "content": ""}), None));
        }

        for part in candidate["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(text) = part["text"].as_str()
                && !text.is_empty()
            {
                chunks.push(self.chunk(data, json!({"content": text}), None));
            }
            if let Some(call) = part.get("functionCall") {
                let index = self.tool_count;
                self.tool_count += 1;
                self.saw_tools = true;
                chunks.push(self.chunk(
                    data,
                    json!({"tool_calls": [{
                        "index": index,
                        "id": format!("call_{index}"),
                        "type": "function",
                        "function": {"name": call["name"], "arguments": call["args"].to_string()},
                    }]}),
                    None,
                ));
            }
        }

        if let Some(reason) = candidate["finishReason"].as_str() {
            let finish = match reason {
                _ if self.saw_tools => "tool_calls",
                "MAX_TOKENS" => "length",
                "SAFETY" | "PROHIBITED_CONTENT" => "content_filter",
                _ => "stop",
            };
            let usage = &data["usageMetadata"];
            let mut final_chunk = self.chunk(data, json!({}), Some(finish));
            final_chunk["usage"] = json!({
                "prompt_tokens": usage["promptTokenCount"].as_u64().unwrap_or(0),
                "completion_tokens": usage["candidatesTokenCount"].as_u64().unwrap_or(0),
                "total_tokens": usage["totalTokenCount"].as_u64().unwrap_or(0),
            });
            chunks.push(final_chunk);
        }
        chunks
    }

    fn chunk(&self, data: &Value, delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id": format!("chatcmpl-{}", data["responseId"].as_str().unwrap_or("gemini")),
            "object": "chat.completion.chunk",
            "model": data["modelVersion"].as_str().unwrap_or(&self.model),
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    }
}

// ---------------------------------------------------------------------------
// Inbound: Gemini request -> internal
// ---------------------------------------------------------------------------

pub fn request_to_internal(
    body: &Value,
    model: &str,
    stream: bool,
) -> Result<ChatRequest, GatewayError> {
    let mut messages: Vec<Message> = Vec::new();

    if let Some(system) = body
        .get("systemInstruction")
        .or_else(|| body.get("system_instruction"))
    {
        let text: String = system["parts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
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

    let mut synth_call_index = 0u64;
    // function name -> most recent synthesized call id.
    let mut last_call_id: std::collections::BTreeMap<String, String> = Default::default();

    for content in body["contents"].as_array().into_iter().flatten() {
        let role = match content["role"].as_str() {
            Some("model") => "assistant",
            _ => "user",
        };
        let mut parts: Vec<ContentPart> = Vec::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        for part in content["parts"].as_array().into_iter().flatten() {
            if let Some(text) = part["text"].as_str() {
                parts.push(ContentPart::Text {
                    text: text.to_owned(),
                });
            } else if let Some(inline) = part.get("inlineData").or_else(|| part.get("inline_data"))
            {
                let url = format!(
                    "data:{};base64,{}",
                    inline["mimeType"]
                        .as_str()
                        .or(inline["mime_type"].as_str())
                        .unwrap_or("image/png"),
                    inline["data"].as_str().unwrap_or_default()
                );
                parts.push(ContentPart::ImageUrl {
                    image_url: router_core::chat::ImageUrl { url, detail: None },
                });
            } else if let Some(call) = part
                .get("functionCall")
                .or_else(|| part.get("function_call"))
            {
                let name = call["name"].as_str().unwrap_or_default().to_owned();
                let id = format!("call_{synth_call_index}");
                synth_call_index += 1;
                last_call_id.insert(name.clone(), id.clone());
                tool_calls.push(ToolCall {
                    id,
                    call_type: "function".into(),
                    function: router_core::chat::FunctionCall {
                        name,
                        arguments: call["args"].to_string(),
                    },
                });
            } else if let Some(resp) = part
                .get("functionResponse")
                .or_else(|| part.get("function_response"))
            {
                let name = resp["name"].as_str().unwrap_or_default();
                messages.push(Message {
                    role: "tool".into(),
                    content: Some(Content::Text(resp["response"].to_string())),
                    tool_calls: None,
                    tool_call_id: Some(
                        last_call_id
                            .get(name)
                            .cloned()
                            .unwrap_or_else(|| name.to_owned()),
                    ),
                    name: Some(name.to_owned()),
                });
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

    let tools = body["tools"][0]["functionDeclarations"]
        .as_array()
        .or_else(|| body["tools"][0]["function_declarations"].as_array())
        .map(|decls| {
            decls
                .iter()
                .map(|d| router_core::chat::Tool {
                    tool_type: "function".into(),
                    function: router_core::chat::FunctionDef {
                        name: d["name"].as_str().unwrap_or_default().to_owned(),
                        description: d["description"].as_str().map(str::to_owned),
                        parameters: Some(d["parameters"].clone()),
                        strict: None,
                    },
                })
                .collect::<Vec<_>>()
        });

    let config = &body["generationConfig"];
    let tool_choice = body["toolConfig"]["functionCallingConfig"]["mode"]
        .as_str()
        .map(|mode| match mode {
            "NONE" => json!("none"),
            "ANY" => json!("required"),
            _ => json!("auto"),
        });

    Ok(ChatRequest {
        model: model.to_owned(),
        messages,
        tools,
        tool_choice,
        parallel_tool_calls: None,
        temperature: config["temperature"].as_f64(),
        top_p: config["topP"].as_f64(),
        max_tokens: config["maxOutputTokens"].as_u64(),
        max_completion_tokens: None,
        stop: config.get("stopSequences").cloned(),
        n: config["candidateCount"].as_u64().map(|n| n as u32),
        stream: Some(stream),
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

// ---------------------------------------------------------------------------
// Inbound: internal (OpenAI shape) response -> Gemini
// ---------------------------------------------------------------------------

pub fn openai_response_to_gemini(body: &Value) -> Value {
    let message = &body["choices"][0]["message"];
    let mut parts = Vec::new();
    if let Some(text) = message["content"].as_str()
        && !text.is_empty()
    {
        parts.push(json!({"text": text}));
    }
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        let args: Value = call["function"]["arguments"]
            .as_str()
            .and_then(|a| serde_json::from_str(a).ok())
            .unwrap_or(json!({}));
        parts.push(json!({"functionCall": {"name": call["function"]["name"], "args": args}}));
    }

    let finish = match body["choices"][0]["finish_reason"].as_str() {
        Some("length") => "MAX_TOKENS",
        Some("content_filter") => "SAFETY",
        _ => "STOP",
    };

    json!({
        "candidates": [{
            "content": {"role": "model", "parts": parts},
            "finishReason": finish,
            "index": 0,
        }],
        "usageMetadata": {
            "promptTokenCount": body["usage"]["prompt_tokens"],
            "candidatesTokenCount": body["usage"]["completion_tokens"],
            "totalTokenCount": body["usage"]["total_tokens"],
        },
        "modelVersion": body["model"],
    })
}

/// Inbound streaming: OpenAI chunks -> Gemini stream chunks.
///
/// OpenAI splits a tool call across chunks (name first, argument
/// fragments after); Gemini clients expect whole `functionCall` parts.
/// Calls buffer per tool index and emit the moment their accumulated
/// arguments parse as complete JSON.
#[derive(Default)]
pub struct OpenAiToGeminiStream {
    /// tool index -> (name, accumulated argument fragments).
    pending: std::collections::BTreeMap<u64, (String, String)>,
    emitted: std::collections::BTreeSet<u64>,
}

impl OpenAiToGeminiStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_chunk(&mut self, chunk: &Value) -> Option<Value> {
        let choice = &chunk["choices"][0];
        let delta = &choice["delta"];
        let mut parts = Vec::new();

        if let Some(text) = delta["content"].as_str()
            && !text.is_empty()
        {
            parts.push(json!({"text": text}));
        }

        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"].as_u64().unwrap_or(0);
            let entry = self.pending.entry(index).or_default();
            if let Some(name) = call["function"]["name"].as_str() {
                entry.0 = name.to_owned();
            }
            if let Some(fragment) = call["function"]["arguments"].as_str() {
                entry.1.push_str(fragment);
            }
        }
        parts.extend(self.flush_complete_calls());

        let finish = choice["finish_reason"].as_str().map(|reason| match reason {
            "length" => "MAX_TOKENS",
            "content_filter" => "SAFETY",
            _ => "STOP",
        });

        if parts.is_empty() && finish.is_none() {
            return None;
        }
        let mut out = json!({
            "candidates": [{
                "content": {"role": "model", "parts": parts},
                "index": 0,
            }],
            "modelVersion": chunk["model"],
        });
        if let Some(finish) = finish {
            out["candidates"][0]["finishReason"] = json!(finish);
            if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
                out["usageMetadata"] = json!({
                    "promptTokenCount": usage["prompt_tokens"],
                    "candidatesTokenCount": usage["completion_tokens"],
                    "totalTokenCount": usage["total_tokens"],
                });
            }
        }
        Some(out)
    }

    /// Emit every buffered call whose arguments now form complete JSON.
    fn flush_complete_calls(&mut self) -> Vec<Value> {
        let mut parts = Vec::new();
        for (&index, (name, args)) in &self.pending {
            if self.emitted.contains(&index) || name.is_empty() {
                continue;
            }
            let parsed: Option<Value> = if args.trim().is_empty() {
                None
            } else {
                serde_json::from_str(args).ok()
            };
            if let Some(parsed) = parsed {
                parts.push(json!({"functionCall": {"name": name, "args": parsed}}));
                self.emitted.insert(index);
            }
        }
        parts
    }
}
