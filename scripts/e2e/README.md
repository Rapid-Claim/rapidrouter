# End-to-end: does ownership actually hold?

`make e2e` starts a real `rapid-router` against a recording stand-in for the
vendor and reads what came out the far side. `make e2e-hold` leaves it up so
the real agent CLIs can drive it.

## Why this exists rather than more unit tests

A unit test can show that `owned_by` returns false. It cannot show that a
request from a service reached the gateway, matched that service's virtual
key, chose an account labelled for it, and put **that account's** credential
on the wire. Every one of those steps has been wrong at some point here, and
two were wrong while the unit tests were green:

- `OPENAI_BASE_URL` was ignored by codex-cli, so a run that looked routed
  went to `api.openai.com` and spent an unallocated seat. Nothing failed.
- The services roster was attached to the wrong handler, so the console's
  picker would have been silently empty. No test asserted on it.

Both are only visible from outside the process, which is what this is.

## What it asserts

| | the failure it would catch |
|---|---|
| a key naming no service is refused on a divided pool | an unlabelled caller quietly draining a divided pool |
| a service is confined to its own account when it runs out | overflow onto another service's accounts |
| it never reached another service's account | the same, but seen from the vendor's side rather than the status code |
| another service is untouched by that exhaustion | one service's spike starving the rest |
| the gateway addressed the vendor's Codex path, as the CLI | a subscription seat refusing the request outright |
| it claimed the *serving* account | the right seat's token with the wrong account id |
| an unassigned account serves nobody | capacity quietly shared before anyone allocated it |
| after a move, the receiving service can use it | a reallocation that needs a restart to take effect |
| a vendor 429 never falls back to another service's account | ownership holding only on the happy path |

## Reading the evidence

`upstream.py` writes one JSON line per request the gateway made, including
the bearer token it presented — so which account served is established from
the vendor's side, not from anything the gateway claims. Under `--hold` the
log path is printed; `tail -f` it while you drive the CLIs.

`POST /_control {"code": 429}` makes the vendor refuse, which is how the
last assertion is set up.

## Driving it with the real CLIs

`make e2e-hold` prints the exact commands. The optimizer's own runners have
end-to-end tests that take the gateway's address and a virtual key:

    RAPID_E2E_ROUTER_URL=… RAPID_E2E_ROUTER_KEY=ck-… RAPID_E2E_ADMIN_KEY=… \
      go test ./internal/adapters/runtime/codexapp/ -run TestE2E -v

Those spawn the real `codex` and `claude` binaries. They are the only tests
that exercise the client half — the part where an ignored environment
variable sends a run to the vendor with nothing in the logs to say so.

## What it does not prove

The upstream is a stand-in. It answers "what did the gateway send", never
"would `chatgpt.com` accept it". The gateway→vendor leg is the same
transport production has used since 2026-08-18, but no test here has put a
real subscription seat behind it. That still needs one real account.
