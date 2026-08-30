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

# A Claude seat as an operator actually has one: the document Claude Code
# writes, already expired, carrying a refresh token. Inline `sk-ant-...`
# strings cannot renew and so cannot exercise any of this — a seat backed
# by a file is the only kind that can, which is exactly why the Claude pool
# had none until now.
(d / "claude-kris.json").write_text(json.dumps({
    "claudeAiOauth": {
        "accessToken": "sk-ant-oat01-STALE",
        "refreshToken": "sk-ant-ort01-STALE",
        "expiresAt": 1000,            # 1970. Due for renewal on first use.
        "refreshTokenExpiresAt": 4000000000000,
        "scopes": ["user:profile", "user:inference"],
        "subscriptionType": "max",
        "rateLimitTier": "default_claude_max_20x",
    },
    "organizationUuid": "org-e2e",
}, indent=2))
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
# Two static keys: one naming a service, one not. The named one stands for a
# caller nobody can reconfigure — it sends a bare shared secret and the
# gateway decides on arrival which service that is.
auth_keys = ["e2e-master-key", {{ key = "e2e-kris-static", tenant = "kris" }}]

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

# One provider, one seat, backed by a credential file. Kept apart from
# claude-max above so the renewal assertions cannot be satisfied by some
# other account happening to serve.
[providers.claude-seat]
type = "claude_subscription"
base_url = "__UPSTREAM__"
keys = [
  {{ name = "seat-claude-kris", value = "file:{d}/claude-kris.json", tenant = "kris" }},
]

[aliases]
"gpt-5.6-sol" = "codex/gpt-5.6-sol"
"claude-renewing" = "claude-seat/claude-sonnet-5"
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
# A refresh token rotates the moment the real endpoint answers, so this can
# only ever run against the stand-in. Unset in production; see token_url.
RAPID_CLAUDE_OAUTH_URL="http://127.0.0.1:$UP_PORT/oauth/token" \
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
echo "a caller that cannot be reconfigured"
# The whole point: this key carries no service in the request, only in the
# gateway's config. It must reach kris's accounts on a divided pool, while
# the unnamed static key beside it reaches nothing.
sk() { curl -s -m 20 -o /dev/null -w "%{http_code}" -X POST "$GW/v1/responses" \
  -H "Authorization: Bearer $1" -H "Content-Type: application/json" \
  -d "{\"model\":\"gpt-5.6-sol\",\"input\":\"ping\",\"stream\":false}"; }
: > "$UP_LOG"
check "a static key naming a service reaches that service's accounts" "$(sk e2e-kris-static)" 200
check "  and it was that service's account that served"               "$(seats_used)" "acct-kris"
check "a static key naming none still reaches nothing"                "$(sk e2e-master-key)" 403

echo
echo "warning the operator at startup"
# The rollout's one unforgiving step: labelling the first account cuts off
# every caller that cannot name a service. This fixture is exactly that
# state — labelled accounts plus a static gateway key — so the warning has
# to be in the log, or the operator finds out from a 403 instead.
grep -q "names no service" "$WORK/gateway.log" \
  && ok "a divided pool warns about the static key that names no service" \
  || bad "a divided pool warns about the static key that names no service" "nothing in the gateway log"
grep -q "name no service" "$WORK/gateway.log" \
  && ok "and names the virtual keys that own nothing" \
  || bad "and names the virtual keys that own nothing" "nothing in the gateway log"

echo
echo "moving an account between services"
# Assert what it means rather than an exact cumulative list: acct-spare is
# labelled for nobody, so no amount of traffic from anyone may reach it.
case "$(seats_used)" in
  *acct-spare*) bad "an unassigned account serves nobody" "acct-spare served traffic" ;;
  *) ok "an unassigned account serves nobody" ;;
esac
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
echo "a Claude seat renews itself"
# Claude accounts were never poolable the way Codex ones are, and the reason
# was never the ownership rule — it was that nothing renewed their tokens, so
# a seat died within hours of being added. This is that gap, closed and
# proved end to end: an expired credential, a real renewal round trip, and a
# request served with the token that came back.
: > "$UP_LOG"
CLAUDE_CODE=$(curl -s -m 20 -o "$WORK/claude.out" -w '%{http_code}' -X POST "$GW/v1/chat/completions" \
  -H "Authorization: Bearer $KRIS" -H 'Content-Type: application/json' \
  -d '{"model":"claude-renewing","messages":[{"role":"user","content":"ping"}]}')
check "an expired seat still serves the request" "$CLAUDE_CODE" 200

oauth_field() {  # read one field off the recorded refresh
  python3 - "$UP_LOG" "$1" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    r = json.loads(line)
    if r.get("oauth"):
        print(r.get(sys.argv[2]) or "")
        break
else:
    print("NO-REFRESH")
PY
}
check "it renewed the credential first"          "$(oauth_field grant_type)" "refresh_token"
check "  as JSON, which is Anthropic's encoding" "$(oauth_field content_type)" "application/json"
check "  against the Claude Code client id"      "$(oauth_field client_id)" "9d1c250a-e61b-44d9-88ed-5944d1962f5e"
check "  presenting the stale refresh token"     "$(oauth_field sent_refresh_token)" "sk-ant-ort01-STALE"
# Scopes come off the document, so a renewal cannot quietly re-grant the
# seat a different set than the one it was issued with.
check "  and the scopes the seat already held"   "$(oauth_field scope)" "user:profile user:inference"

vendor_token() {
  python3 - "$UP_LOG" <<'PY'
import json, sys
for line in open(sys.argv[1]):
    r = json.loads(line)
    if not r.get("oauth"):
        print(r.get("bearer") or "")
        break
else:
    print("NO-REQUEST")
PY
}
check "the vendor saw the renewed token, not the stale one" "$(vendor_token)" "sk-ant-oat01-REFRESHED"

# Persistence is the half that strands a seat when it is wrong: the refresh
# token rotated upstream, so if the new one is not on disk the seat can
# never renew again and needs a human with a browser.
disk() {  # one field out of the credential file the gateway rewrote
  python3 - "$WORK/claude-kris.json" "$1" <<'PY'
import json, sys
doc = json.load(open(sys.argv[1]))
print(doc.get("claudeAiOauth", doc).get(sys.argv[2], "MISSING"))
PY
}
check "the rotated refresh token was persisted" "$(disk refreshToken)" "sk-ant-ort01-ROTATED"
check "  and the new access token with it"      "$(disk accessToken)"  "sk-ant-oat01-REFRESHED"
# expires_in is relative seconds; the document wants an absolute instant in
# milliseconds. Getting this wrong benches the seat on every later request.
NOW_MS=$(python3 -c 'import time;print(int(time.time()*1000))')
EXP=$(disk expiresAt)
python3 -c "import sys;n=int(sys.argv[1]);e=int(sys.argv[2]);sys.exit(0 if n < e <= n+3700000 else 1)" "$NOW_MS" "$EXP" \
  && ok "expiry was rewritten as an absolute instant about an hour out" \
  || bad "expiry was rewritten as an absolute instant about an hour out" "now=$NOW_MS expiresAt=$EXP"
# The file belongs to the Claude Code CLI; a renewal must leave it usable.
check "members the gateway does not model survived" "$(disk subscriptionType)" "max"

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
