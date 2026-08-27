# rapid-router Documentation

**rapid-router** is an open-source LLM gateway written in Rust: one
OpenAI-compatible API in front of every major LLM provider, with
**microseconds — not milliseconds — of added latency**.

Point any OpenAI SDK at `http://localhost:8080/v1`, set
`"model": "anthropic/claude-sonnet-4-5"`, and rapid-router translates the
request to Anthropic's wire format, streams the response back in OpenAI chunk
format, load-balances across your API keys, and fails over to backup
providers when one degrades — all from a single static binary that carries
its own storage, web console, and fleet mode: one box works with nothing
else installed, and three boxes form a fleet with nothing else installed.

```bash
export OPENAI_API_KEY=sk-... ANTHROPIC_API_KEY=sk-ant-...
rapid-router                       # zero-config start on :8080

curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model": "anthropic/claude-sonnet-4-5", "messages": [{"role":"user","content":"hi"}]}'
```

## Documentation map

### Start here
| Doc | Contents |
|---|---|
| [01-overview.md](01-overview.md) | What rapid-router is, design goals, feature summary, non-goals |
| [guides/quickstart.md](guides/quickstart.md) | Install, run, connect your SDK in five minutes |
| [guides/coding-agents.md](guides/coding-agents.md) | Claude Code, Codex, and other agents through the gateway |

### Architecture
| Doc | Contents |
|---|---|
| [architecture/01-system-design.md](architecture/01-system-design.md) | System diagram, crate layout, technology stack, design principles |
| [architecture/02-request-lifecycle.md](architecture/02-request-lifecycle.md) | Life of a request, stage by stage, with latency budgets |
| [architecture/03-concurrency-and-memory.md](architecture/03-concurrency-and-memory.md) | Task model, backpressure, allocation discipline |
| [architecture/04-json-engine.md](architecture/04-json-engine.md) | The three-tier JSON strategy behind the latency numbers |
| [architecture/05-security.md](architecture/05-security.md) | Secret handling, gateway auth, supply-chain posture |
| [architecture/06-state-and-storage.md](architecture/06-state-and-storage.md) | Where every piece of state lives — the embedded replicated store; no external services required |

### API surface
| Doc | Contents |
|---|---|
| [api/01-endpoints.md](api/01-endpoints.md) | Every endpoint; translate vs relay vs passthrough; multimodal |
| [api/02-dialects.md](api/02-dialects.md) | Inbound dialects (`/v1`, `/anthropic`, `/genai`) and the cross-dialect matrix |
| [api/03-streaming.md](api/03-streaming.md) | SSE translation, TTFT discipline, cancellation |
| [api/04-errors.md](api/04-errors.md) | Error taxonomy, wire shapes, retry semantics |

### Components
| Doc | Contents |
|---|---|
| [components/router.md](components/router.md) | Model resolution, key selection, fallbacks, circuit breakers |
| [components/provider-adapters.md](components/provider-adapters.md) | The `Provider` trait and translation rules |
| [components/config.md](components/config.md) | Configuration format, validation, hot reload |
| [components/hooks.md](components/hooks.md) | Middleware layers and the `Hook` extension trait |
| [components/observability.md](components/observability.md) | Metrics, tracing, logging, response headers |
| [components/console.md](components/console.md) | The web console: product pages, bundling into the binary, design system |
| [components/virtual-keys.md](components/virtual-keys.md) | Virtual keys: scoped credentials, budgets, rate limits, lifecycle |
| [components/agent-subscriptions.md](components/agent-subscriptions.md) | Serving traffic from Claude Code / Codex subscription seats, and what is not yet verified |
| [components/account-pools.md](components/account-pools.md) | One pool of provider accounts, and the service label that says who may spend each one |
| [guides/migrating-optimizer-and-kris.md](guides/migrating-optimizer-and-kris.md) | Moving the optimizer and Kris off their own credentials and onto the shared pool |

### Providers & operations
| Doc | Contents |
|---|---|
| [providers.md](providers.md) | Supported providers, capability matrix, per-provider notes |
| [operations/deployment.md](operations/deployment.md) | Binary, Docker, sizing, shutdown, scaling |
| [operations/scaling.md](operations/scaling.md) | Horizontal scaling on any substrate — LB requirements, scaling signals, platform notes |
| [operations/fleet.md](operations/fleet.md) | Fleet mode: stateless nodes over a shared S3 or DynamoDB store |
| [operations/reliability.md](operations/reliability.md) | Timeouts, retries, breakers — operator's view |
| [operations/benchmarking.md](operations/benchmarking.md) | The in-repo rigs and how we keep the numbers honest |
| [roadmap.md](roadmap.md) | Release scope and what comes next |
