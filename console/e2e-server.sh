#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DATA="$(mktemp -d "${TMPDIR:-/tmp}/rapid-console-e2e.XXXXXX")"
CONFIG="$DATA/config.toml"
cleanup() { rm -rf "$DATA"; }
trap cleanup EXIT

printf '%s\n' \
  '[server]' \
  'host = "127.0.0.1"' \
  'port = 18080' \
  '' \
  '[console]' \
  'admin_keys = ["admin-e2e-key"]' \
  '' > "$CONFIG"
cd "$ROOT"
cargo run --quiet -p router-bin -- --data-dir "$DATA" config import "$CONFIG"
exec cargo run --quiet -p router-bin -- --data-dir "$DATA"
