"""Anthropic SDK scenario suite against caret-router's /anthropic dialect.

Covers same-dialect passthrough (anthropic model), cross-dialect routing
(OpenAI model behind the Anthropic SDK — the coding-agent shape), tools,
and streaming event fidelity as the official SDK sees it.
"""

import sys

import anthropic
from anthropic import Anthropic

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18091"
client = Anthropic(base_url=f"{BASE}/anthropic", api_key="unused")

passed, failures = [], 0


def run(name, fn):
    global failures
    try:
        fn()
        passed.append(name)
        print(f"  ok  {name}")
    except Exception as e:  # noqa: BLE001
        failures += 1
        print(f"FAIL {name}: {type(e).__name__}: {e}")


def s_sync_passthrough():
    r = client.messages.create(
        model="anthropic/claude-x", max_tokens=100,
        messages=[{"role": "user", "content": "hi"}],
    )
    assert r.role == "assistant"
    assert r.content[0].text == "mock response"
    assert r.stop_reason == "end_turn"
    assert r.usage.input_tokens == 11


def s_sync_cross_dialect():
    # The coding-agent shape: Anthropic SDK, OpenAI model behind it.
    r = client.messages.create(
        model="openai/gpt-4o", max_tokens=100,
        messages=[{"role": "user", "content": "hi"}],
    )
    assert r.content[0].text == "mock response"
    assert r.usage.input_tokens == 7  # openai mock's usage, translated


def s_streaming_passthrough():
    text = ""
    with client.messages.stream(
        model="anthropic/claude-x", max_tokens=100,
        messages=[{"role": "user", "content": "hi"}],
    ) as stream:
        for chunk in stream.text_stream:
            text += chunk
        final = stream.get_final_message()
    assert text == "mock stream"
    assert final.stop_reason == "end_turn"


def s_streaming_cross_dialect_tools():
    # OpenAI upstream streams split tool args; the SDK must reassemble a
    # complete tool_use block from our translated event sequence.
    with client.messages.stream(
        model="openai/gpt-4o", max_tokens=100,
        messages=[{"role": "user", "content": "weather"}],
        tools=[{"name": "get_weather",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}],
    ) as stream:
        final = stream.get_final_message()
    tool_uses = [b for b in final.content if b.type == "tool_use"]
    assert len(tool_uses) == 1
    assert tool_uses[0].name == "get_weather"
    assert tool_uses[0].input == {"city": "Paris"}
    assert final.stop_reason == "tool_use"


def s_multi_turn_tools():
    r = client.messages.create(
        model="openai/gpt-4o", max_tokens=100,
        messages=[
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather",
                 "input": {"city": "Paris"}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "21C"}]},
        ],
    )
    assert r.content[0].text == "mock response"


def s_unknown_model():
    try:
        client.messages.create(model="never-heard", max_tokens=10,
                               messages=[{"role": "user", "content": "x"}])
        raise AssertionError("expected NotFoundError")
    except anthropic.NotFoundError:
        pass


def s_rate_limit():
    no_retry = client.with_options(max_retries=0)
    try:
        no_retry.messages.create(model="anthropic/err-429", max_tokens=10,
                                 messages=[{"role": "user", "content": "x"}])
        raise AssertionError("expected RateLimitError")
    except anthropic.RateLimitError:
        pass


if __name__ == "__main__":
    for name, fn in [
        ("sync passthrough", s_sync_passthrough),
        ("sync cross-dialect (openai model)", s_sync_cross_dialect),
        ("streaming passthrough", s_streaming_passthrough),
        ("streaming cross-dialect tool use", s_streaming_cross_dialect_tools),
        ("multi-turn tool results", s_multi_turn_tools),
        ("unknown model -> NotFoundError", s_unknown_model),
        ("429 -> RateLimitError", s_rate_limit),
    ]:
        run(name, fn)
    print(f"\n{len(passed)} passed, {failures} failed")
    sys.exit(1 if failures else 0)
