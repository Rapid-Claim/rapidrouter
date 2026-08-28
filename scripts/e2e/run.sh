#!/usr/bin/env bash
#
# End-to-end proof that account ownership actually holds, over real HTTP.
#
# Unit tests can show that `owned_by` returns false. They cannot show that a
# request from the optimizer reached the gateway, was matched to the
# optimizer's virtual key, chose an account labelled `optimizer`, and put
# *that account's* credential on the wire to the vendor. Every one of those
# steps has been wrong at some point in this feature's life, and two of them
# were wrong while the unit tests were green.
#
# So this starts a real gateway against a recording stand-in for the vendor,
# and reads what came out the far side.
#
#   ./run.sh              run the assertions and exit non-zero on failure
#   ./run.sh --hold       leave the gateway up and print how to drive it
#                         with the real CLIs (see docs/guides/coding-agents.md)
#
# Needs: the `rapid-router` binary, python3, curl. Point ROUTER_BIN at a
# build, or let it find target/{debug,release}.
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
PORT="${PORT:-8099}"
# 127.0.0.1 by default so a self-test exposes nothing. Set HOST=0.0.0.0 when
# the gateway runs in a container and the CLIs driving it do not.
HOST="${HOST:-127.0.0.1}"
HOLD=0
[ "${1:-}" = "--hold" ] && HOLD=1

ROUTER_BIN="${ROUTER_BIN:-}"
if [ -z "$ROUTER_BIN" ]; then
  for c in "$ROOT/target/debug/rapid-router" "$ROOT/target/release/rapid-router" \
           "${CARGO_TARGET_DIR:-/target}/debug/rapid-router"; do
    [ -x "$c" ] && ROUTER_BIN="$c" && break
  done
fi
if [ ! -x "${ROUTER_BIN:-}" ]; then
  echo "no rapid-router binary; build one or set ROUTER_BIN" >&2; exit 2
fi

WORK="$(mktemp -d)"; UP_LOG="$WORK/upstream.jsonl"; : > "$UP_LOG"
cleanup() {
  [ -n "${UP_PID:-}" ] && kill "$UP_PID" 2>/dev/null
  [ -n "${GW_PID:-}" ] && kill "$GW_PID" 2>/dev/null
  [ "$HOLD" = 0 ] && rm -rf "$WORK"
}
trap cleanup EXIT

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); printf '  \033[32mok\033[0m   %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  \033[31mFAIL\033[0m %s\n     %s\n' "$1" "$2"; }
check(){ [ "$2" = "$3" ] && ok "$1" || bad "$1" "expected $3, got $2"; }

# --- fixtures -------------------------------------------------------------
# Three Codex seats on one provider. Two are labelled; the third is left
# unassigned on purpose, because "an account nobody owns serves nobody" is
# one of the rules under test, and because moving it later proves a
# reallocation takes effect on a running gateway.
python3 - "$WORK" <<'PY'
import base64, json, pathlib, sys
d = pathlib.Path(sys.argv[1])
enc = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()
jwt = lambda c: "%s.%s.%s" % (enc(b'{"alg":"RS256"}'), enc(json.dumps(c).encode()), enc(b"sig"))
for name in ("opt", "kris", "spare"):
    acct = "acct-" + name
    (d / ("auth-%s.json" % name)).write_text(json.dumps({
        "auth_mode": "chatgpt",
        "tokens": {"access_token": jwt({"exp": 4000000000, "acct": acct}),
                   "refresh_token": "rt-" + name,
                   "id_token": jwt({"https://api.openai.com/auth": {"chatgpt_account_id": acct}}),
                   "account_id": acct}}))
PY

OPT_SECRET=optimizersecret0123; KRIS_SECRET=krissecret0123456789; NONE_SECRET=plainsecret012345678
hash_of() { "$ROUTER_BIN" key hash "$1"; }

python3 - "$WORK" "$(hash_of "$OPT_SECRET")" "$(hash_of "$KRIS_SECRET")" "$(hash_of "$NONE_SECRET")" <<'PY'
import pathlib, sys
d, opt, kris, none = pathlib.Path(sys.argv[1]), *sys.argv[2:5]
(d / "gateway.toml").write_text(f'''tenants = ["optimizer", "kris"]

[server]
host = "__HOST__"
port = __PORT__
auth_keys = ["e2e-master-key"]

[console]
admin_keys = ["e2e-master-key"]

[providers.codex]
type = "codex_subscription"
base_url = "__UPSTREAM__"
keys = [
  # rpm = 2 makes the seat identifiable from outside: exhaust it and see
  # whether the caller is refused (confined, correct) or quietly served by
  # somebody else's account (the bug this whole change exists to prevent).
  {{ name = "seat-opt",   value = "file:{d}/auth-opt.json",   tenant = "optimizer", rpm = 2 }},
  {{ name = "seat-kris",  value = "file:{d}/auth-kris.json",  tenant = "kris" }},
  {{ name = "seat-spare", value = "file:{d}/auth-spare.json" }},
]

# Kris drives Claude Code, so the fixture carries a Claude pool too — same
# rule, second dialect, and it is the pool the ownership check has to hold
# on when two providers are divided differently.
[providers.claude-max]
type = "claude_subscription"
base_url = "__UPSTREAM__"
keys = [
  {{ name = "claude-opt",  value = "sk-ant-oat01-opt-seat",  tenant = "optimizer" }},
  {{ name = "claude-kris", value = "sk-ant-oat01-kris-seat", tenant = "kris" }},
]

[aliases]
"gpt-5.6-sol" = "codex/gpt-5.6-sol"
# What the CLIs actually ask for, so a run does not 404 on a name the
# gateway has never been told about.
"sonnet" = "claude-max/claude-sonnet-5"
"claude-sonnet-5" = "claude-max/claude-sonnet-5"
"claude-opus-5" = "claude-max/claude-opus-5"

[[virtual_keys]]
id = "0a1b2c"
name = "optimizer"
secret_hash = "{opt}"
tenant = "optimizer"

[[virtual_keys]]
id = "0d4e5f"
name = "kris"
secret_hash = "{kris}"
tenant = "kris"

[[virtual_keys]]
id = "0e6f70"
name = "unassigned"
secret_hash = "{none}"
''')
PY

# --- bring it up ----------------------------------------------------------
UP_PORT="${UP_PORT:-8802}"
python3 "$HERE/upstream.py" "$UP_PORT" "$UP_LOG" > "$WORK/upstream.out" 2>&1 &
UP_PID=$!
curl -sf -m 10 --retry 20 --retry-all-errors --retry-delay 1 -o /dev/null \
  "http://127.0.0.1:$UP_PORT/ping" || { echo "upstream did not start" >&2; cat "$WORK/upstream.out"; exit 2; }

sed -e "s|__UPSTREAM__|http://127.0.0.1:$UP_PORT|" -e "s|__PORT__|$PORT|" -e "s|__HOST__|$HOST|" \
    "$WORK/gateway.toml" > "$WORK/gateway.live.toml"
"$ROUTER_BIN" --config "$WORK/gateway.live.toml" > "$WORK/gateway.log" 2>&1 &
GW_PID=$!
GW="http://127.0.0.1:$PORT"
curl -s -m 20 --retry 25 --retry-all-errors --retry-delay 1 -o /dev/null \
  -H "Authorization: Bearer e2e-master-key" "$GW/v1/models" \
  || { echo "gateway did not start" >&2; tail -20 "$WORK/gateway.log"; exit 2; }

OPT="ck-0a1b2c-$OPT_SECRET"; KRIS="ck-0d4e5f-$KRIS_SECRET"; NONE="ck-0e6f70-$NONE_SECRET"
say() {  # one request; echoes the HTTP status
  curl -s -m 20 -o /dev/null -w '%{http_code}' -X POST "$GW/v1/responses" \
    -H "Authorization: Bearer $1" -H 'Content-Type: application/json' \
    -d '{"model":"gpt-5.6-sol","input":"ping","stream":false}'
}
seats_used() {  # which upstream accounts served, from the tokens presented
  python3 - "$UP_LOG" <<'PY'
import base64, json, sys
pad = lambda s: s + "=" * (-len(s) % 4)
seen = []
for line in open(sys.argv[1]):
    r = json.loads(line)
    try:
        acct = json.loads(base64.urlsafe_b64decode(pad(r["bearer"].split(".")[1])))["acct"]
    except Exception:
        acct = "?"
    if acct not in seen:
        seen.append(acct)
print(",".join(sorted(seen)))
PY
}

echo
echo "gateway on $GW, vendor stand-in on 127.0.0.1:$UP_PORT"
echo

# --- the assertions -------------------------------------------------------
echo "ownership"
check "a key naming no service is refused on a divided pool" "$(say "$NONE")" 403

# seat-opt allows two requests a minute and is the only account this
# service owns, so the third has nowhere legitimate to go. Counting all
# three in one breath keeps the allowance the assertion's own, rather than
# something an earlier check already spent.
: > "$UP_LOG"
s1=$(say "$OPT"); s2=$(say "$OPT"); s3=$(say "$OPT")
check "a service reaches its own account"                         "$s1"     200
check "  and spends its whole allowance"                          "$s1$s2"  "200200"
check "a service is confined to its own account when it runs out" "$s3"     503
check "it never reached another service's account" "$(seats_used)" "acct-opt"

check "another service is untouched by that exhaustion" "$(say "$KRIS")" 200
check "  and was served by its own account" "$(seats_used)" "acct-kris,acct-opt"

echo
echo "what the gateway put on the wire"
LAST=$(tail -1 "$UP_LOG")
check "it addressed the vendor's Codex path" \
      "$(python3 -c "import json,sys;print(json.loads(sys.argv[1])['path'])" "$LAST")" \
      "/backend-api/codex/responses"
check "it identified itself as the CLI, which a seat requires" \
      "$(python3 -c "import json,sys;print(json.loads(sys.argv[1])['originator'])" "$LAST")" \
      "codex_cli_rs"
check "it claimed the serving account, not another" \
      "$(python3 -c "import json,sys;print(json.loads(sys.argv[1])['account_header'])" "$LAST")" \
      "acct-kris"

echo
echo "moving an account between services"
check "an unassigned account serves nobody" "$(seats_used)" "acct-kris,acct-opt"
curl -s -m 10 -o /dev/null -X PUT \
  "$GW/admin/api/providers/codex/keys/seat-spare/tenant" \
  -H "Authorization: Bearer e2e-master-key" -H 'Content-Type: application/json' \
  -d '{"tenant":"optimizer"}'
: > "$UP_LOG"
check "after the move, the service it went to can use it" "$(say "$OPT")" 200
check "and it is that account serving"                    "$(seats_used)" "acct-spare"

echo
echo "when the vendor refuses"
: > "$UP_LOG"
curl -s -m 10 -o /dev/null -X POST "http://127.0.0.1:$UP_PORT/_control" -d '{"code":429}'
say "$KRIS" > /dev/null
check "a vendor 429 never falls back to another service's account" "$(seats_used)" "acct-kris"
curl -s -m 10 -o /dev/null -X POST "http://127.0.0.1:$UP_PORT/_control" -d '{"code":200}'

echo
printf '%d passed, %d failed\n' "$PASS" "$FAIL"

if [ "$HOLD" = 1 ]; then
  cat <<EOF

Gateway held on $GW. To drive it with the real agent CLIs:

  # the optimizer's own runners (from the rapid-optimizer checkout)
  RAPID_E2E_ROUTER_URL=$GW \\
  RAPID_E2E_ROUTER_KEY=$OPT \\
  RAPID_E2E_ADMIN_KEY=e2e-master-key \\
    go test ./internal/adapters/runtime/codexapp/ ./internal/adapters/runtime/claudecode/ -run TestE2E -v

  # what the gateway sent upstream
  tail -f $UP_LOG

Ctrl-C to stop.
EOF
  wait "$GW_PID"
fi
[ "$FAIL" -eq 0 ]
