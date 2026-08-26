//! The Responses API dialect: translation between Responses-shaped
//! requests/responses and the internal (chat-shaped) model, plus the
//! streaming event formatter.
//!
//! OpenAI-kind targets relay the Responses surface untouched; this module
//! exists for everything else — the *stateless* core (input items,
//! instructions, tools, streaming) mapped through the internal model.
//! Statefulness (`store`, `previous_response_id`) requires the provider
//! to hold state and is rejected for translated targets at the endpoint.

use router_core::chat::{
    ChatRequest, Content, ContentPart, FunctionCall, FunctionDef, ImageUrl, Message, Tool, ToolCall,
};
use router_core::{ErrorClass, GatewayError};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Responses request -> internal
// ---------------------------------------------------------------------------

pub struct ParsedRequest {
    pub internal: ChatRequest,
    pub dropped_params: Vec<String>,
}

pub fn request_to_internal(body: &Value) -> Result<ParsedRequest, GatewayError> {
    let model = body["model"]
        .as_str()
        .ok_or_else(|| {
            GatewayError::new(ErrorClass::InvalidRequest, "`model` is required").with_param("model")
        })?
        .to_owned();

    let mut dropped = Vec::new();
    let mut messages: Vec<Message> = Vec::new();

    if let Some(instructions) = body["instructions"].as_str()
        && !instructions.is_empty()
    {
        messages.push(text_message("system", instructions.to_owned()));
    }

    match &body["input"] {
        Value::String(text) => messages.push(text_message("user", text.clone())),
        Value::Array(items) => {
            for item in items {
                translate_input_item(item, &mut messages, &mut dropped)?;
            }
        }
        Value::Null => {
            return Err(
                GatewayError::new(ErrorClass::InvalidRequest, "`input` is required")
                    .with_param("input"),
            );
        }
        _ => {
            return Err(GatewayError::new(
                ErrorClass::InvalidRequest,
                "`input` must be a string or an array of items",
            )
            .with_param("input"));
        }
    }

    // Chat-style providers support functions but not Responses namespaces.
    // The default namespace is only a wrapper, so its function members remain
    // callable. Named namespaces need a namespace-bearing response item to
    // route calls back correctly and are omitted on this translated surface.
    let tools = match body["tools"].as_array() {
        Some(tools) if !tools.is_empty() => {
            let mut translated = Vec::new();
            for tool in tools {
                match tool["type"].as_str() {
                    Some("function") | None => translated.push(responses_function(tool)),
                    Some("namespace") => {
                        let namespace = tool["name"].as_str().unwrap_or_default();
                        if namespace == "functions" {
                            for member in tool["tools"].as_array().into_iter().flatten() {
                                match member["type"].as_str() {
                                    Some("function") | None => {
                                        translated.push(responses_function(member));
                                    }
                                    Some(member_type) => dropped
                                        .push(format!("tools.namespace.{namespace}.{member_type}")),
                                }
                            }
                        } else {
                            dropped.push(format!("tools.namespace.{namespace}"));
                        }
                    }
                    Some("web_search") => dropped.push("tools.web_search".to_owned()),
                    Some(builtin) => {
                        return Err(GatewayError::new(
                            ErrorClass::InvalidRequest,
                            format!(
                                "built-in tool `{builtin}` is only available on providers that \
                                 relay the Responses API natively"
                            ),
                        )
                        .with_param("tools"));
                    }
                }
            }
            (!translated.is_empty()).then_some(translated)
        }
        _ => None,
    };

    let tool_choice = body
        .get("tool_choice")
        .filter(|v| !v.is_null())
        .map(|choice| {
            match choice {
                // Named form is flat in Responses, nested in chat.
                Value::Object(o) if o.get("type").map(|t| t == "function").unwrap_or(false) => {
                    json!({"type": "function", "function": {"name": o["name"]}})
                }
                other => other.clone(),
            }
        });

    let response_format = match body["text"]["format"]["type"].as_str() {
        Some("json_schema") => Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": body["text"]["format"]["name"],
                "schema": body["text"]["format"]["schema"],
                "strict": body["text"]["format"]["strict"],
            },
        })),
        Some("json_object") => Some(json!({"type": "json_object"})),
        _ => None,
    };

    let mut extra = std::collections::BTreeMap::new();
    match body.get("reasoning") {
        Some(Value::Object(reasoning)) => {
            for (name, value) in reasoning {
                if value.is_null() {
                    continue;
                }
                if name == "effort" {
                    if let Some(effort) = value.as_str() {
                        extra.insert("reasoning_effort".to_owned(), json!(effort));
                    } else {
                        dropped.push("reasoning.effort".to_owned());
                    }
                } else {
                    dropped.push(format!("reasoning.{name}"));
                }
            }
        }
        Some(value) if !value.is_null() => dropped.push("reasoning".to_owned()),
        _ => {}
    }

    // `metadata` is absent from this list on purpose: the gateway reads
    // it as attribution, so it is consumed rather than dropped.
    for param in ["include", "truncation", "service_tier"] {
        if body.get(param).is_some_and(|v| !v.is_null()) {
            dropped.push(param.to_owned());
        }
    }

    let internal = ChatRequest {
        model,
        messages,
        tools,
        tool_choice,
        parallel_tool_calls: body["parallel_tool_calls"].as_bool(),
        temperature: body["temperature"].as_f64(),
        top_p: body["top_p"].as_f64(),
        max_tokens: body["max_output_tokens"].as_u64(),
        max_completion_tokens: None,
        stop: None,
        n: None,
        stream: body["stream"].as_bool(),
        stream_options: None,
        response_format,
        logprobs: None,
        presence_penalty: None,
        frequency_penalty: None,
        seed: None,
        user: body["user"].as_str().map(str::to_owned),
        extra,
    };
    Ok(ParsedRequest {
        internal,
        dropped_params: dropped,
    })
}

fn responses_function(tool: &Value) -> Tool {
    Tool {
        tool_type: "function".into(),
        function: FunctionDef {
            name: tool["name"].as_str().unwrap_or_default().to_owned(),
            description: tool["description"].as_str().map(str::to_owned),
            parameters: tool
                .get("parameters")
                .cloned()
                .filter(|parameters| !parameters.is_null()),
            strict: tool["strict"].as_bool(),
        },
    }
}

fn translate_input_item(
    item: &Value,
    messages: &mut Vec<Message>,
    dropped: &mut Vec<String>,
) -> Result<(), GatewayError> {
    let item_type = item["type"].as_str().unwrap_or("message");
    match item_type {
        "message" => {
            let role = item["role"].as_str().unwrap_or("user").to_owned();
            match &item["content"] {
                Value::String(text) => messages.push(text_message(&role, text.clone())),
                Value::Array(parts) => {
                    let parts = parts
                        .iter()
                        .filter_map(translate_content_part)
                        .collect::<Vec<_>>();
                    messages.push(Message {
                        role,
                        content: Some(Content::Parts(parts)),
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    });
                }
                _ => {}
            }
        }
        "function_call" => messages.push(Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: item["call_id"].as_str().unwrap_or_default().to_owned(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: item["name"].as_str().unwrap_or_default().to_owned(),
                    arguments: item["arguments"].as_str().unwrap_or("{}").to_owned(),
                },
            }]),
            tool_call_id: None,
            name: None,
        }),
        "function_call_output" => messages.push(Message {
            role: "tool".into(),
            content: Some(Content::Text(
                item["output"].as_str().unwrap_or_default().to_owned(),
            )),
            tool_calls: None,
            tool_call_id: Some(item["call_id"].as_str().unwrap_or_default().to_owned()),
            name: None,
        }),
        "reasoning" => dropped.push("input.reasoning".to_owned()),
        other => {
            return Err(GatewayError::new(
                ErrorClass::InvalidRequest,
                format!(
                    "input item type `{other}` is only available on providers that relay \
                     the Responses API natively"
                ),
            )
            .with_param("input"));
        }
    }
    Ok(())
}

fn translate_content_part(part: &Value) -> Option<ContentPart> {
    match part["type"].as_str() {
        Some("input_text") | Some("output_text") | Some("text") => Some(ContentPart::Text {
            text: part["text"].as_str().unwrap_or_default().to_owned(),
        }),
        Some("input_image") => {
            let url = part["image_url"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| part["image_url"]["url"].as_str().map(str::to_owned))?;
            Some(ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url,
                    detail: part["detail"].as_str().map(str::to_owned),
                },
            })
        }
        Some("input_file") => Some(ContentPart::File {
            file: json!({
                "file_data": part["file_data"],
                "filename": part["filename"],
            }),
        }),
        _ => None,
    }
}

fn text_message(role: &str, text: String) -> Message {
    Message {
        role: role.into(),
        content: Some(Content::Text(text)),
        tool_calls: None,
        tool_call_id: None,
        name: None,
    }
}

// ---------------------------------------------------------------------------
// internal (OpenAI chat shape) response -> Responses response
// ---------------------------------------------------------------------------

pub fn openai_response_to_responses(body: &Value) -> Value {
    let choice = &body["choices"][0];
    let message = &choice["message"];
    let mut output = Vec::new();

    for (i, call) in message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        output.push(json!({
            "type": "function_call",
            "id": format!("fc_{i}"),
            "call_id": call["id"],
            "name": call["function"]["name"],
            "arguments": call["function"]["arguments"],
            "status": "completed",
        }));
    }
    if let Some(text) = message["content"].as_str()
        && !text.is_empty()
    {
        output.push(json!({
            "type": "message",
            "id": "msg_0",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }));
    }

    let (status, incomplete) = match choice["finish_reason"].as_str() {
        Some("length") => ("incomplete", Some(json!({"reason": "max_output_tokens"}))),
        _ => ("completed", None),
    };

    let mut response = json!({
        "id": format!("resp_{}", body["id"].as_str().unwrap_or("x")),
        "object": "response",
        "status": status,
        "model": body["model"],
        "output": output,
        "usage": {
            "input_tokens": body["usage"]["prompt_tokens"],
            "output_tokens": body["usage"]["completion_tokens"],
            "total_tokens": body["usage"]["total_tokens"],
        },
        "error": null,
        "incomplete_details": incomplete,
    });
    if status == "incomplete" {
        response["incomplete_details"] = incomplete.unwrap_or(Value::Null);
    }
    response
}

// ---------------------------------------------------------------------------
// internal chunks -> Responses streaming events
// ---------------------------------------------------------------------------

/// Translates internal (chat-chunk) deltas into the Responses event
/// sequence: `response.created`, `response.output_item.added`,
/// `response.output_text.delta` / `response.function_call_arguments.delta`,
/// their `.done` counterparts, and `response.completed`.
#[derive(Default)]
pub struct ChunksToResponses {
    started: bool,
    response_id: String,
    model: String,
    /// Open text accumulation, if a message item is open.
    text: Option<String>,
    /// tool index -> (call_id, name, accumulated args).
    calls: std::collections::BTreeMap<u64, (String, String, String)>,
    open_item: Option<OpenItem>,
    output_index: u64,
    sequence: u64,
    finish: Option<String>,
    usage: Option<Value>,
    /// Completed output items, for the final `response.completed`.
    output: Vec<Value>,
}

#[derive(PartialEq)]
enum OpenItem {
    Message,
    FunctionCall(u64),
}

impl ChunksToResponses {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_chunk(&mut self, chunk: &Value) -> Vec<String> {
        let mut frames = Vec::new();
        let choice = &chunk["choices"][0];
        let delta = &choice["delta"];

        if !self.started {
            self.started = true;
            self.response_id = format!("resp_{}", chunk["id"].as_str().unwrap_or("stream"));
            self.model = chunk["model"].as_str().unwrap_or_default().to_owned();
            frames.push(self.event(
                "response.created",
                json!({"response": {
                    "id": self.response_id, "object": "response",
                    "status": "in_progress", "model": self.model, "output": [],
                }}),
            ));
        }

        if let Some(text) = delta["content"].as_str()
            && !text.is_empty()
        {
            if self.open_item != Some(OpenItem::Message) {
                self.close_open_item(&mut frames);
                self.open_item = Some(OpenItem::Message);
                self.text = Some(String::new());
                frames.push(self.event(
                    "response.output_item.added",
                    json!({"output_index": self.output_index, "item": {
                        "type": "message", "id": format!("msg_{}", self.output_index),
                        "role": "assistant", "status": "in_progress", "content": [],
                    }}),
                ));
                frames.push(self.event(
                    "response.content_part.added",
                    json!({"item_id": format!("msg_{}", self.output_index),
                           "output_index": self.output_index, "content_index": 0,
                           "part": {"type": "output_text", "text": "", "annotations": []}}),
                ));
            }
            self.text
                .as_mut()
                .expect("open message item")
                .push_str(text);
            frames.push(self.event(
                "response.output_text.delta",
                json!({"item_id": format!("msg_{}", self.output_index),
                       "output_index": self.output_index, "content_index": 0,
                       "delta": text}),
            ));
        }

        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"].as_u64().unwrap_or(0);
            if let Some(name) = call["function"]["name"].as_str() {
                self.close_open_item(&mut frames);
                let call_id = call["id"].as_str().unwrap_or("call_0").to_owned();
                self.calls
                    .insert(index, (call_id.clone(), name.to_owned(), String::new()));
                self.open_item = Some(OpenItem::FunctionCall(index));
                frames.push(self.event(
                    "response.output_item.added",
                    json!({"output_index": self.output_index, "item": {
                        "type": "function_call", "id": format!("fc_{index}"),
                        "call_id": call_id, "name": name, "arguments": "",
                        "status": "in_progress",
                    }}),
                ));
            }
            if let Some(fragment) = call["function"]["arguments"].as_str()
                && !fragment.is_empty()
                && let Some(entry) = self.calls.get_mut(&index)
            {
                entry.2.push_str(fragment);
                frames.push(self.event(
                    "response.function_call_arguments.delta",
                    json!({"item_id": format!("fc_{index}"),
                           "output_index": self.output_index, "delta": fragment}),
                ));
            }
        }

        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(json!({
                "input_tokens": usage["prompt_tokens"],
                "output_tokens": usage["completion_tokens"],
                "total_tokens": usage["total_tokens"],
            }));
        }
        if let Some(finish) = choice["finish_reason"].as_str() {
            self.finish = Some(finish.to_owned());
        }
        frames
    }

    pub fn finish(&mut self) -> Vec<String> {
        let mut frames = Vec::new();
        self.close_open_item(&mut frames);
        let status = match self.finish.as_deref() {
            Some("length") => "incomplete",
            _ => "completed",
        };
        let mut response = json!({
            "id": self.response_id, "object": "response", "status": status,
            "model": self.model, "output": self.output,
            "usage": self.usage.clone().unwrap_or(Value::Null),
        });
        if status == "incomplete" {
            response["incomplete_details"] = json!({"reason": "max_output_tokens"});
        }
        frames.push(self.event("response.completed", json!({"response": response})));
        frames
    }

    fn close_open_item(&mut self, frames: &mut Vec<String>) {
        match self.open_item.take() {
            Some(OpenItem::Message) => {
                let text = self.text.take().unwrap_or_default();
                frames.push(self.event(
                    "response.output_text.done",
                    json!({"item_id": format!("msg_{}", self.output_index),
                           "output_index": self.output_index, "content_index": 0,
                           "text": text}),
                ));
                let item = json!({
                    "type": "message", "id": format!("msg_{}", self.output_index),
                    "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": text, "annotations": []}],
                });
                frames.push(self.event(
                    "response.output_item.done",
                    json!({"output_index": self.output_index, "item": item}),
                ));
                self.output.push(item);
                self.output_index += 1;
            }
            Some(OpenItem::FunctionCall(index)) => {
                if let Some((call_id, name, args)) = self.calls.get(&index).cloned() {
                    frames.push(self.event(
                        "response.function_call_arguments.done",
                        json!({"item_id": format!("fc_{index}"),
                               "output_index": self.output_index, "arguments": args}),
                    ));
                    let item = json!({
                        "type": "function_call", "id": format!("fc_{index}"),
                        "call_id": call_id, "name": name, "arguments": args,
                        "status": "completed",
                    });
                    frames.push(self.event(
                        "response.output_item.done",
                        json!({"output_index": self.output_index, "item": item}),
                    ));
                    self.output.push(item);
                    self.output_index += 1;
                }
            }
            None => {}
        }
    }

    fn event(&mut self, name: &str, mut data: Value) -> String {
        data["type"] = json!(name);
        data["sequence_number"] = json!(self.sequence);
        self.sequence += 1;
        format!("event: {name}\ndata: {data}\n\n")
    }
}
