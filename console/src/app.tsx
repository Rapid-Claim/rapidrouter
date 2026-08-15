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
import { api, clearSession, login, sessionToken, type UsageRecord, type VirtualKey } from "./api";

type Page = "overview" | "providers" | "routing" | "keys" | "usage" | "requests" | "playground" | "cluster" | "settings";

const navigation: Array<{ id: Page; label: string; icon: typeof CircleGauge }> = [
  { id: "overview", label: "Overview", icon: CircleGauge },
  { id: "providers", label: "Providers", icon: Server },
  { id: "routing", label: "Routing", icon: Route },
  { id: "keys", label: "Keys", icon: KeyRound },
  { id: "usage", label: "Usage", icon: ChartNoAxesCombined },
  { id: "requests", label: "Requests", icon: Activity },
  { id: "playground", label: "Playground", icon: Play },
  { id: "cluster", label: "Cluster", icon: Network },
];

/// Settings is deliberately not a first-class page: it lives in the nav
/// footer, reachable by `g ,` like every other destination.
const settingsNav = { id: "settings" as Page, label: "Settings", icon: SettingsIcon };

/// `g` then a letter jumps to a page; `/` focuses the page's filter.
const jumps: Record<string, Page> = {
  o: "overview",
  p: "providers",
  r: "routing",
  k: "keys",
  u: "usage",
  q: "requests",
  y: "playground",
  c: "cluster",
  ",": "settings",
};

function routeFromHash(): Page {
  const candidate = location.hash.slice(1) as Page;
  const known = [...navigation, settingsNav];
  return known.some((item) => item.id === candidate) ? candidate : "overview";
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
        <a class="brand" href="#overview" aria-label="Caret Router overview">
          <span class="brand-mark"><Boxes size={18} /></span>
          <span><strong>Caret</strong><small>Router</small></span>
        </a>
        <nav aria-label="Main navigation">
          <For each={navigation}>{(item) => (
            <a href={`#${item.id}`} aria-label={item.label} classList={{ active: page() === item.id }}>
              <item.icon size={17} aria-hidden="true" />
              <span>{item.label}</span>
            </a>
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
          <div><p class="eyebrow">Operations</p><h1>{current().label}</h1></div>
          <button class="icon-button" title="Refresh data" aria-label="Refresh data" onClick={() => setRefresh((value) => value + 1)}><RefreshCw size={17} /></button>
        </header>
        <div class="page-content">
          <Switch>
            <Match when={page() === "overview"}><Overview refresh={refresh} /></Match>
            <Match when={page() === "providers"}><Providers refresh={refresh} /></Match>
            <Match when={page() === "routing"}><Routing refresh={refresh} /></Match>
            <Match when={page() === "keys"}><Keys refresh={refresh} bump={() => setRefresh((v) => v + 1)} /></Match>
            <Match when={page() === "usage"}><Usage refresh={refresh} /></Match>
            <Match when={page() === "requests"}><Requests refresh={refresh} /></Match>
            <Match when={page() === "playground"}><Playground /></Match>
            <Match when={page() === "cluster"}><Cluster refresh={refresh} /></Match>
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

function Overview(props: { refresh: () => number }) {
  const [usage] = createResource(props.refresh, () => api.usage(3600));
  const [requests] = createResource(props.refresh, () => api.requests());
  const [fleet] = createResource(props.refresh, api.fleet);
  const totals = createMemo(() => usage()?.groups?.reduce((acc: any, group: any) => {
    for (const key of ["requests", "errors", "input_tokens", "output_tokens", "cost_usd"]) acc[key] = (acc[key] ?? 0) + (group.totals[key] ?? 0);
    return acc;
  }, {}) ?? {});
  return <div class="stack-lg">
    <section class="summary-strip" aria-label="Last hour summary">
      <Metric label="Requests" value={formatNumber(totals().requests)} />
      <Metric label="Tokens" value={formatNumber((totals().input_tokens ?? 0) + (totals().output_tokens ?? 0))} />
      <Metric label="Spend" value={formatUsd(totals().cost_usd)} />
      <Metric label="Errors" value={formatNumber(totals().errors)} tone={(totals().errors ?? 0) > 0 ? "danger" : "default"} />
    </section>
    <div class="two-column">
      <section class="panel"><SectionTitle title="Gateway state" subtitle="Current local control-plane status" />
        <dl class="facts"><Fact label="Mode" value={fleet()?.mode ?? "—"} /><Fact label="Store version" value={String(fleet()?.version ?? "—")} /><Fact label="Quorum" value={fleet()?.quorum ? "Available" : "Unavailable"} /></dl>
      </section>
      <section class="panel"><SectionTitle title="Recent activity" subtitle="Latest metadata-only requests" />
        <RequestRows records={(requests()?.data ?? []).slice(0, 6)} compact />
      </section>
    </div>
  </div>;
}

function Providers(props: { refresh: () => number }) {
  const [config] = createResource(props.refresh, api.config);
  const providers = createMemo(() => parseSections(config()?.text ?? "", "providers"));
  return <section class="panel"><SectionTitle title="Provider inventory" subtitle="Endpoints and credentials remain server-side" />
    <Show when={providers().length} fallback={<Empty title="No managed providers" action="Add a provider in Routing." />}>
      <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table"><table><thead><tr><th>Name</th><th>Kind</th><th>Status</th><th>Source</th></tr></thead><tbody>
        <For each={providers()}>{(provider) => <tr><td class="strong">{provider.name}</td><td>{provider.kind}</td><td><Status text="Ready" tone="success" /></td><td>{config()?.mode}</td></tr>}</For>
      </tbody></table></div>
    </Show>
  </section>;
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

function Usage(props: { refresh: () => number }) {
  const [window, setWindow] = createSignal(3600);
  const [usage] = createResource(() => [props.refresh(), window()] as const, ([, value]) => api.usage(value, "provider"));
  return <div class="stack-lg"><div class="view-toolbar"><div><h2>Spend and volume</h2><p>Provider-reported usage for the selected window.</p></div><select aria-label="Usage window" value={window()} onChange={(e) => setWindow(Number(e.currentTarget.value))}><option value="3600">Last hour</option><option value="21600">Last 6 hours</option><option value="86400">Last 24 hours</option></select></div>
    <section class="panel chart-panel"><UsageChart groups={usage()?.groups ?? []} /></section>
    <section class="panel"><div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table"><table><thead><tr><th>Provider</th><th>Requests</th><th>Errors</th><th>Tokens</th><th>Spend</th><th>Avg latency</th></tr></thead><tbody><For each={usage()?.groups ?? []}>{(group: any) => <tr><td class="strong">{group.group}</td><td>{formatNumber(group.totals.requests)}</td><td>{formatNumber(group.totals.errors)}</td><td>{formatNumber(group.totals.input_tokens + group.totals.output_tokens)}</td><td>{formatUsd(group.totals.cost_usd)}</td><td>{group.totals.avg_latency_ms} ms</td></tr>}</For></tbody></table></div></section>
  </div>;
}

function UsageChart(props: { groups: any[] }) {
  let element!: HTMLDivElement;
  let plot: uPlot | undefined;
  createEffect(() => {
    const points = new Map<number, number>();
    for (const group of props.groups) for (const item of group.series ?? []) points.set(item.minute_ts / 1000, (points.get(item.minute_ts / 1000) ?? 0) + item.requests);
    const xs = [...points.keys()].sort(); const ys = xs.map((x) => points.get(x) ?? 0);
    plot?.destroy();
    plot = new uPlot({ width: Math.max(element.clientWidth, 300), height: 240, cursor: { drag: { x: false, y: false } }, scales: { x: { time: true } }, series: [{}, { label: "Requests", stroke: "#24745b", width: 2, fill: "rgba(36,116,91,.1)" }], axes: [{ stroke: "#6d736f", grid: { stroke: "#e5e8e6" } }, { stroke: "#6d736f", grid: { stroke: "#e5e8e6" } }] }, [xs, ys], element);
  });
  onCleanup(() => plot?.destroy());
  return <div ref={element} class="chart" aria-label="Requests over time" />;
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

function Cluster(props: { refresh: () => number }) {
  const [fleet] = createResource(props.refresh, api.fleet);
  const [token, setToken] = createSignal("");
  const [error, setError] = createSignal("");
  const members = createMemo(() => fleet()?.member_list ?? []);
  const clustered = createMemo(() => members().length > 0);
  return <div class="stack-lg">
    <section class="summary-strip" aria-label="Cluster summary">
      <Metric label="Live members" value={String(fleet()?.live ?? 1)} />
      <Metric label="Voters" value={String(fleet()?.members ?? 1)} />
      <Metric label="Applied version" value={String(fleet()?.version ?? 0)} />
      <Metric label="Quorum" value={fleet()?.quorum ? "Available" : "Lost"} tone={fleet()?.quorum ? "default" : "danger"} />
    </section>

    <Show when={fleet() && !fleet()!.quorum}>
      <div class="notice" role="status">
        This node cannot reach a quorum. It keeps serving traffic from the configuration it
        last applied, and refuses configuration changes until enough members are back.
      </div>
    </Show>

    <Show when={fleet() && fleet()!.live < fleet()!.members}>
      <div class="notice" role="status">
        {fleet()!.members - fleet()!.live} of {fleet()!.members} members are not responding.
        Rate-limit shares have been redistributed across the {fleet()!.live} that are.
        A member that is never coming back should be removed.
      </div>
    </Show>

    <section class="panel">
      <SectionTitle
        title="Fleet members"
        subtitle={clustered()
          ? "Every node serves this page; the view is the same from any of them"
          : "Single node — a cluster of one. Add a node to form a cluster."}
      />
      <Show when={clustered()} fallback={<Empty title="No peers" action="Start another node with --join <this-host>:9444 and the cluster token." />}>
        <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table">
          <table>
            <thead><tr><th>Node</th><th>Address</th><th>Role</th><th>Applied</th><th>Lag</th><th /></tr></thead>
            <tbody>
              <For each={members()}>{(member: any) => <tr>
                <td><strong>{String(member.id).slice(0, 10)}…</strong><small>{member.is_self ? "this node" : member.voter ? "voter" : "learner"}</small></td>
                <td class="mono">{member.addr}</td>
                <td><Status text={member.leader ? "Leader" : "Follower"} tone={member.leader ? "success" : "muted"} /></td>
                <td>{member.applied ?? "—"}</td>
                <td>{member.lag === null || member.lag === undefined ? "—" : String(member.lag)}</td>
                <td class="actions">
                  <Show when={!member.is_self}>
                    <button class="icon-button danger" title={"Remove " + member.id} aria-label={"Remove node " + member.id}
                      onClick={async () => {
                        if (!confirm("Remove node " + member.id + " from membership?\n\nDo this only for a node that is never coming back. The remaining members must still form a quorum.")) return;
                        setError("");
                        try { await api.removeNode(member.id); }
                        catch (err) { setError(err instanceof Error ? err.message : "Remove failed"); }
                      }}><Trash2 size={16} /></button>
                  </Show>
                </td>
              </tr>}</For>
            </tbody>
          </table>
        </div>
      </Show>
      <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
    </section>

    <section class="panel">
      <SectionTitle
        title="Adding a node"
        subtitle="Same binary, one command — the join streams a snapshot and starts serving"
        action={<button class="button" onClick={async () => {
          setError("");
          try { setToken((await api.clusterToken()).token); }
          catch (err) { setError(err instanceof Error ? err.message : "Could not read the token"); }
        }}><KeyRound size={16} />Show join token</button>}
      />
      <Show when={token()} fallback={<p class="muted">The join token is a credential: anyone holding it can join this cluster. It is shown only when you ask for it.</p>}>
        <div class="secret-banner">
          <div><strong>Join token</strong><code>{token()}</code></div>
          <button class="icon-button" title="Copy" aria-label="Copy join token" onClick={() => navigator.clipboard?.writeText(token())}><Copy size={16} /></button>
        </div>
      </Show>
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
function formatNumber(value: number | undefined) { return new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 }).format(value ?? 0); }
function formatUsd(value: number | undefined) { return new Intl.NumberFormat(undefined, { style: "currency", currency: "USD", minimumFractionDigits: 2, maximumFractionDigits: 4 }).format(value ?? 0); }
function parseSections(text: string, root: string) { const regex = new RegExp(`^\\[${root}\\.([^\\]]+)\\]`, "gm"); const result: Array<{ name: string; kind: string }> = []; let match: RegExpExecArray | null; while ((match = regex.exec(text))) { const block = text.slice(match.index, text.indexOf("\n[", match.index + 1) < 0 ? undefined : text.indexOf("\n[", match.index + 1)); result.push({ name: match[1], kind: block.match(/kind\s*=\s*"([^"]+)"/)?.[1] ?? match[1] }); } return result; }
