import {
  Activity,
  Boxes,
  ChartNoAxesCombined,
  ChevronRight,
  CircleGauge,
  Copy,
  KeyRound,
  LogOut,
  Network,
  Play,
  Plus,
  RefreshCw,
  Route,
  ScrollText,
  Save,
  Server,
  Settings as SettingsIcon,
  Trash2,
} from "lucide-solid";
import {
  For,
  Match,
  Show,
  Switch,
  createEffect,
  createMemo,
  createResource,
  createSignal,
  onCleanup,
  onMount,
} from "solid-js";
import uPlot from "uplot";
import {
  api,
  clearSession,
  login,
  sessionToken,
  type DayBucket,
  type Provider,
  type ProviderKey,
  type QuotaWindow,
  type UsageRecord,
  type VirtualKey,
} from "./api";

type Page =
  | "usage"
  | "activity"
  | "keys"
  | "providers"
  | "models"
  | "routing"
  | "playground"
  | "logs"
  | "cluster"
  | "settings";

/// Ordered by how often an operator needs them, not by the shape of the
/// backend. Usage leads because "what is this costing" is the question
/// people arrive with; configuration sits below the things you watch.
const navigation: Array<{ id: Page; label: string; icon: typeof CircleGauge; group?: string }> = [
  { id: "usage", label: "Usage", icon: ChartNoAxesCombined, group: "Observe" },
  { id: "activity", label: "Model activity", icon: Activity },
  { id: "logs", label: "Logs", icon: ScrollText },
  { id: "keys", label: "Virtual keys", icon: KeyRound, group: "Configure" },
  { id: "providers", label: "Providers", icon: Server },
  { id: "models", label: "Models", icon: Boxes },
  { id: "routing", label: "Routing", icon: Route },
  { id: "playground", label: "Playground", icon: Play, group: "Tools" },
  { id: "cluster", label: "Cluster", icon: Network },
];

/// Settings is deliberately not a first-class page: it lives in the nav
/// footer, reachable by `g ,` like every other destination.
const settingsNav: { id: Page; label: string; icon: typeof CircleGauge; group?: string } = {
  id: "settings",
  label: "Settings",
  icon: SettingsIcon,
};

/// `g` then a letter jumps to a page; `/` focuses the page's filter.
const jumps: Record<string, Page> = {
  u: "usage",
  a: "activity",
  l: "logs",
  k: "keys",
  p: "providers",
  m: "models",
  r: "routing",
  y: "playground",
  c: "cluster",
  ",": "settings",
};

/// The nav group a page belongs to, for the header eyebrow: groups are
/// declared on the first item of each run, so a later page inherits the
/// last heading above it.
function sectionOf(page: Page): string {
  let group = "";
  for (const item of navigation) {
    if (item.group) group = item.group;
    if (item.id === page) return group;
  }
  return "Settings";
}

function routeFromHash(): Page {
  const candidate = location.hash.slice(1) as Page;
  const known = [...navigation, settingsNav];
  return known.some((item) => item.id === candidate) ? candidate : "usage";
}

/// Typing in a field must never be swallowed by a shortcut.
function isTyping(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null;
  if (!el) return false;
  return ["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName) || el.isContentEditable;
}

export function App() {
  const [authenticated, setAuthenticated] = createSignal(Boolean(sessionToken()));
  const [page, setPage] = createSignal<Page>(routeFromHash());
  const [refresh, setRefresh] = createSignal(0);
  const [live, setLive] = createSignal(false);

  const onHash = () => setPage(routeFromHash());
  let pendingJump = false;
  const onKey = (event: KeyboardEvent) => {
    if (event.metaKey || event.ctrlKey || event.altKey || isTyping(event.target)) return;
    if (event.key === "/") {
      const filter = document.querySelector<HTMLElement>("[data-filter]");
      if (filter) {
        event.preventDefault();
        filter.focus();
      }
      return;
    }
    if (pendingJump) {
      pendingJump = false;
      const destination = jumps[event.key.toLowerCase()];
      if (destination) {
        event.preventDefault();
        location.hash = `#${destination}`;
      }
      return;
    }
    if (event.key.toLowerCase() === "g") pendingJump = true;
  };
  onMount(() => {
    addEventListener("hashchange", onHash);
    addEventListener("keydown", onKey);
  });
  createEffect(() => {
    if (!authenticated()) return;
    const events = new EventSource("/admin/api/events");
    events.onopen = () => setLive(true);
    events.onerror = () => setLive(false);
    events.onmessage = () => setRefresh((value) => value + 1);
    onCleanup(() => events.close());
  });
  onCleanup(() => {
    removeEventListener("hashchange", onHash);
    removeEventListener("keydown", onKey);
  });

  const current = createMemo(
    () => [...navigation, settingsNav].find((item) => item.id === page())!,
  );
  return (
    <Show when={authenticated()} fallback={<Login onSuccess={() => setAuthenticated(true)} />}>
    <div class="app-shell">
      <aside class="sidebar">
        <a class="brand" href="#usage" aria-label="Caret Router usage">
          <span class="brand-mark"><Boxes size={18} /></span>
          <span><strong>Caret</strong><small>Router</small></span>
        </a>
        <nav aria-label="Main navigation">
          <For each={navigation}>{(item) => (
            <>
              <Show when={item.group}><p class="nav-group">{item.group}</p></Show>
              <a href={`#${item.id}`} aria-label={item.label} classList={{ active: page() === item.id }}>
                <item.icon size={17} aria-hidden="true" />
                <span>{item.label}</span>
              </a>
            </>
          )}</For>
        </nav>
        <div class="sidebar-foot">
          <a href="#settings" class="foot-link" aria-label="Settings" classList={{ active: page() === "settings" }} title="Settings (g ,)"><SettingsIcon size={17} aria-hidden="true" /></a>
          <div class="live-state"><span classList={{ online: live() }} />{live() ? "Live" : "Reconnecting"}</div>
          <button class="icon-button" title="Sign out" aria-label="Sign out" onClick={() => {
            clearSession();
            setAuthenticated(false);
          }}><LogOut size={17} /></button>
        </div>
      </aside>
      <main>
        <header class="page-header">
          <div><p class="eyebrow">{current().group ?? sectionOf(page())}</p><h1>{current().label}</h1></div>
          <button class="icon-button" title="Refresh data" aria-label="Refresh data" onClick={() => setRefresh((value) => value + 1)}><RefreshCw size={17} /></button>
        </header>
        <div class="page-content">
          <Switch>
            <Match when={page() === "usage"}><Usage refresh={refresh} /></Match>
            <Match when={page() === "activity"}><ModelActivity refresh={refresh} /></Match>
            <Match when={page() === "logs"}><Requests refresh={refresh} /></Match>
            <Match when={page() === "keys"}><Keys refresh={refresh} bump={() => setRefresh((v) => v + 1)} /></Match>
            <Match when={page() === "providers"}><Providers refresh={refresh} /></Match>
            <Match when={page() === "models"}><Models refresh={refresh} /></Match>
            <Match when={page() === "routing"}><Routing refresh={refresh} /></Match>
            <Match when={page() === "playground"}><Playground /></Match>
            <Match when={page() === "cluster"}><Fleet refresh={refresh} /></Match>
            <Match when={page() === "settings"}><Settings refresh={refresh} /></Match>
          </Switch>
        </div>
      </main>
    </div>
    </Show>
  );
}

function Login(props: { onSuccess: () => void }) {
  const [key, setKey] = createSignal("");
  const [error, setError] = createSignal("");
  const [pending, setPending] = createSignal(false);
  return <main class="login-shell">
    <form class="login-panel" onSubmit={async (event) => {
      event.preventDefault();
      setPending(true); setError("");
      try { await login(key()); props.onSuccess(); }
      catch (err) { setError(err instanceof Error ? err.message : "Sign in failed"); }
      finally { setPending(false); }
    }}>
      <span class="brand-mark large"><Boxes size={22} /></span>
      <div><p class="eyebrow">Caret Router</p><h1>Operator sign in</h1><p class="muted">Use the admin key configured on this gateway.</p></div>
      <label>Admin key<input autofocus type="password" autocomplete="current-password" value={key()} onInput={(e) => setKey(e.currentTarget.value)} /></label>
      <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
      <button class="button primary" disabled={pending() || !key()}>{pending() ? "Signing in…" : "Sign in"}<ChevronRight size={16} /></button>
    </form>
  </main>;
}

function Providers(props: { refresh: () => number }) {
  const [providers] = createResource(props.refresh, api.providers);
  const [selected, setSelected] = createSignal<string>("");
  const current = createMemo(
    () => providers()?.data.find((p) => p.name === selected()) ?? providers()?.data[0],
  );
  return <div class="stack-lg">
    <div class="view-toolbar">
      <p class="muted">Every credential, the ceiling it works against, and what the provider last said about it.</p>
    </div>
    <Show when={providers()?.data.length} fallback={<Empty title="No providers configured" action="Add one in Routing, or set a provider environment variable and restart." />}>
      <div class="two-column">
        <section class="panel">
          <SectionTitle title="Configured" subtitle="Select a provider to inspect its credentials" />
          <div class="table-wrap"><table><thead><tr><th>Provider</th><th>Keys</th><th>Health</th></tr></thead><tbody>
            <For each={providers()?.data ?? []}>{(provider) => {
              const worst = () => providerHealth(provider);
              return <tr class="clickable" classList={{ selected: current()?.name === provider.name }}
                         onClick={() => setSelected(provider.name)}>
                <td><strong>{provider.name}</strong><small>{provider.subscription ? "Subscription seats" : provider.kind}</small></td>
                <td>{provider.keys.length}</td>
                <td><Status text={worst().label} tone={worst().tone} /></td>
              </tr>;
            }}</For>
          </tbody></table></div>
        </section>
        <section class="panel">
          <Show when={current()} keyed>{(provider) => <>
            <SectionTitle
              title={provider.name}
              subtitle={provider.base_url ?? "Provider default endpoint"}
              action={<span class="pill" classList={{ accent: provider.subscription }}>{provider.subscription ? "Subscription" : "Metered API"}</span>} />
            <Show when={provider.keys.length}
                  fallback={<Empty title="No credentials on this provider"
                                   action="A keyless provider (a local server) needs none; anything else needs a key in Routing." />}>
              <For each={provider.keys}>{(key) => <CredentialCard providerKey={key} subscription={provider.subscription} />}</For>
            </Show>
          </>}</Show>
        </section>
      </div>
    </Show>
  </div>;
}

/// The health of a provider is the state of its worst key: one benched
/// seat in a healthy pool is the thing an operator needs to see, and an
/// average would hide it.
function providerHealth(provider: Provider): { label: string; tone: "success" | "danger" | "muted" } {
  if (!provider.keys.length) return { label: "No keys", tone: "muted" };
  if (provider.keys.every((k: ProviderKey) => k.health === "benched")) return { label: "Out of quota", tone: "danger" };
  if (provider.keys.some((k: ProviderKey) => k.health === "benched")) return { label: "Partly benched", tone: "danger" };
  if (provider.keys.some((k: ProviderKey) => k.health === "open")) return { label: "Degraded", tone: "danger" };
  return { label: "Ready", tone: "success" };
}

/// One credential. The two provider kinds are genuinely different here:
/// a metered key shows the ceiling *we* configured and how much of it is
/// left, while a seat shows the plan windows the provider reports and
/// which of them is currently refusing traffic.
function CredentialCard(props: { providerKey: ProviderKey; subscription: boolean }) {
  const key = () => props.providerKey;
  const tone = () => ({
    healthy: "success", probing: "warning", open: "danger", benched: "danger",
  } as const)[key().health as "healthy" | "probing" | "open" | "benched"];
  return <div style={{ "border-top": "1px solid var(--border)", padding: "16px 0" }}>
    <div class="section-title" style={{ "margin-bottom": "12px" }}>
      <div>
        <h2>{key().name}</h2>
        <p>{key().models?.length ? key().models!.join(", ") : "Serves every model"} · weight {key().weight}</p>
      </div>
      <span class="pill" classList={{ success: key().health === "healthy", danger: tone() === "danger", warning: tone() === "warning" }}>
        {key().health}
      </span>
    </div>

    <Show when={key().health === "benched" && key().benched_until_ms}>
      <p class="notice">Out of quota until {new Date(key().benched_until_ms!).toLocaleString()}. Requests route to other keys.</p>
    </Show>

    <Show when={props.subscription} fallback={<MeteredLimits providerKey={key()} />}>
      <Show when={key().quota} fallback={<p class="muted">No quota reported yet — the provider states a seat's windows on its responses, so this fills in after the first request.</p>}>
        <dl class="facts">
          <QuotaRow label="Primary window" window={key().quota!.primary} />
          <QuotaRow label="Secondary window" window={key().quota!.secondary} />
        </dl>
        <p class="muted">As of {new Date(key().quota!.observed_ms).toLocaleTimeString()}</p>
      </Show>
      <Show when={key().credential}>
        <dl class="facts">
          <Fact label="Credential expires"
                value={key().credential!.expires_at_ms
                  ? new Date(key().credential!.expires_at_ms!).toLocaleString()
                  : "Unknown — opaque token"} />
          <Fact label="Can self-renew" value={key().credential!.can_refresh ? "Yes" : "No — needs an operator"} />
        </dl>
      </Show>
    </Show>
  </div>;
}

function MeteredLimits(props: { providerKey: ProviderKey }) {
  const limits = () => props.providerKey.limits;
  return <Show when={limits().rpm || limits().tpm}
               fallback={<p class="muted">No per-key rate limit configured. Set <code>rpm</code> or <code>tpm</code> on this key to cap it independently of the others.</p>}>
    <dl class="facts">
      <Show when={limits().rpm}><Fact label="Requests remaining this minute" value={formatNumber(limits().rpm!.remaining ?? 0)} /></Show>
      <Show when={limits().tpm}><Fact label="Tokens remaining this minute" value={formatNumber(limits().tpm!.remaining ?? 0)} /></Show>
    </dl>
  </Show>;
}

/// A plan window. Utilization is shown as a bar because the number that
/// matters is "how close to full", and the reset time because that is
/// the only thing an operator can actually plan around.
function QuotaRow(props: { label: string; window: QuotaWindow | null }) {
  return <Show when={props.window} keyed>{(w) => {
    const pct = Math.round(Math.min(w.utilization, 1) * 100);
    const tone = w.rejected || pct >= 100 ? "danger" : pct >= 80 ? "warning" : "";
    return <div>
      <dt>{props.label}{w.length_s ? ` · ${formatDuration(w.length_s)}` : ""}</dt>
      <dd>
        <div class="meter-row">
          <div class={`meter ${tone}`}><i style={{ width: `${pct}%` }} /></div>
          <span>{pct}%</span>
        </div>
        <small class="muted">{w.rejected ? "Refusing requests" : "Serving"}{w.resets_in_s ? ` · resets in ${formatDuration(w.resets_in_s)}` : ""}</small>
      </dd>
    </div>;
  }}</Show>;
}

function Routing(props: { refresh: () => number }) {
  const [config, { refetch }] = createResource(props.refresh, api.config);
  const [text, setText] = createSignal("");
  const [message, setMessage] = createSignal("");
  const [error, setError] = createSignal("");
  createEffect(() => { if (config()) setText(config()!.text); });
  return <section class="editor-layout">
    <div class="editor-toolbar"><div><h2>Routing configuration</h2><p>Validated and applied atomically across the local data plane.</p></div>
      <button class="button primary" disabled={config()?.read_only} onClick={async () => {
        setError(""); setMessage("");
        try { await api.saveConfig(config()!.version, text()); setMessage("Configuration applied"); await refetch(); }
        catch (err) { setError(err instanceof Error ? err.message : "Save failed"); }
      }}><Save size={16} />Apply</button>
    </div>
    <Show when={config()?.read_only}><div class="notice">File mode is read-only. Edit the source file and reload the gateway.</div></Show>
    <Show when={message()}><p class="success-message" role="status">{message()}</p></Show><Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
    <label class="code-editor"><span>caret-router.toml</span><textarea spellcheck={false} value={text()} onInput={(e) => setText(e.currentTarget.value)} readOnly={config()?.read_only} /></label>
  </section>;
}

function Keys(props: { refresh: () => number; bump: () => void }) {
  const [keys, { refetch }] = createResource(props.refresh, api.keys);
  const [creating, setCreating] = createSignal(false);
  const [revealed, setRevealed] = createSignal("");
  const [name, setName] = createSignal("");
  const [models, setModels] = createSignal("");
  const [error, setError] = createSignal("");
  const reload = async () => { await refetch(); props.bump(); };
  return <div class="stack-lg">
    <section class="panel"><SectionTitle title="Virtual keys" subtitle="Scoped credentials with limits, budgets, and immediate revocation" action={<button class="button primary" onClick={() => setCreating(true)}><Plus size={16} />Create key</button>} />
      <Show when={(keys()?.data.length ?? 0) > 0} fallback={<Empty title="No virtual keys" action="Create a key for an application or team." />}>
        <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table"><table><thead><tr><th>Name</th><th>Scope</th><th>Rate</th><th>Budget</th><th>Status</th><th><span class="sr-only">Actions</span></th></tr></thead><tbody>
          <For each={keys()?.data ?? []}>{(key) => <KeyRow key={key} reload={reload} reveal={setRevealed} />}</For>
        </tbody></table></div>
      </Show>
    </section>
    <Show when={creating()}><div class="dialog-backdrop" role="presentation" onMouseDown={(e) => { if (e.target === e.currentTarget) setCreating(false); }}><form class="dialog" role="dialog" aria-modal="true" aria-labelledby="create-key-title" onSubmit={async (e) => {
      e.preventDefault(); setError("");
      try { const result = await api.createKey({ name: name(), models: models().split(",").map((v) => v.trim()).filter(Boolean) }); setRevealed(result.key); setCreating(false); setName(""); setModels(""); await reload(); }
      catch (err) { setError(err instanceof Error ? err.message : "Create failed"); }
    }}><h2 id="create-key-title">Create virtual key</h2><p class="muted">The secret is shown once after creation.</p>
      <label>Name<input required value={name()} onInput={(e) => setName(e.currentTarget.value)} /></label><label>Allowed models <span class="optional">Optional</span><input value={models()} placeholder="fast, openai/gpt-4.1-mini" onInput={(e) => setModels(e.currentTarget.value)} /></label>
      <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show><div class="dialog-actions"><button type="button" class="button" onClick={() => setCreating(false)}>Cancel</button><button class="button primary">Create key</button></div></form></div></Show>
    <Show when={revealed()}><div class="secret-banner" role="status"><div><strong>Copy this key now</strong><code>{revealed()}</code></div><button class="icon-button" aria-label="Copy new virtual key" title="Copy key" onClick={() => navigator.clipboard.writeText(revealed())}><Copy size={17} /></button></div></Show>
  </div>;
}

function KeyRow(props: { key: VirtualKey; reload: () => Promise<void>; reveal: (value: string) => void }) {
  return <tr><td><strong>{props.key.name}</strong><small class="mono">{props.key.id}</small></td><td>{props.key.models.length ? props.key.models.join(", ") : "All models"}</td><td>{props.key.rate?.rpm ? `${props.key.rate.rpm} RPM` : "Unlimited"}</td><td>{props.key.budget ? `${formatUsd(props.key.budget.usd)} / ${props.key.budget.period}` : "None"}</td><td><Status text={props.key.enabled ? "Active" : "Revoked"} tone={props.key.enabled ? "success" : "muted"} /></td><td class="actions"><button class="icon-button" title={`Rotate ${props.key.name}`} aria-label={`Rotate ${props.key.name}`} onClick={async () => { const result = await api.rotateKey(props.key.id); props.reveal(result.key); await props.reload(); }}><RefreshCw size={16} /></button><button class="icon-button danger" title={`Delete ${props.key.name}`} aria-label={`Delete ${props.key.name}`} onClick={async () => { if (confirm(`Delete ${props.key.name}?`)) { await api.deleteKey(props.key.id); await props.reload(); } }}><Trash2 size={16} /></button></td></tr>;
}

const RAMPS = [
  { label: "1h", seconds: 3600, days: 1 },
  { label: "24h", seconds: 86400, days: 1 },
  { label: "7d", seconds: 86400, days: 7 },
  { label: "30d", seconds: 86400, days: 30 },
] as const;

function Usage(props: { refresh: () => number }) {
  const [ramp, setRamp] = createSignal(1);
  const selected = () => RAMPS[ramp()];
  // Anything past 24h has to come from the flushed files: the in-memory
  // aggregate only spans a day.
  const long = () => selected().days > 1;
  const [live] = createResource(
    () => [props.refresh(), ramp()] as const,
    () => api.usage(selected().seconds, "model"),
  );
  const [history] = createResource(
    () => [props.refresh(), ramp()] as const,
    () => api.history(selected().days, "model"),
  );
  const [spendBy, setSpendBy] = createSignal<"model" | "provider" | "key">("model");
  const [breakdown] = createResource(
    () => [props.refresh(), ramp(), spendBy()] as const,
    () => api.history(selected().days, spendBy()),
  );

  const rows = createMemo<ModelRow[]>(() =>
    long() ? rowsFromHistory(history()?.data ?? {}) : rowsFromGroups(live()?.groups ?? []));
  const totals = createMemo(() => rows().reduce((acc, row) => ({
    requests: acc.requests + row.requests,
    failed: acc.failed + row.failed,
    tokens: acc.tokens + row.tokens,
    cost: acc.cost + row.cost,
  }), { requests: 0, failed: 0, tokens: 0, cost: 0 }));

  return <div class="stack-lg">
    <div class="view-toolbar">
      <p class="muted">Volume, failures and spend per model.</p>
      <div class="segmented" role="group" aria-label="Time range">
        <For each={RAMPS}>{(item, index) => (
          <button aria-pressed={ramp() === index()} onClick={() => setRamp(index())}>{item.label}</button>
        )}</For>
      </div>
    </div>

    <section class="summary-strip six" aria-label="Totals">
      <Metric label="Total requests" value={formatNumber(totals().requests)} />
      <Metric label="Successful" value={formatNumber(totals().requests - totals().failed)} />
      <Metric label="Failed" value={formatNumber(totals().failed)} tone={totals().failed > 0 ? "danger" : "default"} />
      <Metric label="Total tokens" value={formatNumber(totals().tokens)} />
      <Metric label="Total spend" value={formatUsd(totals().cost)} />
      <Metric label="Avg per request"
              value={formatUsd(totals().requests ? totals().cost / totals().requests : 0)} />
    </section>

    <section class="panel chart-panel">
      <SectionTitle title="Spend over time" subtitle={long() ? "Daily totals from flushed usage" : "Requests per minute"} />
      <Show when={long()} fallback={<UsageChart groups={live()?.groups ?? []} />}>
        <DailySpendChart series={history()?.data ?? {}} />
      </Show>
    </section>

    <section class="panel">
      <SectionTitle title="Per model" subtitle="Every model this gateway served in the window" />
      <Show when={rows().length} fallback={<Empty title="No traffic in this window" action="Send a request through the gateway to see it here." />}>
        <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table"><table>
          <thead><tr>
            <th>Model</th><th class="num">Requests</th><th class="num">Successful</th><th class="num">Failed</th>
            <th class="num">Tokens</th><th class="num">Total cost</th><th class="num">Avg / request</th>
          </tr></thead>
          <tbody><For each={rows()}>{(row) => <tr>
            <td class="strong">{row.model}</td>
            <td class="num">{formatNumber(row.requests)}</td>
            <td class="num">{formatNumber(row.requests - row.failed)}</td>
            <td class="num">{row.failed ? <span class="status danger"><span />{formatNumber(row.failed)}</span> : "0"}</td>
            <td class="num">{formatNumber(row.tokens)}</td>
            <td class="num">{formatUsd(row.cost)}</td>
            <td class="num">{formatUsd(row.requests ? row.cost / row.requests : 0)}</td>
          </tr>}</For></tbody>
        </table></div>
      </Show>
    </section>

    <section class="panel">
      <SectionTitle title="Where the money goes"
                    subtitle="Spend split by the dimension you pick"
                    action={<div class="segmented" role="group" aria-label="Break spend down by">
                      <For each={["model", "provider", "key"] as const}>{(dimension) => (
                        <button aria-pressed={spendBy() === dimension} onClick={() => setSpendBy(dimension)}>
                          {dimension === "key" ? "Virtual key" : dimension}
                        </button>
                      )}</For>
                    </div>} />
      <SpendBars series={breakdown()?.data ?? {}} />
    </section>
  </div>;
}

type ModelRow = { model: string; requests: number; failed: number; tokens: number; cost: number };

function rowsFromGroups(groups: any[]): ModelRow[] {
  return groups
    .map((group) => ({
      model: group.group,
      requests: group.totals.requests ?? 0,
      failed: group.totals.errors ?? 0,
      tokens: (group.totals.input_tokens ?? 0) + (group.totals.output_tokens ?? 0),
      cost: group.totals.cost_usd ?? 0,
    }))
    .sort((a, b) => b.cost - a.cost || b.requests - a.requests);
}

function rowsFromHistory(series: Record<string, DayBucket[]>): ModelRow[] {
  return Object.entries(series)
    .map(([model, days]) => ({
      model,
      requests: days.reduce((n, d) => n + d.requests, 0),
      failed: days.reduce((n, d) => n + d.failed, 0),
      tokens: days.reduce((n, d) => n + d.input_tokens + d.output_tokens, 0),
      cost: days.reduce((n, d) => n + d.cost_micro_usd, 0) / 1e6,
    }))
    .sort((a, b) => b.cost - a.cost || b.requests - a.requests);
}

/// Horizontal bars rather than a pie: shares are read by comparing
/// lengths against a shared baseline, which a pie makes hard and a bar
/// makes trivial. Sorted descending so the answer is the first row.
function SpendBars(props: { series: Record<string, DayBucket[]> }) {
  const rows = createMemo(() => {
    const totals = Object.entries(props.series)
      .map(([name, days]) => ({ name, cost: days.reduce((n, d) => n + d.cost_micro_usd, 0) / 1e6 }))
      .filter((row) => row.cost > 0)
      .sort((a, b) => b.cost - a.cost);
    const max = totals[0]?.cost ?? 0;
    return totals.map((row) => ({ ...row, share: max ? row.cost / max : 0 }));
  });
  return <Show when={rows().length} fallback={<Empty title="No spend recorded" action="Costs appear once priced models serve traffic." />}>
    <dl class="facts">
      <For each={rows()}>{(row) => <div>
        <dt>{row.name}</dt>
        <dd><div class="meter-row">
          <div class="meter" style={{ "min-width": "160px" }}><i style={{ width: `${Math.round(row.share * 100)}%` }} /></div>
          <span>{formatUsd(row.cost)}</span>
        </div></dd>
      </div>}</For>
    </dl>
  </Show>;
}

/// Requests per minute. Hardcoded greens replaced with theme tokens, so
/// this follows light/dark like everything else, and the legend is off:
/// a single unlabelled series does not need one, and uPlot's floats in
/// the middle of an empty plot when there is nothing to draw.
function UsageChart(props: { groups: any[] }) {
  let element!: HTMLDivElement;
  let plot: uPlot | undefined;
  const points = createMemo(() => {
    const map = new Map<number, number>();
    for (const group of props.groups) {
      for (const item of group.series ?? []) {
        map.set(item.minute_ts / 1000, (map.get(item.minute_ts / 1000) ?? 0) + item.requests);
      }
    }
    return map;
  });
  createEffect(() => {
    const map = points();
    plot?.destroy();
    plot = undefined;
    if (!map.size || !element) return;
    const xs = [...map.keys()].sort((a, b) => a - b);
    const ys = xs.map((x) => map.get(x) ?? 0);
    const ink = getComputedStyle(document.documentElement);
    const accent = ink.getPropertyValue("--series-1").trim();
    plot = new uPlot({
      width: Math.max(element.clientWidth, 300),
      height: 240,
      legend: { show: false },
      cursor: { drag: { x: false, y: false } },
      scales: { x: { time: true } },
      series: [
        {},
        {
          label: "Requests",
          stroke: accent,
          width: 2,
          fill: `color-mix(in srgb, ${accent} 14%, transparent)`,
        },
      ],
      axes: [
        { stroke: ink.getPropertyValue("--muted"), grid: { stroke: ink.getPropertyValue("--grid") } },
        { stroke: ink.getPropertyValue("--muted"), grid: { stroke: ink.getPropertyValue("--grid") } },
      ],
    }, [xs, ys], element);
  });
  onCleanup(() => plot?.destroy());
  return <Show when={points().size}
               fallback={<Empty title="No requests in this window" action="Traffic appears here within a minute of arriving." />}>
    <div ref={element} class="chart" aria-label="Requests over time" />
  </Show>;
}

function Requests(props: { refresh: () => number }) {
  const [errorsOnly, setErrorsOnly] = createSignal(false);
  const [filter, setFilter] = createSignal("");
  const [records] = createResource(() => [props.refresh(), errorsOnly()] as const, ([, errors]) => api.requests(errors));
  const shown = createMemo(() => {
    const needle = filter().trim().toLowerCase();
    const all = records()?.data ?? [];
    if (!needle) return all;
    return all.filter((record) =>
      `${record.requested} ${record.provider}/${record.model} ${record.vkey ?? ""} ${record.status}`
        .toLowerCase()
        .includes(needle),
    );
  });
  return <section class="panel">
    <SectionTitle
      title="Request log"
      subtitle="Metadata only; prompts and outputs are never stored"
      action={<>
        <label class="sr-only" for="request-filter">Filter requests</label>
        <input id="request-filter" class="filter-input" data-filter placeholder="Filter (press /)" value={filter()} onInput={(e) => setFilter(e.currentTarget.value)} />
        <label class="toggle"><input type="checkbox" checked={errorsOnly()} onChange={(e) => setErrorsOnly(e.currentTarget.checked)} /><span />Errors only</label>
      </>}
    />
    <RequestRows records={shown()} />
  </section>;
}

function RequestRows(props: { records: UsageRecord[]; compact?: boolean }) {
  return <Show when={props.records.length} fallback={<Empty title="No requests yet" action="Send a request through the gateway." />}><div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table"><table><thead><tr><th>Time</th><th>Route</th><Show when={!props.compact}><th>Tokens</th><th>Cost</th></Show><th>Latency</th><th>Status</th></tr></thead><tbody><For each={props.records}>{(record) => <tr><td class="mono">{new Date(record.ts).toLocaleTimeString()}</td><td><strong>{record.requested}</strong><small>{record.provider}/{record.model}</small></td><Show when={!props.compact}><td>{formatNumber(record.input_tokens + record.output_tokens)}</td><td>{formatUsd(record.cost_micro_usd / 1e6)}</td></Show><td>{record.latency_ms} ms</td><td><Status text={String(record.status)} tone={record.status < 400 ? "success" : "danger"} /></td></tr>}</For></tbody></table></div></Show>;
}

function Playground() {
  const [model, setModel] = createSignal("openai/gpt-4.1-mini"); const [key, setKey] = createSignal(""); const [prompt, setPrompt] = createSignal("Reply with one short sentence."); const [output, setOutput] = createSignal(""); const [meta, setMeta] = createSignal(""); const [pending, setPending] = createSignal(false);
  return <div class="playground"><section class="playground-input"><h2>Test a route</h2><label>Model<input value={model()} onInput={(e) => setModel(e.currentTarget.value)} /></label><label>Virtual key<input type="password" value={key()} onInput={(e) => setKey(e.currentTarget.value)} /></label><label>Prompt<textarea value={prompt()} onInput={(e) => setPrompt(e.currentTarget.value)} /></label><button class="button primary" disabled={pending() || !key()} onClick={async () => {
    setPending(true); setOutput(""); setMeta(""); const started = performance.now();
    try { const response = await fetch("/v1/chat/completions", { method: "POST", headers: { "content-type": "application/json", authorization: `Bearer ${key()}` }, body: JSON.stringify({ model: model(), messages: [{ role: "user", content: prompt() }] }) }); const body = await response.json(); if (!response.ok) throw new Error(body?.error?.message ?? `HTTP ${response.status}`); setOutput(body.choices?.[0]?.message?.content ?? JSON.stringify(body, null, 2)); setMeta(`${Math.round(performance.now() - started)} ms · ${response.headers.get("x-caret-provider") ?? "provider"} · ${response.headers.get("x-caret-overhead-us") ?? "0"} µs gateway`); }
    catch (err) { setOutput(err instanceof Error ? err.message : "Request failed"); }
    finally { setPending(false); }
  }}><Play size={16} />{pending() ? "Running…" : "Run"}</button></section><section class="playground-output" aria-live="polite"><div><p class="eyebrow">Response</p><span>{meta()}</span></div><pre>{output() || "The model response will appear here."}</pre></section></div>;
}

function Fleet(props: { refresh: () => number }) {
  const [fleet] = createResource(props.refresh, api.fleet);
  const nodes = createMemo(() => fleet()?.nodes ?? []);
  const shared = createMemo(() => nodes().length > 0);
  const age = (ms: number) => ms < 1000 ? "just now" : Math.round(ms / 1000) + "s ago";
  return <div class="stack-lg">
    <section class="summary-strip" aria-label="Fleet summary">
      <Metric label="Live nodes" value={String(fleet()?.live ?? 1)} />
      <Metric label="Rate-limit shares" value={String(fleet()?.shares ?? 1)} />
      <Metric label="Store version" value={String(fleet()?.version ?? 0)} />
      <Metric
        label="Store"
        value={fleet()?.reachable === false ? "Unreachable" : "Reachable"}
        tone={fleet()?.reachable === false ? "danger" : "default"}
      />
    </section>

    <Show when={fleet() && fleet()!.reachable === false}>
      <div class="notice" role="status">
        This node cannot reach the control-plane store. It keeps serving traffic from the
        configuration it last loaded, and refuses configuration changes until the store
        is back. Nothing needs to be done to the node itself.
      </div>
    </Show>

    <section class="panel">
      <SectionTitle
        title="Nodes"
        subtitle="Every node is identical and serves this page; the view is the same from any of them"
      />
      <dl class="facts">
        <Fact label="Backend" value={fleet()?.backend ?? "—"} />
        <Fact label="This node" value={fleet()?.node ?? "local"} />
      </dl>
      <Show
        when={shared()}
        fallback={<Empty
          title="No shared store"
          action="This node keeps its configuration locally. Point several nodes at the same S3 bucket or DynamoDB table to run a fleet."
        />}
      >
        <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table">
          <table>
            <thead><tr><th>Node</th><th>Address</th><th>Last heartbeat</th></tr></thead>
            <tbody>
              <For each={nodes()}>{(node: any) => <tr>
                <td>
                  <strong>{String(node.id).slice(0, 13)}…</strong>
                  <Show when={node.self}><small>this node</small></Show>
                </td>
                <td class="mono">{node.addr || "—"}</td>
                <td>{age(node.age_ms ?? 0)}</td>
              </tr>}</For>
            </tbody>
          </table>
        </div>
      </Show>
      <p class="hint">
        Nodes appear here by writing a heartbeat to the store and disappear when they stop.
        There is nothing to join and nothing to remove — scale the service and the fleet
        follows. Rate limits are divided by the number of live nodes.
      </p>
    </section>
  </div>;
}

function Settings(props: { refresh: () => number }) {
  const [config] = createResource(props.refresh, api.config);
  const [fleet] = createResource(props.refresh, api.fleet);
  const [theme, setTheme] = createSignal(localStorage.getItem("caret-theme") ?? "system");
  createEffect(() => {
    const choice = theme();
    localStorage.setItem("caret-theme", choice);
    document.documentElement.dataset.theme = choice === "system" ? "" : choice;
  });
  const setting = (name: string) => {
    const match = (config()?.text ?? "").match(new RegExp(`^\\s*${name}\\s*=\\s*(\\S+)`, "m"));
    return match?.[1]?.replace(/"/g, "") ?? "default";
  };
  const download = () => {
    const blob = new Blob([config()?.text ?? ""], { type: "text/plain" });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = "caret-router.toml";
    anchor.click();
    URL.revokeObjectURL(url);
  };
  return <div class="stack-lg">
    <section class="panel">
      <SectionTitle
        title="Configuration"
        subtitle="Where this gateway's source of truth lives"
        action={<button class="button" onClick={download} disabled={!config()?.text}><Save size={16} />Export TOML</button>}
      />
      <dl class="facts">
        <Fact label="Config mode" value={config()?.mode ?? "—"} />
        <Fact label="Writes" value={config()?.read_only ? "Read-only (file-managed)" : "Enabled"} />
        <Fact label="Store version" value={String(config()?.version ?? 0)} />
        <Fact label="Node" value={fleet()?.node ?? "local"} />
      </dl>
      <Show when={config()?.read_only}>
        <div class="notice">
          Configuration is file-managed on this node. Edit the file your deploy tool
          distributes, then reload — the console will not write over it.
        </div>
      </Show>
    </section>
    <section class="panel">
      <SectionTitle title="Usage retention" subtitle="Local partitions are pruned by the gateway itself" />
      <dl class="facts">
        <Fact label="Retention" value={`${setting("retention_days")} days`} />
        <Fact label="Flush interval" value={`${setting("flush_interval_secs")} s`} />
        <Fact label="Per-key metrics" value={setting("per_key_metrics") === "true" ? "On" : "Off"} />
      </dl>
      <p class="muted">
        Change these under <code>[usage]</code> in the configuration document.
      </p>
    </section>
    <section class="panel">
      <SectionTitle title="Admin access" subtitle="Separate credentials from data-plane keys" />
      <dl class="facts">
        <Fact label="Admin keys" value="Configured in [console] admin_keys" />
        <Fact label="Session" value="Short-lived token, cleared on sign out" />
      </dl>
      <p class="muted">
        Secrets are write-only here: the console submits values but reads back only
        <code>env.*</code> and <code>store.*</code> references.
      </p>
    </section>
    <section class="panel">
      <SectionTitle title="Appearance" subtitle="Follows your system by default" />
      <label>Theme
        <select value={theme()} onChange={(event) => setTheme(event.currentTarget.value)}>
          <option value="system">Match system</option>
          <option value="light">Light</option>
          <option value="dark">Dark</option>
        </select>
      </label>
      <p class="muted">Shortcuts: <kbd>g</kbd> then <kbd>o</kbd>, <kbd>k</kbd>, <kbd>u</kbd>… jumps between pages; <kbd>/</kbd> focuses a filter.</p>
    </section>
  </div>;
}

function Metric(props: { label: string; value: string; tone?: "default" | "danger" }) { return <div classList={{ metric: true, danger: props.tone === "danger" }}><span>{props.label}</span><strong>{props.value ?? "0"}</strong></div>; }
function SectionTitle(props: { title: string; subtitle: string; action?: any }) { return <div class="section-title"><div><h2>{props.title}</h2><p>{props.subtitle}</p></div>{props.action}</div>; }
function Fact(props: { label: string; value: string }) { return <div><dt>{props.label}</dt><dd>{props.value}</dd></div>; }
function Status(props: { text: string; tone: "success" | "danger" | "muted" }) { return <span class={`status ${props.tone}`}><span />{props.text}</span>; }
function Empty(props: { title: string; action: string }) { return <div class="empty"><strong>{props.title}</strong><p>{props.action}</p></div>; }
/// Seconds as the coarsest unit that still says something useful. A
/// weekly quota window reported as "604800s" tells an operator nothing;
/// "7d" tells them which plan window they are looking at.
function formatDuration(seconds: number): string {
  if (seconds < 60) return `${Math.round(seconds)}s`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`;
  if (seconds < 86400) return `${(seconds / 3600).toFixed(seconds < 36000 ? 1 : 0)}h`;
  return `${(seconds / 86400).toFixed(seconds < 864000 ? 1 : 0)}d`;
}

/// Daily spend, one line per series. Bars would imply the days are
/// independent buckets to compare; a line reads as a trend, which is
/// what a spend chart is for.
function DailySpendChart(props: { series: Record<string, DayBucket[]> }) {
  let element!: HTMLDivElement;
  let plot: uPlot | undefined;
  const palette = ["--series-1", "--series-2", "--series-3", "--series-4", "--series-5", "--series-6"];
  createEffect(() => {
    const names = Object.keys(props.series).slice(0, 6);
    const days = [...new Set(Object.values(props.series).flat().map((d) => d.day))].sort();
    const x = days.map((day) => Date.parse(`${day}T00:00:00Z`) / 1000);
    const columns = names.map((name) => {
      const byDay = new Map(props.series[name].map((d) => [d.day, d.cost_micro_usd / 1e6]));
      return days.map((day) => byDay.get(day) ?? 0);
    });
    const ink = getComputedStyle(document.documentElement);
    plot?.destroy();
    plot = undefined;
    if (!days.length || !element) return;
    plot = new uPlot({
      width: element.clientWidth || 720,
      height: 240,
      padding: [12, 8, 0, 0],
      legend: { show: false },
      axes: [
        { stroke: ink.getPropertyValue("--muted"), grid: { stroke: ink.getPropertyValue("--grid") } },
        {
          stroke: ink.getPropertyValue("--muted"),
          grid: { stroke: ink.getPropertyValue("--grid") },
          values: (_u, splits) => splits.map((v) => `$${v.toFixed(2)}`),
        },
      ],
      series: [
        {},
        ...names.map((name, index) => ({
          label: name,
          stroke: ink.getPropertyValue(palette[index % palette.length]),
          width: 2,
          points: { show: days.length < 20 },
        })),
      ],
    }, [x, ...columns], element);
    onCleanup(() => plot?.destroy());
  });
  return <>
    <div class="chart" ref={element} />
    <div class="legend">
      <For each={Object.keys(props.series).slice(0, 6)}>{(name, index) => (
        <div><i style={{ background: `var(${palette[index() % palette.length]})` }} />{name}</div>
      )}</For>
    </div>
  </>;
}

/// Tokens, requests and cost over time for each model — the same three
/// questions the Usage page answers in aggregate, split per model so a
/// single model's change is visible rather than averaged away.
function ModelActivity(props: { refresh: () => number }) {
  const [days, setDays] = createSignal(7);
  const [history] = createResource(
    () => [props.refresh(), days()] as const,
    () => api.history(days(), "model"),
  );
  const [measure, setMeasure] = createSignal<"cost" | "requests" | "tokens">("cost");
  const shaped = createMemo(() => {
    const source = history()?.data ?? {};
    const out: Record<string, DayBucket[]> = {};
    for (const [model, buckets] of Object.entries(source)) {
      out[model] = buckets.map((bucket) => ({
        ...bucket,
        // The chart plots cost, so the selected measure is projected onto
        // that field rather than duplicating the whole chart component.
        cost_micro_usd:
          measure() === "cost" ? bucket.cost_micro_usd
          : measure() === "requests" ? bucket.requests * 1e6
          : (bucket.input_tokens + bucket.output_tokens) * 1e6,
      }));
    }
    return out;
  });
  return <div class="stack-lg">
    <div class="view-toolbar">
      <p class="muted">Per-model trend over time.</p>
      <div class="toolbar-controls">
        <div class="segmented" role="group" aria-label="Measure">
          <For each={["cost", "requests", "tokens"] as const}>{(item) => (
            <button aria-pressed={measure() === item} onClick={() => setMeasure(item)}>{item}</button>
          )}</For>
        </div>
        <div class="segmented" role="group" aria-label="Days">
          <For each={[7, 30, 90]}>{(item) => (
            <button aria-pressed={days() === item} onClick={() => setDays(item)}>{item}d</button>
          )}</For>
        </div>
      </div>
    </div>
    <section class="panel chart-panel">
      <SectionTitle title={measure() === "cost" ? "Cost per day" : `${measure()} per day`}
                    subtitle="One line per model, highest six by volume" />
      <Show when={Object.keys(shaped()).length}
            fallback={<Empty title="No history yet" action="Usage is written to disk periodically; check back after some traffic." />}>
        <DailySpendChart series={shaped()} />
      </Show>
    </section>
    <section class="panel">
      <SectionTitle title="Totals over the window" subtitle="Sorted by spend" />
      <div class="table-wrap"><table>
        <thead><tr><th>Model</th><th class="num">Requests</th><th class="num">Tokens</th><th class="num">Cost</th></tr></thead>
        <tbody><For each={rowsFromHistory(history()?.data ?? {})}>{(row) => <tr>
          <td class="strong">{row.model}</td>
          <td class="num">{formatNumber(row.requests)}</td>
          <td class="num">{formatNumber(row.tokens)}</td>
          <td class="num">{formatUsd(row.cost)}</td>
        </tr>}</For></tbody>
      </table></div>
    </section>
  </div>;
}

/// Every model the gateway will route, grouped by the provider that
/// serves it. This is the resolved catalog — what `/v1/models` returns to
/// an SDK — rather than a wish list, so what is shown here is exactly
/// what a caller can ask for.
function Models(props: { refresh: () => number }) {
  const [providers] = createResource(props.refresh, api.providers);
  const [config] = createResource(props.refresh, api.config);
  const [filter, setFilter] = createSignal("");
  const aliases = createMemo(() => parseAliases(config()?.text ?? ""));
  const rows = createMemo(() => {
    const out: Array<{ model: string; provider: string; kind: string; alias?: string }> = [];
    for (const provider of providers()?.data ?? []) {
      const models = new Set<string>();
      for (const key of provider.keys) for (const model of key.models ?? []) models.add(model);
      if (!models.size) out.push({ model: "(any model this provider serves)", provider: provider.name, kind: provider.kind });
      for (const model of models) {
        const alias = aliases().find((a) => a.target === `${provider.name}/${model}`)?.name;
        out.push({ model, provider: provider.name, kind: provider.kind, alias });
      }
    }
    const needle = filter().toLowerCase();
    return out.filter((row) => !needle || row.model.toLowerCase().includes(needle) || row.provider.includes(needle));
  });
  return <div class="stack-lg">
    <div class="view-toolbar">
      <p class="muted">What callers can ask for, and which provider answers.</p>
      <input class="filter-input" data-filter placeholder="Filter models" value={filter()}
             onInput={(e) => setFilter(e.currentTarget.value)} aria-label="Filter models" />
    </div>
    <section class="panel">
      <Show when={rows().length} fallback={<Empty title="No models resolved" action="A key with no `models` list serves everything its provider offers." />}>
        <div class="table-wrap"><table>
          <thead><tr><th>Model</th><th>Provider</th><th>Kind</th><th>Alias</th></tr></thead>
          <tbody><For each={rows()}>{(row) => <tr>
            <td class="strong mono">{row.model}</td>
            <td>{row.provider}</td>
            <td><span class="pill">{row.kind}</span></td>
            <td>{row.alias ? <span class="pill accent">{row.alias}</span> : <span class="muted">—</span>}</td>
          </tr>}</For></tbody>
        </table></div>
      </Show>
    </section>
    <p class="muted">
      A model appears here when a provider key lists it, or when an alias points at it. Add either in Routing.
    </p>
  </div>;
}

/// `name = "provider/model"` pairs out of the `[aliases]` table.
function parseAliases(text: string): Array<{ name: string; target: string }> {
  const out: Array<{ name: string; target: string }> = [];
  let inside = false;
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line.startsWith("[")) { inside = line === "[aliases]"; continue; }
    if (!inside || !line || line.startsWith("#")) continue;
    const match = line.match(/^"?([^"=]+?)"?\s*=\s*"([^"]+)"/);
    if (match) out.push({ name: match[1].trim(), target: match[2] });
  }
  return out;
}

function formatNumber(value: number | undefined) { return new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 }).format(value ?? 0); }
function formatUsd(value: number | undefined) { return new Intl.NumberFormat(undefined, { style: "currency", currency: "USD", minimumFractionDigits: 2, maximumFractionDigits: 4 }).format(value ?? 0); }
