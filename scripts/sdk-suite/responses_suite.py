"""OpenAI SDK Responses-API scenarios against caret-router: native relay
(openai target) and stateless translation (anthropic target)."""

import sys

from openai import OpenAI

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18091"
client = OpenAI(base_url=f"{BASE}/v1", api_key="unused")

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


def s_relay_sync():
    r = client.responses.create(model="openai/gpt-4o", input="hi")
    assert r.object == "response"
    assert r.output_text == "mock response"
    assert r.usage.total_tokens == 10


def s_relay_stream():
    text = ""
    with client.responses.stream(model="openai/gpt-4o", input="hi") as stream:
        for event in stream:
            if event.type == "response.output_text.delta":
                text += event.delta
        final = stream.get_final_response()
    assert text == "mock stream"
    assert final.status == "completed"


def s_translate_sync():
    r = client.responses.create(
        model="anthropic/claude-x", input="hi", instructions="be brief",
    )
    assert r.output_text == "mock response"
    assert r.usage.input_tokens == 11


def s_translate_stream():
    text = ""
    with client.responses.stream(model="anthropic/claude-x", input="hi") as stream:
        for event in stream:
            if event.type == "response.output_text.delta":
                text += event.delta
        final = stream.get_final_response()
    assert text == "mock stream"
    assert final.output[0].content[0].text == "mock stream"


def s_translate_function_call():
    r = client.responses.create(
        model="anthropic/claude-x",
        input="weather?",
        tools=[{"type": "function", "name": "get_weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}],
    )
    calls = [o for o in r.output if o.type == "function_call"]
    assert calls, "expected a function call"
    assert calls[0].name == "get_weather"
    import json
    assert json.loads(calls[0].arguments)["city"] == "Paris"


def s_state_gate():
    import openai
    try:
        client.responses.create(model="anthropic/claude-x", input="hi", store=True)
        raise AssertionError("expected BadRequestError")
    except openai.BadRequestError as e:
        assert "stateless" in str(e)


if __name__ == "__main__":
    for name, fn in [
        ("relay sync", s_relay_sync),
        ("relay stream", s_relay_stream),
        ("translate sync (anthropic)", s_translate_sync),
        ("translate stream (anthropic)", s_translate_stream),
        ("translate function call", s_translate_function_call),
        ("statefulness gate", s_state_gate),
    ]:
        run(name, fn)
    print(f"\n{len(passed)} passed, {failures} failed")
    sys.exit(1 if failures else 0)
