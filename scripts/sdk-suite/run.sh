#!/usr/bin/env bash
# Run the SDK scenario suites against a fresh gateway + mock provider.
# Usage: scripts/sdk-suite/run.sh [python-with-sdks]
set -euo pipefail
cd "$(dirname "$0")/../.."
PY="${1:-/tmp/sdkvenv/bin/python}"
export PATH="$HOME/.cargo/bin:$PATH"

cargo build --release -p router-bin -p mock-provider >/dev/null

./target/release/mock-provider > /tmp/sdk-mock.log 2>&1 & MOCK=$!
trap 'kill $MOCK ${GW:-} 2>/dev/null || true' EXIT
sleep 0.5
MOCK_URL=$(grep -o 'http://[^ ]*' /tmp/sdk-mock.log)

cat > /tmp/sdk-gw.toml <<CFG
[server]
port = 18091

[providers.openai]
base_url = "$MOCK_URL"
keys = [
  { name = "catalog", value = "sk-cat", models = ["gpt-4o-mini"] },
  { name = "wide", value = "sk-suite" },
]

[aliases]
fast = "openai/gpt-4o"

[providers.anthropic]
base_url = "$MOCK_URL"
keys = [{ name = "main", value = "sk-ant-suite" }]

[providers.gemini]
base_url = "$MOCK_URL"
keys = [{ name = "main", value = "sk-gem-suite" }]

# Error-path scenarios hammer err-* stubs on purpose; a tight breaker
# would (correctly) trip and change later scenarios' outcomes. Breaker
# behavior has its own dedicated e2e suite.
[reliability.breaker]
failure_threshold = 1000
CFG

./target/release/caret-router --config /tmp/sdk-gw.toml > /tmp/sdk-gw.log 2>&1 & GW=$!
sleep 0.8

"$PY" scripts/sdk-suite/openai_suite.py http://127.0.0.1:18091
"$PY" scripts/sdk-suite/anthropic_suite.py http://127.0.0.1:18091
