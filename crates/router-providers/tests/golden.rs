//! Golden-fixture tests: the fixtures are the adapter spec. A fixture
//! carries one conversation in the internal (OpenAI) shape plus its
//! expected translation into each foreign dialect.

use router_core::chat::ChatRequest;
use router_core::sse::SseParser;
use serde_json::Value;

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn internal(fx: &Value) -> ChatRequest {
    serde_json::from_value(fx["internal"].clone()).unwrap()
}

#[test]
fn tool_conversation_to_anthropic() {
    let fx = fixture("tool_conversation.json");
    let built = router_providers::anthropic::build_request(&internal(&fx), "claude-x").unwrap();
    assert_eq!(
        built.body, fx["anthropic"],
        "anthropic translation drifted from fixture"
    );
    assert!(built.dropped_params.is_empty());
}

#[test]
fn tool_conversation_to_gemini() {
    let fx = fixture("tool_conversation.json");
    let built = router_providers::gemini::build_request(&internal(&fx)).unwrap();
    assert_eq!(
        built.body, fx["gemini"],
        "gemini translation drifted from fixture"
    );
    assert!(built.dropped_params.is_empty());
}

/// Anthropic -> internal -> anthropic round trip preserves the
/// conversation structure (up to representation).
#[test]
fn anthropic_round_trip_stability() {
    let fx = fixture("tool_conversation.json");
    let req = router_providers::anthropic::request_to_internal(&fx["anthropic"]).unwrap();
    let rebuilt = router_providers::anthropic::build_request(&req, "claude-x").unwrap();
    assert_eq!(rebuilt.body["messages"], fx["anthropic"]["messages"]);
    assert_eq!(rebuilt.body["tools"], fx["anthropic"]["tools"]);
    assert_eq!(rebuilt.body["system"], fx["anthropic"]["system"]);
}

/// A recorded Anthropic tool stream must translate into OpenAI chunks
/// whose accumulated state matches the sync translation.
#[test]
fn anthropic_stream_transcript_translates() {
    let transcript = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-x\",\"usage\":{\"input_tokens\":11}}}\n\n\
event: ping\ndata: {\"type\":\"ping\"}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"checking\"}}\n\n\
event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\",\"input\":{}}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"ci\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"ty\\\":\\\"Paris\\\"}\"}}\n\n\
event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":9}}\n\n\
event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    let mut parser = SseParser::new();
    let mut translator = router_providers::anthropic::StreamToOpenAi::new(false);
    let mut content = String::new();
    let mut args = String::new();
    let mut tool_name = None;
    let mut finish = None;
    let mut usage = None;
    for event in parser.push(transcript.as_bytes()) {
        for chunk in translator.on_event(&event) {
            let delta = &chunk["choices"][0]["delta"];
            if let Some(t) = delta["content"].as_str() {
                content.push_str(t);
            }
            for call in delta["tool_calls"].as_array().into_iter().flatten() {
                if let Some(n) = call["function"]["name"].as_str() {
                    tool_name = Some(n.to_owned());
                    assert_eq!(call["id"], "toolu_1");
                    assert_eq!(call["index"], 0);
                }
                if let Some(a) = call["function"]["arguments"].as_str() {
                    args.push_str(a);
                }
            }
            if let Some(f) = chunk["choices"][0]["finish_reason"].as_str() {
                finish = Some(f.to_owned());
            }
            if !chunk["usage"].is_null() {
                usage = Some(chunk["usage"].clone());
            }
        }
    }
    assert_eq!(content, "checking");
    assert_eq!(tool_name.as_deref(), Some("get_weather"));
    assert_eq!(args, "{\"city\":\"Paris\"}");
    assert_eq!(finish.as_deref(), Some("tool_calls"));
    let usage = usage.unwrap();
    assert_eq!(usage["prompt_tokens"], 11);
    assert_eq!(usage["completion_tokens"], 9);
    assert_eq!(usage["total_tokens"], 20);
}

/// OpenAI chunks -> Anthropic events: the reverse stream produces a
/// spec-shaped event sequence.
#[test]
fn openai_chunks_to_anthropic_events() {
    use serde_json::json;
    let chunks = [
        json!({"id": "c1", "model": "gpt-4o", "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]}),
        json!({"id": "c1", "model": "gpt-4o", "choices": [{"index": 0, "delta": {"content": "hi "}, "finish_reason": null}]}),
        json!({"id": "c1", "model": "gpt-4o", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "f", "arguments": ""}}]}, "finish_reason": null}]}),
        json!({"id": "c1", "model": "gpt-4o", "choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"x\":1}"}}]}, "finish_reason": null}]}),
        json!({"id": "c1", "model": "gpt-4o", "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}], "usage": {"completion_tokens": 6}}),
    ];
    let mut state = router_providers::anthropic::OpenAiToAnthropicStream::new();
    let mut events: Vec<(String, Value)> = Vec::new();
    for chunk in &chunks {
        events.extend(state.on_chunk(chunk));
    }
    events.extend(state.finish());

    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "message_start",
            "content_block_start", // text block
            "content_block_delta",
            "content_block_stop",  // closed when the tool block opens
            "content_block_start", // tool block
            "content_block_delta", // arguments fragment
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );
    let (_, tool_start) = &events[4];
    assert_eq!(tool_start["content_block"]["type"], "tool_use");
    assert_eq!(tool_start["content_block"]["name"], "f");
    let (_, message_delta) = &events[7];
    assert_eq!(message_delta["delta"]["stop_reason"], "tool_use");
    assert_eq!(message_delta["usage"]["output_tokens"], 6);
}

/// Gemini function calls have no ids; synthesized ids must be stable and
/// name-mapping must survive the round trip.
#[test]
fn gemini_id_synthesis_round_trip() {
    let fx = fixture("tool_conversation.json");
    let req =
        router_providers::gemini::request_to_internal(&fx["gemini"], "gemini-pro", false).unwrap();
    // Assistant turn got synthesized ids; tool results reference them.
    let assistant = req.messages.iter().find(|m| m.role == "assistant").unwrap();
    let ids: Vec<&str> = assistant
        .tool_calls
        .as_ref()
        .unwrap()
        .iter()
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    let tool_messages: Vec<&router_core::chat::Message> =
        req.messages.iter().filter(|m| m.role == "tool").collect();
    // Same function name twice: both results map to the latest call id —
    // a documented Gemini limitation (calls are keyed by name, not id).
    assert!(tool_messages.iter().all(|m| {
        m.tool_call_id
            .as_deref()
            .is_some_and(|id| ids.contains(&id))
    }));

    // And back out to Gemini: functionResponse parts keyed by name again.
    let rebuilt = router_providers::gemini::build_request(&req).unwrap();
    assert_eq!(rebuilt.body["contents"], fx["gemini"]["contents"]);
}

#[test]
fn capability_violations_are_named() {
    let fx = fixture("tool_conversation.json");
    let mut req = internal(&fx);
    req.n = Some(3);
    let err = router_providers::anthropic::build_request(&req, "claude-x").unwrap_err();
    assert_eq!(err.param.as_deref(), Some("n"));

    let mut req = internal(&fx);
    req.logprobs = Some(true);
    let err = router_providers::gemini::build_request(&req).unwrap_err();
    assert_eq!(err.param.as_deref(), Some("logprobs"));
}

#[test]
fn dropped_params_are_reported_not_fatal() {
    let fx = fixture("tool_conversation.json");
    let mut req = internal(&fx);
    req.frequency_penalty = Some(0.5);
    req.seed = Some(42);
    let built = router_providers::anthropic::build_request(&req, "claude-x").unwrap();
    assert!(
        built
            .dropped_params
            .contains(&"frequency_penalty".to_owned())
    );
    assert!(built.dropped_params.contains(&"seed".to_owned()));
    assert!(built.body.get("frequency_penalty").is_none());
}
