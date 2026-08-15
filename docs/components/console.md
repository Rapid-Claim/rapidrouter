# The Web Console

A single-page app served **by the gateway binary itself** at `/console`.
There is no separate frontend deployment, no Node runtime in production, no
CDN, no external font or script — the console works identically on a
laptop, an air-gapped VM, and a cluster node, because it is a feature of
the port you already run.

## How it's bundled into the binary

- The console is a workspace directory (`console/`) built at release time
  (Vite) into static assets: content-hashed filenames, pre-compressed
  (brotli + gzip) at build, source maps stripped.
- The build output is embedded into `router-server` at compile time
  (`rust-embed`), behind a `console` cargo feature — **on by default**,
  removable for minimal builds (`--no-default-features` drops ~1.5 MB).
- Serving: immutable cache headers for hashed assets, SPA fallback to
  `index.html`, precompressed variants negotiated by `Accept-Encoding`.
  Zero requests leave the user's machine except to the gateway itself —
  a privacy property and an air-gap property, not just a performance one.
- Budget: **≤ 250 KB gzipped total JS**, enforced in CI like the latency
  budgets. The gateway's frontend does not get to be the slow part.

## Frontend stack

| Concern | Choice | Why |
|---|---|---|
| Framework | SolidJS + TypeScript | compiles away; smallest runtime among mainstream reactive frameworks; no VDOM cost on dense tables |
| Styling | vanilla CSS with design tokens (custom properties); no CSS framework | the design system is small and owned; tokens drive light/dark |
| Charts | uPlot for time series; hand-rolled SVG for sparklines/gauges | µs-resolution histograms need a chart lib that can draw 10k points without jank |
| Data layer | thin typed client over `/admin/api/*` + one SSE subscription | polling for slow data, server-push for live data; no state-management framework |
| Build | Vite, single entry, route-level code splitting | hashed chunks per page keep first paint under budget |

## How it works against the gateway

- The console is a pure client of the **admin API** — everything it does is
  scriptable without it:

```
GET  /admin/api/config              # current document (secrets as env.*/store.* refs)
PUT  /admin/api/config              # validate + versioned CAS write
GET  /admin/api/usage?window=24h&by=model,key
GET  /admin/api/requests?limit=200&key=…&status=…
GET  /admin/api/keys · POST /admin/api/keys · /admin/api/keys/{id}/…
GET  /admin/api/fleet               # members, roles, applied versions, health
GET  /admin/api/events              # SSE: live request ticks, breaker flips, config applies
POST /admin/api/test                # playground dispatch (ordinary data plane, marked)
```

- **Writes** go through the replicated store with versioned compare-and-swap:
  the editor holds the version it read; a concurrent edit surfaces as a
  visible conflict with a diff, never a lost update. In `file` config mode
  all write affordances render disabled with a "config is file-managed"
  banner — same product, honest about its mode.
- **Fleet data** is scatter-gathered from peers server-side; the browser
  always talks to one node and gets the whole cluster's view.
- **Live updates** ride one SSE connection (`/admin/api/events`): dashboard
  tiles tick, breaker states flip, and config-applied toasts appear without
  reloads — and without websockets, so every proxy that can carry the data
  plane can carry the console.

## The pages (product decisions)

Eight pages. Each exists because an operator question exists; anything that
answers no question was cut.

| Page | The question it answers | Contents |
|---|---|---|
| **Overview** | "is the gateway healthy right now?" | traffic/tokens/cost/error tiles with sparklines; overhead p50/p99 chart; provider health strip (breaker states at a glance); live request ticker |
| **Providers** | "what's behind the gateway and is it working?" | provider cards: key pool with weights and per-key health, breaker state, concurrency, latency; add/edit provider; guided key entry (`env.*` or sealed `store.*`); one-click test request |
| **Routing** | "where do requests go?" | model catalog; alias table; fallback-chain editor with a visual chain and capability warnings ("this chain crosses a dialect; json_schema will be emulated") |
| **Keys** | "who can spend what?" | virtual-key table: scope, budget bar, limit status, last-used; create/rotate/revoke flows (secret shown once); per-key drill-down of spend and model mix ([virtual-keys.md](virtual-keys.md)) |
| **Usage** | "what did we spend and on what?" | time-series and breakdowns by provider/model/key/tag; period comparisons; CSV/JSON export; budget burn-down per key |
| **Requests** | "what just happened?" | filterable recent-request log (metadata only — bodies never leave the gateway unless body-logging was explicitly enabled); detail drawer: timings waterfall, fallback trail, receipt headers, usage |
| **Playground** | "does this model/route actually work?" | chat pane against any configured model or alias; streaming with TTFT/overhead readout; shows the equivalent `curl`; requests are tagged `playground` in usage |
| **Cluster** | "are all my boxes okay?" | member table: role, version, applied config version, lag, breaker summary; join instructions with the token; degraded-quorum warnings in plain language |

Plus **Settings** (admin keys, config mode, `config export` download,
usage retention, appearance) — reachable from the nav footer, deliberately
not a first-class page.

Product rules:

- **Read is instant, write is deliberate.** Every mutation shows a diff of
  the config document and applies atomically; there is no "save" that half
  happened. Dangerous actions (revoke, remove node) name their blast radius.
- **Truth over reassurance.** Budget lag, per-node limit shares, file-mode
  read-only, degraded quorum — the UI states these plainly rather than
  pretending precision it doesn't have.
- **Every view is an API call you can copy.** A "view as curl" affordance
  on each page keeps the console honest and the API documented by use.
- **No bodies by default, anywhere.** The explorer shows metadata; prompt
  content appears only if body-logging was explicitly enabled in config,
  and the page says so.

## Design language

The console should look like a precision instrument: calm, dense, fast.

- **Tokens, not decisions-per-page**: a small set of CSS custom properties
  (surface/ink scales, one accent, four semantic status colors
  ok/warn/error/muted, spacing scale, radius, type ramp) drives everything;
  light and dark themes are token swaps honoring `prefers-color-scheme`.
- **Typography**: system font stack (no font downloads); **tabular
  numerals everywhere numbers align** — metrics, tables, budgets; type
  ramp of four sizes; generous line-height in prose, tight in tables.
- **Color discipline**: neutral surfaces carry the UI; the accent is for
  interaction, not decoration; status colors appear only when they mean
  something (a green wall says nothing — health is shown by absence of
  warnings). Charts use a single consistent categorical palette with
  ok/warn/error reserved.
- **Density with air**: operator tables are compact (rows ~32 px) but the
  layout keeps a fixed rhythm from the spacing scale; empty states teach
  (each shows the CLI command that would populate the page).
- **Motion**: 120–160 ms ease-out transitions, live-ticking numbers
  interpolate; nothing bounces; `prefers-reduced-motion` respected.
- **Accessibility is a gate, not a hope**: WCAG AA contrast in both
  themes, full keyboard navigation (`g o` → Overview, `g k` → Keys, `/` →
  filter), visible focus rings, aria-live on async results — checked in CI
  (axe) alongside the bundle budget.
- **Perceived performance**: skeletons under 100 ms, optimistic UI only
  for reversible actions, and the overhead chart renders 10k points
  without dropping frames — the console of a microseconds gateway is not
  allowed to feel slow.

## Repository structure

```
console/
├── src/
│   ├── routes/            # one directory per page (overview/, providers/, …)
│   ├── components/        # tables, charts, forms, status primitives
│   ├── lib/api.ts         # typed admin-API client (generated from the server's schema)
│   ├── lib/events.ts      # SSE subscription + stores
│   └── styles/tokens.css  # the design system
├── e2e/                   # Playwright: every page against a seeded gateway
└── vite.config.ts
```

CI builds the console, runs axe + bundle-budget + Playwright against a
gateway with fixture data, and the release job embeds the output — the
frontend has the same "measured, not asserted" bar as the hot path.

## Security posture

- **Off by default**: `/console` and `/admin/api/*` exist only when
  `[console] admin_keys` is configured — separate credentials from
  data-plane keys, same constant-time handling; sessions are short-lived
  tokens minted from an admin key, httpOnly, SameSite=Strict.
- Secrets are write-only: the console submits values but reads back only
  references (`env.*` / `store.*`) — it cannot leak what it never receives.
- CSRF-safe by construction (no cookies on state-changing API calls without
  the custom header), strict CSP (self-only — trivial since nothing is
  external), no analytics of any kind.
- Serve it on your internal network or behind your SSO proxy; the gateway
  does not grow a user-management system.

## Powering your own web apps (the other kind of "web app")

Applications — including browsers — consume the gateway like any other
client. Two rules:

1. **Browsers never hold provider keys.** Issue a scoped virtual key with
   its own budget and rate limit ([virtual-keys.md](virtual-keys.md));
   revoke it without touching provider credentials.
2. If the browser calls the gateway directly (no backend), enable CORS
   (`[server.cors] allowed_origins = […]`) — streaming works from
   `fetch()` out of the box. The safer default remains browser → your
   backend → gateway.
