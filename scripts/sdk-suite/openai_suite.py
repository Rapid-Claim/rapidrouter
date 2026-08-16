"""OpenAI SDK scenario suite, run against a rapid-router gateway.

Usage: openai_suite.py <gateway_base_url>
Exits non-zero on the first failed scenario. The gateway is expected to
front the mock provider (models: gpt-4o etc., err-429/err-500 stubs).
"""

import sys

import httpx
import openai
from openai import OpenAI

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18080"
client = OpenAI(base_url=f"{BASE}/v1", api_key="unused")

passed = []


def scenario(name):
    def wrap(fn):
        def run():
            fn()
            passed.append(name)
            print(f"  ok  {name}")

        return run

    return wrap


@scenario("sync completion")
def s_sync():
    r = client.chat.completions.create(
        model="openai/gpt-4o", messages=[{"role": "user", "content": "hi"}]
    )
    assert r.model == "gpt-4o"
    assert r.choices[0].message.content == "mock response"
    assert r.usage.total_tokens == 10


@scenario("streaming accumulates content and finishes")
def s_stream():
    stream = client.chat.completions.create(
        model="openai/gpt-4o", messages=[{"role": "user", "content": "hi"}], stream=True
    )
    content, finish = "", None
    for chunk in stream:
        delta = chunk.choices[0].delta
        if delta.content:
            content += delta.content
        if chunk.choices[0].finish_reason:
            finish = chunk.choices[0].finish_reason
    assert content == "mock stream", content
    assert finish == "stop"


@scenario("streamed tool call reassembles arguments")
def s_tools():
    stream = client.chat.completions.create(
        model="openai/gpt-4o",
        messages=[{"role": "user", "content": "weather?"}],
        tools=[{
            "type": "function",
            "function": {
                "name": "get_weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
            },
        }],
        stream=True,
    )
    name, args, finish = None, "", None
    for chunk in stream:
        choice = chunk.choices[0]
        for call in choice.delta.tool_calls or []:
            if call.function.name:
                name = call.function.name
            if call.function.arguments:
                args += call.function.arguments
        if choice.finish_reason:
            finish = choice.finish_reason
    assert name == "get_weather"
    assert args == '{"city":"Paris"}'
    assert finish == "tool_calls"


@scenario("vision content parts pass through")
def s_vision():
    r = client.chat.completions.create(
        model="openai/gpt-4o",
        messages=[{
            "role": "user",
            "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "image_url",
                 "image_url": {"url": "data:image/png;base64,aGVsbG8="}},
            ],
        }],
    )
    assert r.choices[0].message.content == "mock response"


@scenario("404 for unknown model raises NotFoundError")
def s_unknown_model():
    try:
        client.chat.completions.create(model="never-heard-of-it", messages=[])
        raise AssertionError("expected NotFoundError")
    except openai.NotFoundError as e:
        assert "unknown model" in str(e)


@scenario("429 surfaces as RateLimitError with retry disabled")
def s_rate_limit():
    no_retry = client.with_options(max_retries=0)
    try:
        no_retry.chat.completions.create(model="openai/err-429", messages=[])
        raise AssertionError("expected RateLimitError")
    except openai.RateLimitError as e:
        assert e.response.headers.get("retry-after") == "7"


@scenario("500 surfaces as InternalServerError")
def s_server_error():
    no_retry = client.with_options(max_retries=0)
    try:
        no_retry.chat.completions.create(model="openai/err-500", messages=[])
        raise AssertionError("expected InternalServerError")
    except openai.InternalServerError:
        pass


@scenario("SDK retry-after honored transparently on 429->retry")
def s_sdk_retries():
    # The SDK's own retry logic must see well-formed 429s; with 1 retry it
    # fails after ~2 attempts, proving retry-after parsing worked.
    retrying = client.with_options(max_retries=1, timeout=httpx.Timeout(30.0))
    try:
        retrying.chat.completions.create(model="openai/err-429", messages=[])
        raise AssertionError("expected RateLimitError after retries")
    except openai.RateLimitError:
        pass


@scenario("embeddings endpoint")
def s_embeddings():
    r = client.embeddings.create(model="openai/gpt-4o", input="hello")
    assert r.data[0].embedding == [0.1, 0.2, 0.3]


@scenario("models list")
def s_models():
    ids = [m.id for m in client.models.list()]
    assert any("/" in i for i in ids), ids


if __name__ == "__main__":
    failures = 0
    for fn in [s_sync, s_stream, s_tools, s_vision, s_unknown_model,
               s_rate_limit, s_server_error, s_sdk_retries, s_embeddings, s_models]:
        try:
            fn()
        except Exception as e:  # noqa: BLE001 - report and continue
            failures += 1
            print(f"FAIL {fn.__name__}: {type(e).__name__}: {e}")
    print(f"\n{len(passed)} passed, {failures} failed")
    sys.exit(1 if failures else 0)
