import {
  Activity,
  ArrowDown,
  ArrowUp,
  Boxes,
  ChartNoAxesCombined,
  ChevronsUpDown,
  Coins,
  ChevronRight,
  CircleGauge,
  Copy,
  KeyRound,
  LogIn,
  LogOut,
  PanelLeft,
  Play,
  Plus,
  RefreshCw,
  Route,
  ScrollText,
  UserRound,
  Users,
  Save,
  Search,
  Server,
  Settings as SettingsIcon,
  Stethoscope,
  Trash2,
  X,
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
  on,
  onCleanup,
  onMount,
} from "solid-js";
import uPlot from "uplot";
import { JsonView, Transcript, parseAnswer, parseConversation } from "./bodies";
import logo from "./logo.svg";
import {
  Combobox,
  Loading,
  Skeleton,
  Drawer,
  FilterBar,
  MultiCombobox,
  RangePicker,
  escapeCloses,
  resolveRange,
  type Option,
  type TimeRange,
} from "./ui";
import {
  hashPath,
  setHashPath,
  urlFilters,
  urlParam,
  urlRange,
  urlSearch,
  type DimFilters,
} from "./route";
import {
  api,
  clearSession,
  login,
  sessionToken,
  type DayBucket,
  type DeviceLogin,
  type Provider,
  type ProviderKey,
  type QuotaWindow,
  type CatalogPreset,
  type InternalUser,
  type RouteGroup,
  type RouteTarget,
  type Team,
  type UsageRecord,
  type VirtualKey,
  RequestsSummary,
  UsageSlice,
  UsageSummary,
} from "./api";

type Page =
  | "users"
  | "teams"
  | "usage"
  | "cost"
  | "activity"
  | "keys"
  | "providers"
  | "models"
  | "routing"
  | "playground"
  | "logs"
  | "settings";

/// Ordered by how often an operator needs them, not by the shape of the
/// backend. Usage leads because "what is this costing" is the question
/// people arrive with; configuration sits below the things you watch.
const navigation: Array<{ id: Page; label: string; icon: typeof CircleGauge; group?: string; adminOnly?: boolean }> = [
  { id: "usage", label: "Usage", icon: ChartNoAxesCombined, group: "Observe" },
  { id: "cost", label: "Cost", icon: Coins },
  { id: "activity", label: "Model activity", icon: Activity },
  { id: "logs", label: "Logs", icon: ScrollText },
  { id: "keys", label: "Virtual keys", icon: KeyRound, group: "Configure" },
  { id: "providers", label: "Providers", icon: Server },
  { id: "models", label: "Models", icon: Boxes },
  { id: "routing", label: "Routing groups", icon: Route },
  { id: "playground", label: "Playground", icon: Play },
  { id: "users", label: "Internal users", icon: UserRound, group: "Access control", adminOnly: true },
  { id: "teams", label: "Teams", icon: Users, adminOnly: true },
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
  o: "cost",
  a: "activity",
  l: "logs",
  k: "keys",
  p: "providers",
  m: "models",
  r: "routing",
  y: "playground",
  i: "users",
  t: "teams",
  ",": "settings",
};

/// Which page the hash names, ignoring the filters hanging off it.
function routeFromHash(): Page {
  const path = hashPath();
  if (path === "cluster") return "settings";
  const known = [...navigation, settingsNav];
  return known.some((item) => item.id === path) ? (path as Page) : "usage";
}

/// Typing in a field must never be swallowed by a shortcut.
function isTyping(target: EventTarget | null): boolean {
  const el = target as HTMLElement | null;
  if (!el) return false;
  return ["INPUT", "TEXTAREA", "SELECT"].includes(el.tagName) || el.isContentEditable;
}

export /// How often live traffic may trigger a refetch.
///
/// The gateway emits an event per request, so an unthrottled console
/// refetches at the request rate — a hundred admin queries a second on a
/// busy gateway. Charts are read by humans; five seconds is under the
/// threshold where anyone notices, and it turns an unbounded cost into a
/// fixed one.
const TRAFFIC_REFRESH_MS = 5_000;

/// Rows per page in the request log.
const PAGE_SIZE = 100;

/// The state of the console's live connection to the gateway.
///
/// "connecting" is the one that was missing: a page that has not finished
/// its first handshake has not lost anything, and saying so is the
/// difference between a console that looks broken on every reload and one
/// that doesn't.
type LinkState = "connecting" | "live" | "reconnecting" | "expired";

/// How often a dropped stream may ask the gateway *why* it dropped.
const DIAGNOSE_INTERVAL_MS = 5_000;

const LINK_STATES: Record<LinkState, { label: string; tone: string; title: string }> = {
  connecting: { label: "Connecting", tone: "pending", title: "Opening the live update stream" },
  live: { label: "Live", tone: "online", title: "Streaming live updates" },
  reconnecting: { label: "Reconnecting", tone: "offline", title: "The live update stream dropped; retrying" },
  expired: { label: "Signed out", tone: "offline", title: "This session expired; sign in again" },
};

export function App() {
  const savedTheme = localStorage.getItem("rapid-theme");
  if (savedTheme === "light" || savedTheme === "dark") {
    document.documentElement.dataset.theme = savedTheme;
  }
  const [authenticated, setAuthenticated] = createSignal(Boolean(sessionToken()));
  const [me] = createResource(authenticated, (ok) => (ok ? api.me().catch(() => undefined) : undefined));
  const visibleNav = createMemo(() =>
    navigation.filter((item) => !item.adminOnly || me()?.is_admin !== false));
  // Derived, not stored: the hash is the state, and a signal alongside
  // it is a second copy to keep in step.
  const page = createMemo(routeFromHash);
  // Filters are written back to the hash, so it has to name the page
  // they belong to — an empty hash or the `cluster` alias would leave
  // `?model=…` attached to a path that resolves somewhere else on the
  // next reload.
  createEffect(() => {
    if (hashPath() !== page()) setHashPath(page());
  });
  const [refresh, setRefresh] = createSignal(0);
  // Four states, not a boolean. A boolean has to call the first moments
  // of a page load something, and calling them "Reconnecting" made every
  // single reload look like a fault — the most common thing the header
  // said was that something was wrong, when nothing was.
  const [link, setLink] = createSignal<LinkState>("connecting");
  const [collapsed, setCollapsed] = createSignal(localStorage.getItem("rapid-rail") === "collapsed");
  const toggleRail = () => {
    const next = !collapsed();
    setCollapsed(next);
    localStorage.setItem("rapid-rail", next ? "collapsed" : "expanded");
  };

  let pendingJump = false;
  const onKey = (event: KeyboardEvent) => {
    if (event.metaKey || event.ctrlKey || event.altKey || isTyping(event.target)) return;
    if (event.key === "/") {
      // A filter inside an open drawer wins: the page's own filter is
      // behind it, and focusing something invisible reads as the key
      // having done nothing at all.
      const filter = document.querySelector<HTMLElement>(".drawer [data-filter]")
        ?? document.querySelector<HTMLElement>("[data-filter]");
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
  onMount(() => addEventListener("keydown", onKey));
  createEffect(() => {
    if (!authenticated()) return;
    setLink("connecting");
    const events = new EventSource("/admin/api/events");
    // Distinguishing "the gateway is down" from "this session is no
    // longer valid" matters, because they need opposite things from the
    // operator: wait, or sign in. EventSource cannot report a status
    // code, so ask.
    // EventSource retries on its own every second, so `onerror` fires
    // about that often while a gateway is down. Probing on each one
    // would turn an outage into a request flood against the thing that
    // is already struggling.
    let probing = false;
    let probedAt = 0;
    const diagnose = async () => {
      if (probing || Date.now() - probedAt < DIAGNOSE_INTERVAL_MS) return;
      probing = true;
      probedAt = Date.now();
      try {
        await api.me();
        setLink("reconnecting");
      } catch {
        // A restarted gateway used to land here permanently: the token
        // in sessionStorage looked fine to this tab, the stream 401'd
        // forever, and the header just said "Reconnecting". Signed
        // tokens make that rare, but when the session really is gone,
        // say so and show the login screen.
        clearSession();
        setLink("expired");
        setAuthenticated(false);
      } finally {
        probing = false;
      }
    };
    events.onopen = () => setLink("live");
    events.onerror = () => {
      setLink((current) => (current === "live" ? "reconnecting" : current));
      void diagnose();
    };
    // The gateway emits an event per *request*, not just per config
    // change. Refetching everything on each one turns one busy second
    // into a hundred admin queries — the console DoSing its own gateway,
    // and every page permanently reloading.
    //
    // So: configuration changes refresh immediately (they are rare and
    // the operator is usually the one who caused them), and traffic
    // events are coalesced into one refresh every few seconds, which is
    // faster than anybody reads a chart anyway.
    let pendingTraffic = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const scheduleTraffic = () => {
      pendingTraffic = true;
      if (timer) return;
      timer = setTimeout(() => {
        timer = undefined;
        if (!pendingTraffic) return;
        pendingTraffic = false;
        setRefresh((value) => value + 1);
      }, TRAFFIC_REFRESH_MS);
    };
    events.onmessage = (event) => {
      let type = "";
      try {
        type = JSON.parse(event.data)?.type ?? "";
      } catch {
        // An unparseable event still means something happened.
      }
      // The gateway's greeting. It carries no news, so it must not
      // trigger a refetch — otherwise every reconnect would reload every
      // page on screen.
      if (type === "hello") {
        setLink("live");
        return;
      }
      if (type === "request") scheduleTraffic();
      else setRefresh((value) => value + 1);
    };
    onCleanup(() => {
      if (timer) clearTimeout(timer);
      events.close();
    });
  });
  onCleanup(() => removeEventListener("keydown", onKey));

  const current = createMemo(
    () => [...navigation, settingsNav].find((item) => item.id === page())!,
  );
  return (
    <Show when={authenticated()} fallback={<Login onSuccess={() => setAuthenticated(true)} />}>
    <div class="app-shell" classList={{ collapsed: collapsed() }}>
      <aside class="sidebar">
        <div class="brand-row">
          <a class="brand" href="#usage" aria-label="Rapid Router">
            <img class="brand-logo" src={logo} alt="" />
            <span class="brand-name"><small>Rapid</small><strong>Router</strong></span>
          </a>
        </div>
        <nav aria-label="Main navigation">
          <For each={visibleNav()}>{(item) => (
            <>
              <Show when={item.group}><p class="nav-group">{item.group}</p></Show>
              <a href={`#${item.id}`} aria-label={item.label} title={item.label} classList={{ active: page() === item.id }}>
                <item.icon size={17} aria-hidden="true" />
                <span>{item.label}</span>
              </a>
            </>
          )}</For>
        </nav>
        <div class="sidebar-foot">
          <a href="#settings" class="foot-link" aria-label="Settings" classList={{ active: page() === "settings" }} title="Settings (g ,)">
            <SettingsIcon size={16} aria-hidden="true" />
          </a>
          <span class="foot-spacer" />
          <button class="foot-link" title="Sign out" aria-label="Sign out" onClick={() => {
            clearSession();
            setAuthenticated(false);
          }}><LogOut size={16} /></button>
        </div>
      </aside>
      <main>
        <header class="page-header">
          <div class="page-header-left">
            <button
              class="rail-toggle"
              aria-label={collapsed() ? "Expand sidebar" : "Collapse sidebar"}
              aria-pressed={collapsed()}
              title={collapsed() ? "Expand sidebar" : "Collapse sidebar"}
              onClick={toggleRail}
            >
              <PanelLeft size={15} />
            </button>
            <span class="top-bar-divider" aria-hidden="true" />
            <h1>{current().label}</h1>
          </div>
          <div class="page-header-right">
            <div class="live-state" title={LINK_STATES[link()].title}>
              <span class="dot" classList={{ [LINK_STATES[link()].tone]: true }} />
              <span class="live-label">{LINK_STATES[link()].label}</span>
            </div>
            <button class="icon-button" title="Refresh data" aria-label="Refresh data" onClick={() => setRefresh((value) => value + 1)}><RefreshCw size={16} /></button>
          </div>
        </header>
        <div class="page-content" classList={{ flush: page() === "playground" }}>
          <Switch>
            <Match when={page() === "usage"}><Usage refresh={refresh} /></Match>
            <Match when={page() === "cost"}><Cost refresh={refresh} /></Match>
            <Match when={page() === "activity"}><ModelActivity refresh={refresh} /></Match>
            <Match when={page() === "logs"}><Requests refresh={refresh} /></Match>
            <Match when={page() === "keys"}><Keys refresh={refresh} bump={() => setRefresh((v) => v + 1)} /></Match>
            <Match when={page() === "providers"}><Providers refresh={refresh} /></Match>
            <Match when={page() === "models"}><Models refresh={refresh} /></Match>
            <Match when={page() === "routing"}><Routing refresh={refresh} /></Match>
            <Match when={page() === "playground"}><Playground /></Match>
            <Match when={page() === "users"}><UsersPage refresh={refresh} /></Match>
            <Match when={page() === "teams"}><TeamsPage refresh={refresh} /></Match>
            <Match when={page() === "settings"}><Settings refresh={refresh} /></Match>
          </Switch>
        </div>
      </main>
    </div>
    </Show>
  );
}

function Login(props: { onSuccess: () => void }) {
  const [mode, setMode] = createSignal<"key" | "email">("email");
  const [key, setKey] = createSignal("");
  const [email, setEmail] = createSignal("");
  const [password, setPassword] = createSignal("");
  const [error, setError] = createSignal("");
  const [pending, setPending] = createSignal(false);
  return <main class="login-shell">
    <form class="login-panel" onSubmit={async (event) => {
      event.preventDefault();
      setPending(true); setError("");
      try {
        await login(mode() === "key" ? { key: key() } : { email: email(), password: password() });
        props.onSuccess();
      } catch (err) { setError(err instanceof Error ? err.message : "Sign in failed"); }
      finally { setPending(false); }
    }}>
      <img class="brand-logo large" src={logo} alt="" />
      <div><p class="eyebrow">Rapid Router</p><h1>Sign in</h1><p class="muted">
        {mode() === "key" ? "Use the admin key configured on this gateway." : "Use the account an admin created for you."}
      </p></div>
      <Show when={mode() === "key"} fallback={<>
        <label>Email<input required type="email" autocomplete="email" value={email()} onInput={(e) => setEmail(e.currentTarget.value)} /></label>
        <label>Password<input required type="password" autocomplete="current-password" value={password()} onInput={(e) => setPassword(e.currentTarget.value)} /></label>
      </>}>
        <label>Admin key<input autofocus type="password" autocomplete="current-password" value={key()} onInput={(e) => setKey(e.currentTarget.value)} /></label>
      </Show>
      <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
      <button class="button primary" disabled={pending() || (mode() === "key" ? !key() : !email() || !password())}>
        {pending() ? "Signing in…" : "Sign in"}<ChevronRight size={16} />
      </button>
      <button type="button" class="button ghost" onClick={() => { setMode(mode() === "key" ? "email" : "key"); setError(""); }}>
        {mode() === "key" ? "Use email and password" : "Use admin key"}
      </button>
    </form>
  </main>;
}

const PROVIDER_META: Record<string, { label: string; color: string; mark?: string }> = {
  openai: { label: "OpenAI", color: "#10a37f" },
  anthropic: { label: "Anthropic", color: "#d97757" },
  gemini: { label: "Google Gemini", color: "#4285f4", mark: "G" },
  azure: { label: "Azure OpenAI", color: "#0078d4", mark: "Az" },
  bedrock: { label: "AWS Bedrock", color: "#ff9900", mark: "B" },
  vertex: { label: "Vertex AI", color: "#669df6", mark: "V" },
  databricks: { label: "Databricks", color: "#ff3621", mark: "D" },
  groq: { label: "Groq", color: "#f55036" },
  mistral: { label: "Mistral", color: "#fa520f" },
  cerebras: { label: "Cerebras", color: "#f15a29" },
  openrouter: { label: "OpenRouter", color: "#6467f2", mark: "OR" },
  ollama: { label: "Ollama", color: "#4a4a4a", mark: "Ol" },
  vllm: { label: "vLLM", color: "#3b82f6", mark: "vL" },
  openai_compat: { label: "OpenAI Compatible", color: "#64748b", mark: "{}" },
  claude_subscription: { label: "Claude Code", color: "#d97757", mark: "CC" },
  codex_subscription: { label: "Codex", color: "#10a37f", mark: "Cx" },
};

function providerMeta(name: string, kind?: string) {
  return (
    PROVIDER_META[name] ??
    PROVIDER_META[(kind ?? "").toLowerCase()] ??
    PROVIDER_META[(kind ?? "").toLowerCase().replace(/aicompat|compat/, "ai_compat")] ??
    { label: name.charAt(0).toUpperCase() + name.slice(1), color: "#78716c" }
  );
}

function ProviderMark(props: { name: string; kind?: string; size?: number }) {
  const meta = () => providerMeta(props.name, props.kind);
  return (
    <span
      class="provider-mark"
      style={{ background: meta().color, width: `${props.size ?? 22}px`, height: `${props.size ?? 22}px` }}
      aria-hidden="true"
    >
      {meta().mark ?? meta().label.slice(0, 2)}
    </span>
  );
}

function Providers(props: { refresh: () => number }) {
  const [providers, { refetch }] = createResource(props.refresh, api.providers);
  const [catalog] = createResource(props.refresh, api.catalog);
  const [selected, setSelected] = createSignal<string>("");
  // Opening a provider refetches before showing it. The page otherwise
  // only reloads when the gateway pushes an event, so a console left open
  // on a quiet router would open the drawer onto a snapshot from whenever
  // traffic last stopped — the one moment an operator most needs the
  // current state of every seat.
  const open = (name: string) => {
    setSelected(name);
    void refetch();
  };
  const [adding, setAdding] = createSignal(false);
  const [search, setSearch] = createSignal("");
  const [error, setError] = createSignal("");

  const current = createMemo(() => providers()?.data.find((p) => p.name === selected()));
  const shown = createMemo(() => {
    const needle = search().trim().toLowerCase();
    const all = providers()?.data ?? [];
    return needle ? all.filter((p) => `${p.name} ${p.kind}`.toLowerCase().includes(needle)) : all;
  });

  const [loggingIn, setLoggingIn] = createSignal<{ provider: string; key: string; email: string | null } | null>(null);

  // A check is a real request per credential, so the page says which one
  // is in flight ("*" for all) and keeps each result until the next run.
  const [checking, setChecking] = createSignal<string | null>(null);
  const [probes, setProbes] = createSignal<Record<string, { status: string; detail: string }>>({});
  const runProbe = async (providerName: string, key?: string) => {
    setChecking(key ?? "*");
    setError("");
    try {
      const result = await api.probeProvider(providerName, key ? { key } : {});
      setProbes((prev) => {
        const next = key ? { ...prev } : {};
        for (const entry of result.results) next[entry.key] = { status: entry.status, detail: entry.detail };
        return next;
      });
      await refetch();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Check failed");
    } finally {
      setChecking(null);
    }
  };

  return <div class="stack-lg">
    <FilterBar
      search={search()}
      onSearch={setSearch}
      searchPlaceholder="Search providers (press /)"
      filters={[]}
      extra={<button class="button primary" onClick={() => setAdding(true)}><Plus size={15} />Add provider</button>}
    />
    <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
    <section class="panel">
      <SectionTitle title="Providers" subtitle="Select one to inspect its credentials, limits and quota" />
      <Loading when={providers.loading && !providers()} skeleton="table"><Show when={shown().length} fallback={<Empty title="No providers configured" action="Add one to start routing traffic." />}>
        <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table"><table>
          <thead><tr><th>Provider</th><th>Type</th><th class="num">Keys</th><th>Health</th><th>Endpoint</th></tr></thead>
          <tbody><For each={shown()}>{(provider) => {
            const health = () => providerHealth(provider);
            return <tr class="clickable" onClick={() => open(provider.name)}>
              <td>
                <span class="provider-cell">
                  <ProviderMark name={provider.name} kind={provider.kind} />
                  <span>
                    <strong>{providerMeta(provider.name, provider.kind).label}</strong>
                    <Show when={providerMeta(provider.name, provider.kind).label.toLowerCase() !== provider.name}>
                      <small class="mono">{provider.name}</small>
                    </Show>
                  </span>
                </span>
              </td>
              <td><span class="pill" classList={{ accent: provider.subscription }}>{provider.subscription ? "Subscription" : provider.kind}</span></td>
              <td class="num">{provider.keys.length}</td>
              <td><Status text={health().label} tone={health().tone} /></td>
              <td class="mono" style={{ color: "var(--muted)" }}>{provider.base_url ?? "provider default"}</td>
            </tr>;
          }}</For></tbody>
        </table></div>
      </Show>
      </Loading>
    </section>

    <Drawer
      wide
      open={Boolean(current())}
      title={current() ? providerMeta(current()!.name, current()!.kind).label : ""}
      subtitle={current()?.name}
      onClose={() => setSelected("")}
      actions={<button class="button" onClick={async () => {
        if (!confirm(`Remove provider ${current()!.name}? Routes pointing at it will stop resolving.`)) return;
        setError("");
        try { await api.deleteProvider(current()!.name); setSelected(""); await refetch(); }
        catch (err) { setError(err instanceof Error ? err.message : "Delete failed"); }
      }}><Trash2 size={14} />Remove</button>}
    >
      {/* Keyed on the provider *name*, not the provider object. The page
          refetches whenever the gateway reports an event, and a block
          keyed on the object would be torn down and rebuilt on every one
          of those — taking the search box's focus and the operator's
          selection with it, mid-task. */}
      <Show when={current() ? selected() : ""} keyed>{(name) => (
        <ProviderAccounts
          provider={current()!}
          tenants={providers()?.tenants ?? []}
          probes={probes()}
          checking={checking()}
          onCheck={runProbe}
          onLogin={(key) => setLoggingIn({
            provider: name,
            key: key.name,
            email: key.credential?.email ?? null,
          })}
          onDone={refetch}
          onError={setError}
        />
      )}</Show>
    </Drawer>

    <Show when={loggingIn()} keyed>{(target) => (
      <DeviceLoginDialog
        provider={target.provider}
        credential={target.key}
        email={target.email}
        onClose={() => setLoggingIn(null)}
        // Left open on success: the operator reads "signed in as …",
        // which is the confirmation they came for. The table behind it
        // is refreshed so the row stops claiming it needs a login.
        onSignedIn={() => { void refetch(); }}
      />
    )}</Show>

    <Show when={adding()}>
      <AddProviderDialog
        catalog={catalog()}
        onClose={() => setAdding(false)}
        onDone={async () => { setAdding(false); await refetch(); }}
      />
    </Show>
  </div>;
}

/// The seats of one provider, and the tools to find one among eighty.
///
/// Three things a pool of this size needs that a short list does not: a
/// count that answers "how many of these am I looking at", a way to cut
/// the pool down, and a way to see that two rows are secretly the same
/// upstream account. The last one is not a nicety — a duplicate seat is
/// one account's quota counted twice, so a pool that looks like eighty
/// seats can be sixty-five, and nothing on an ungrouped row says so.
function ProviderAccounts(props: {
  provider: Provider;
  /// Declared services, for the per-account owner picker.
  tenants: string[];
  probes: Record<string, { status: string; detail: string }>;
  checking: string | null;
  onCheck: (provider: string, key?: string) => Promise<void>;
  onLogin: (key: ProviderKey) => void;
  onDone: () => void;
  onError: (message: string) => void;
}) {
  const provider = () => props.provider;
  const keys = () => provider().keys;
  const noun = (n: number) => (n === 1 ? "account" : "accounts");

  // The drawer opens on the seats with the most room left, because the
  // question it is opened to answer is almost always "which of these
  // can take traffic right now". Any column can take over from there.
  const [sort, setSort] = createSignal<CredSort>({ column: "headroom", dir: "desc" });
  const sortBy = (column: CredColumn) => setSort((prev) => prev.column === column
    ? { column, dir: prev.dir === "asc" ? "desc" : "asc" }
    : { column, dir: CRED_SORT_DIR[column] });

  const [search, setSearch] = createSignal("");
  const [domain, setDomain] = createSignal("");
  // "" = any, "\u0000" = unassigned only. A sentinel rather than null so the
  // combobox can offer "Unassigned" as an ordinary choice.
  const [service, setService] = createSignal("");
  const [dupesOnly, setDupesOnly] = createSignal(false);
  const [picked, setPicked] = createSignal<string[]>([]);
  const [busy, setBusy] = createSignal<"" | "check" | "remove" | "assign">("");
  const [progress, setProgress] = createSignal("");

  // Seats by the account they sign in as, each group freshest-first so
  // the one worth keeping is the one on top.
  const accounts = createMemo(() => {
    const groups = new Map<string, ProviderKey[]>();
    for (const key of keys()) {
      const id = seatAccount(key);
      if (!id) continue;
      const group = groups.get(id);
      if (group) group.push(key);
      else groups.set(id, [key]);
    }
    for (const group of groups.values()) group.sort(byFreshness);
    return groups;
  });
  const duplicated = createMemo(
    () => new Set([...accounts()].filter(([, group]) => group.length > 1).map(([id]) => id)),
  );
  const isDuplicate = (key: ProviderKey) => {
    const id = seatAccount(key);
    return Boolean(id && duplicated().has(id));
  };
  /// Seats standing behind another: every member of a group past the
  /// freshest. This is the number an operator means by "how many
  /// duplicates" — three names on one account is two seats too many, not
  /// three.
  const redundant = createMemo(() => [...accounts().values()]
    .filter((group) => group.length > 1)
    .reduce((total, group) => total + group.length - 1, 0));

  // Services this provider's accounts are split across, with counts, plus
  // an "Unassigned" entry — on a divided pool those are the accounts that
  // serve nobody, which is the thing you most need to be able to isolate.
  const serviceOptions = createMemo(() => {
    const counts = new Map<string, number>();
    let none = 0;
    for (const key of keys()) {
      if (key.tenant) counts.set(key.tenant, (counts.get(key.tenant) ?? 0) + 1);
      else none += 1;
    }
    const out = [...counts].sort().map(([name, n]) => ({
      value: name, label: name, hint: String(n),
    }));
    if (none) out.push({ value: "\u0000", label: "Unassigned", hint: String(none) });
    return out;
  });

  const domains = createMemo(() => {
    const counts = new Map<string, number>();
    for (const key of keys()) {
      const domain = seatDomain(key);
      if (domain) counts.set(domain, (counts.get(domain) ?? 0) + 1);
    }
    return [...counts]
      .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
      .map(([value, count]) => ({ value, label: value, hint: `${count} ${noun(count)}` }));
  });

  const matching = createMemo(() => {
    const needle = search().trim().toLowerCase();
    const wanted = domain();
    const wantedService = service();
    const onlyDupes = dupesOnly();
    return keys().filter((key) => {
      if (wanted && seatDomain(key) !== wanted) return false;
      if (wantedService === "\u0000" ? key.tenant : wantedService && key.tenant !== wantedService) return false;
      if (onlyDupes && !isDuplicate(key)) return false;
      return !needle || seatHaystack(key).includes(needle);
    });
  });

  /// The rows in the chosen order — except that the seats of one account
  /// are pulled together behind whichever of them the sort placed first.
  ///
  /// Without this, sorting by headroom scatters a duplicate group down the
  /// table and the duplication goes back to being invisible, which is the
  /// problem the grouping exists to solve. Members not passing the current
  /// filter stay out: a group is drawn from what is on screen, so its
  /// "2 of 3" always counts rows the operator can see.
  const rows = createMemo(() => {
    const sorted = sortCredentials(matching(), sort(), provider().subscription);
    const shown = new Set(sorted.map((key) => key.name));
    const out: ProviderKey[] = [];
    const placed = new Set<string>();
    for (const key of sorted) {
      if (placed.has(key.name)) continue;
      const id = seatAccount(key);
      const group = id && duplicated().has(id)
        ? accounts().get(id)!.filter((member) => shown.has(member.name))
        : [key];
      for (const member of group) {
        out.push(member);
        placed.add(member.name);
      }
    }
    return out;
  });

  /// Where each row sits in its account's group, for the badge and the
  /// rail down the side. Seats that are the only one on their account —
  /// the ordinary case — are absent, and get no decoration.
  ///
  /// Built once per render rather than looked up per row: a scan of the
  /// table for every row in it is quadratic, and this table is opened
  /// precisely when the pool is too big to read.
  const groups = createMemo(() => {
    const counted = new Map<string, ProviderKey[]>();
    for (const key of rows()) {
      const id = seatAccount(key);
      if (!id || !duplicated().has(id)) continue;
      const group = counted.get(id);
      if (group) group.push(key);
      else counted.set(id, [key]);
    }
    const placing = new Map<string, { size: number; index: number; first: boolean; last: boolean }>();
    for (const group of counted.values()) {
      // A filter can leave one member of a pair on screen. The badge
      // counts rows the operator can actually see, so a lone survivor is
      // not called a duplicate — the pool-wide count above still is.
      if (group.length < 2) continue;
      group.forEach((key, index) => placing.set(key.name, {
        size: group.length,
        index,
        first: index === 0,
        last: index === group.length - 1,
      }));
    }
    return placing;
  });

  // The selection is held by name and survives a refetch, so it can name a
  // seat that has since been removed. Resolved against what is actually on
  // screen at the moment it is acted on, rather than trusted.
  const selected = createMemo(() => {
    const names = new Set(picked());
    return rows().filter((key) => names.has(key.name));
  });
  const allShown = () => rows().length > 0 && selected().length === rows().length;
  // "Some but not all" is a third state the `checked` attribute cannot
  // express, and it is the one an operator is in for most of a selection.
  const [pickAll, setPickAll] = createSignal<HTMLInputElement>();
  createEffect(() => {
    const box = pickAll();
    if (box) box.indeterminate = selected().length > 0 && !allShown();
  });
  const toggle = (name: string) => setPicked((prev) =>
    prev.includes(name) ? prev.filter((held) => held !== name) : [...prev, name]);

  // Removing the last duplicate while the duplicates-only view is on
  // leaves an empty table whose only escape — the toggle that set it —
  // has disappeared along with the thing it counted. Stand it down.
  createEffect(() => {
    if (dupesOnly() && !redundant()) setDupesOnly(false);
  });

  const filtering = () => Boolean(search().trim() || domain() || service() || dupesOnly());
  const clearFilters = () => { setSearch(""); setDomain(""); setService(""); setDupesOnly(false); };

  const probeSummary = () => {
    const entries = Object.values(props.probes);
    if (!entries.length) return "";
    const ok = entries.filter((e) => e.status === "ok").length;
    const limited = entries.filter((e) => e.status === "rate_limited").length;
    const bad = entries.length - ok - limited;
    return `Last check: ${ok} ready`
      + (limited ? `, ${limited} rate limited` : "")
      + (bad ? `, ${bad} failing` : "")
      + ".";
  };

  /// Check the selected seats, one at a time.
  ///
  /// Sequential on purpose. A check is a real request per credential, and
  /// the provider-wide endpoint caps its own fan-out precisely so that
  /// eighty seats do not fire at one provider at once; issuing N
  /// single-key checks concurrently from here would walk straight around
  /// that cap and rate-limit the pool we are trying to measure.
  const checkSelected = async () => {
    const names = selected().map((key) => key.name);
    if (!names.length) return;
    setBusy("check");
    try {
      for (const [index, name] of names.entries()) {
        setProgress(`${index + 1} of ${names.length}`);
        await props.onCheck(provider().name, name);
      }
    } finally {
      setBusy("");
      setProgress("");
    }
  };

  // Assigning a divided pool one row at a time is not a workable operation:
  // 117 accounts is 117 confirmations. Worse, done sequentially there is a
  // window where the service that owns most of the pool holds only the few
  // already moved — so the bulk path is the safe one as well as the quick one.
  const assignSelected = async (tenant: string | null) => {
    const names = picked();
    if (!names.length) return;
    if (!tenant && !confirm(`Unassign ${names.length} account${names.length === 1 ? "" : "s"}?\n\nOn a divided pool an unassigned account serves nobody.`)) return;
    setBusy("assign"); props.onError("");
    let failed = 0;
    for (const name of names) {
      try { await api.setAccountTenant(provider().name, name, tenant); }
      catch { failed += 1; }
    }
    setBusy(""); setPicked([]);
    if (failed) props.onError(`${failed} of ${names.length} could not be changed.`);
    props.onDone();
  };

  const removeSelected = async () => {
    const names = selected().map((key) => key.name);
    if (!names.length) return;
    const what = names.length === 1 ? `account ${names[0]}` : `${names.length} accounts`;
    if (!confirm(
      `Remove ${what} from ${provider().name}?\n\n`
      + "Routing stops using them immediately. The credential files on disk are left alone, "
      + "so this can be undone by adding them back.",
    )) return;
    setBusy("remove");
    props.onError("");
    try {
      const result = await api.deleteProviderKeys(provider().name, names);
      setPicked([]);
      // Some of the batch already being gone is the outcome that was
      // wanted, so it is only worth saying when *none* of it was there.
      if (!result.removed.length) {
        props.onError("Nothing to remove — those accounts were already gone.");
      }
      props.onDone();
    } catch (err) {
      props.onError(err instanceof Error ? err.message : "Remove failed");
    } finally {
      setBusy("");
    }
  };

  return <div class="seat-pane">
    <BaseUrlEditor provider={provider()} onDone={props.onDone} onError={props.onError} />
    <div class="drawer-section">
      <div class="seat-summary">
        <div class="seat-count">
          <strong>{rows().length}</strong>
          <span>
            <Show when={filtering()} fallback={noun(rows().length)}>
              of {keys().length} {noun(keys().length)}
            </Show>
          </span>
        </div>
        <Show when={redundant()}>
          <button
            type="button"
            class="seat-dupe-toggle"
            classList={{ on: dupesOnly() }}
            aria-pressed={dupesOnly()}
            title="Seats sharing an upstream account with another seat. One account's quota, counted twice."
            onClick={() => setDupesOnly((on) => !on)}
          >
            <Copy size={13} aria-hidden="true" />
            {redundant()} duplicate{redundant() === 1 ? "" : "s"}
          </button>
        </Show>
        <button
          class="button outline seat-check-all"
          disabled={!keys().length || Boolean(props.checking) || Boolean(busy())}
          onClick={() => void props.onCheck(provider().name)}
        >
          <Show when={props.checking === "*"} fallback={<Stethoscope size={14} />}><RefreshCw size={14} class="spin" /></Show>
          {props.checking === "*" ? "Checking…" : "Check all"}
        </button>
      </div>

      <Show when={keys().length > 1}>
        <div class="seat-filters">
          <div class="search-field">
            <Search size={14} aria-hidden="true" />
            {/* `type="search"` and `autocomplete="off"` together are what
                keep the browser from offering the operator's own saved
                addresses here — Chrome reads a field as an address field
                from its surroundings, and a box that mentioned "email"
                got a dropdown of the reader's contact details over the
                table they were trying to read. The placeholder names what
                it matches without using the word. */}
            <input
              data-filter
              type="search"
              autocomplete="off"
              spellcheck={false}
              value={search()}
              placeholder={provider().subscription
                ? "Search accounts, seats or domains (press /)"
                : "Search keys or domains (press /)"}
              aria-label="Search accounts"
              onInput={(e) => setSearch(e.currentTarget.value)}
            />
            <Show when={search()}>
              <button type="button" aria-label="Clear search" onClick={() => setSearch("")}>
                <X size={13} />
              </button>
            </Show>
          </div>
          {/* A single-domain pool has nothing to choose between, so the
              control only appears once there is a cut to make. */}
          <Show when={domains().length > 1}>
            <Combobox
              value={domain()}
              options={domains()}
              onSelect={setDomain}
              label="Domain"
              placeholder="Any domain"
              allowEmpty
            />
          </Show>
          {/* On a divided pool of 119, "which are the optimizer's" and
              "which are stranded" are the two questions actually asked of
              this table, and neither was answerable before. */}
          <Show when={props.tenants.length}>
            <Combobox
              value={service()}
              options={serviceOptions()}
              onSelect={setService}
              label="Service"
              placeholder="Any service"
              allowEmpty
            />
          </Show>
          <Show when={filtering()}>
            <button type="button" class="button ghost" onClick={clearFilters}>Clear filters</button>
          </Show>
        </div>
      </Show>

      <Show when={probeSummary()}>
        <p class="muted">{probeSummary()}</p>
      </Show>

      <Show
        when={keys().length}
        fallback={<Empty title="No credentials" action="A keyless provider needs none; anything else needs a key." />}
      >
        <Show
          when={rows().length}
          fallback={<Empty
            title="No accounts match"
            action="Widen the search, or clear the filters to see the whole pool."
          />}
        >
          <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table">
            <table class="dense seats" classList={{ picking: selected().length > 0 }}>
              <thead><tr>
                <th class="seat-pick">
                  <input
                    ref={setPickAll}
                    type="checkbox"
                    aria-label={allShown() ? "Deselect all shown accounts" : "Select all shown accounts"}
                    checked={allShown()}
                    onChange={(e) => setPicked(e.currentTarget.checked ? rows().map((key) => key.name) : [])}
                  />
                </th>
                <SortHeader label="Credential" column="credential" sort={sort()} onSort={sortBy} />
                <SortHeader label="Service" column="service" sort={sort()} onSort={sortBy} />
                <SortHeader
                  label={provider().subscription ? "Plan windows" : "Limits"}
                  column="headroom"
                  sort={sort()}
                  onSort={sortBy}
                />
                <SortHeader label="Token" column="token" sort={sort()} onSort={sortBy} />
                <SortHeader label="Health" column="health" sort={sort()} onSort={sortBy} />
                <th><span class="sr-only">Actions</span></th>
              </tr></thead>
              <tbody><For each={rows()}>{(key) => (
                <CredentialRow
                  providerKey={key}
                  kind={provider().kind}
                  subscription={provider().subscription}
                  managed={provider().managed}
                  checking={props.checking === "*" || props.checking === key.name}
                  probe={props.probes[key.name] ?? null}
                  group={groups().get(key.name) ?? null}
                  picked={picked().includes(key.name)}
                  onPick={() => toggle(key.name)}
                  onCheck={() => void props.onCheck(provider().name, key.name)}
                  onLogin={() => props.onLogin(key)}
                  onRemove={async () => {
                    props.onError("");
                    try {
                      await api.deleteProviderKey(provider().name, key.name);
                      props.onDone();
                    } catch (err) {
                      props.onError(err instanceof Error ? err.message : "Remove failed");
                    }
                  }}
                />
              )}</For></tbody>
            </table>
          </div>
        </Show>
      </Show>
    </div>

    <AddCredential provider={provider()} tenants={props.tenants} onDone={props.onDone} onError={props.onError} />

    {/* Sticky to the bottom of the drawer's own scroll area rather than
        the viewport, so it belongs to the table it acts on and cannot
        outlive the drawer that opened it. */}
    <Show when={selected().length}>
      <div class="seat-bar" role="group" aria-label="Actions for the selected accounts">
        <strong>{selected().length} selected</strong>
        <Show when={progress()}><span class="muted">{progress()}</span></Show>
        {/* Icons, not words. The two verbs are ordinary table actions and
            already have this exact icon on every row, so the pill stays
            narrow enough to float. Each carries the count in its label,
            because an icon-only control that destroys twelve things has
            to say twelve somewhere a screen reader will reach — and
            removal still asks before it does anything. */}
        <div class="seat-bar-actions">
          <button
            class="icon-button"
            disabled={Boolean(busy())}
            title={`Check the health of ${selected().length} selected`}
            aria-label={`Check the health of ${selected().length} selected accounts`}
            onClick={() => void checkSelected()}
          >
            <Show when={busy() === "check"} fallback={<Stethoscope size={15} />}><RefreshCw size={15} class="spin" /></Show>
          </button>
          <button
            class="icon-button danger"
            disabled={Boolean(busy())}
            title={`Remove ${selected().length} selected`}
            aria-label={`Remove ${selected().length} selected accounts`}
            onClick={() => void removeSelected()}
          >
            <Show when={busy() === "remove"} fallback={<Trash2 size={15} />}><RefreshCw size={15} class="spin" /></Show>
          </button>
        </div>
        <Show when={props.tenants.length}>
          <select
            class="service-picker"
            aria-label={`Assign ${selected().length} selected accounts to a service`}
            disabled={Boolean(busy())}
            onChange={(e) => {
              const v = e.currentTarget.value;
              e.currentTarget.value = "";
              if (v) void assignSelected(v === "\u0000" ? null : v);
            }}
          >
            <option value="" selected>Assign to…</option>
            <For each={props.tenants}>{(name) => <option value={name}>{name}</option>}</For>
            <option value="\u0000">Unassigned</option>
          </select>
        </Show>
        <button class="button ghost" disabled={Boolean(busy())} onClick={() => setPicked([])}>Clear</button>
      </div>
    </Show>
  </div>;
}

/// Pull the account email out of an uploaded Codex auth.json, from the
/// id_token's JWT claims. Best-effort: a document without one still
/// uploads, it just gets a generic seat name.
function codexEmailFrom(content: string): string | null {
  try {
    const doc = JSON.parse(content);
    const idToken: string | undefined = doc?.tokens?.id_token;
    if (!idToken) return null;
    const payload = JSON.parse(atob(idToken.split(".")[1].replace(/-/g, "+").replace(/_/g, "/")));
    return typeof payload.email === "string" ? payload.email : null;
  } catch {
    return null;
  }
}

/// A seat name that identifies the *account*, not just its mailbox.
///
/// This used to take the local part alone, so `ali@one.com` and
/// `ali@two.com` collided and the uploader numbered them `ali`, `ali-2`
/// as though one account had been added twice. That is worse than a
/// cosmetic naming problem: each seat entry carries its own breaker, so
/// two entries for one account let traffic keep hitting an account whose
/// twin was just benched for being out of quota. The whole address keeps
/// distinct accounts distinct.
function seatNameFrom(email: string | null, fallback: string): string {
  if (!email) return fallback;
  return email.toLowerCase().replace(/[^a-z0-9_-]/g, "-").replace(/-+/g, "-").replace(/^-|-$/g, "");
}

export type Seat = { name: string; email: string | null; content: string; file: string };

/// Turn a set of uploaded auth.json documents into named seats.
///
/// Seats are named after the account email, which is what an operator
/// recognises in a list of eighty. Two files for the same account are a
/// real thing in an exported pool (the same login authorised twice), so
/// duplicates are numbered rather than silently collapsed — the operator
/// can see them and drop the extras.
export async function seatsFromFiles(
  files: File[],
  taken: Iterable<string> = [],
): Promise<{ seats: Seat[]; rejected: string[] }> {
  const seats: Seat[] = [];
  const rejected: string[] = [];
  const used = new Map<string, number>();
  // Names already on the provider count as used. Without this a second
  // upload produced names the gateway had to reject or silently skip, so
  // adding forty files could yield fewer than forty seats with nothing
  // said about which were dropped.
  for (const name of taken) used.set(name, 1);
  for (const file of files) {
    const content = await file.text();
    let parsed: any;
    try {
      parsed = JSON.parse(content);
    } catch {
      rejected.push(`${file.name}: not valid JSON`);
      continue;
    }
    if (!parsed?.tokens?.refresh_token) {
      rejected.push(`${file.name}: no refresh token in it`);
      continue;
    }
    const email = codexEmailFrom(content);
    const base = seatNameFrom(email, file.name.replace(/\.json$/i, ""));
    const seen = used.get(base) ?? 0;
    used.set(base, seen + 1);
    seats.push({
      name: seen ? `${base}-${seen + 1}` : base,
      email,
      content,
      file: file.name,
    });
  }
  return { seats, rejected };
}

/// The per-kind credential field: a setup token for Claude Code, one or
/// many auth.json uploads for Codex, a reference or literal otherwise.
function CredentialField(props: {
  kind: "claude" | "codex" | "plain";
  placeholder?: string;
  onToken: (token: string) => void;
  onFile: (content: string, email: string | null) => void;
  onSeats?: (seats: Seat[], rejected: string[]) => void;
  /// Names already configured on this provider, so an upload does not
  /// propose one the gateway would reject or skip.
  taken?: string[];
}) {
  const [fileName, setFileName] = createSignal("");
  const [seats, setSeats] = createSignal<Seat[]>([]);
  const [rejected, setRejected] = createSignal<string[]>([]);
  const [email, setEmail] = createSignal<string | null>(null);
  return <Switch>
    <Match when={props.kind === "claude"}>
      <label>Setup token
        <input
          type="password"
          placeholder="sk-ant-oat01-…"
          autocomplete="off"
          onInput={(e) => props.onToken(e.currentTarget.value.trim())}
        />
      </label>
      <p class="muted">
        Run <code class="mono">claude setup-token</code> on any machine with a Claude subscription and
        paste the result — it is valid for a year and stored sealed in the gateway's store.
      </p>
    </Match>
    <Match when={props.kind === "codex"}>
      <label>auth.json {props.onSeats ? <span class="optional">One or many</span> : ""}
        <input
          type="file"
          accept=".json,application/json"
          multiple={Boolean(props.onSeats)}
          onChange={async (e) => {
            const picked = [...(e.currentTarget.files ?? [])];
            if (!picked.length) return;
            if (props.onSeats) {
              const result = await seatsFromFiles(picked, props.taken ?? []);
              setSeats(result.seats);
              setRejected(result.rejected);
              setFileName(picked.length === 1 ? picked[0].name : `${picked.length} files`);
              setEmail(result.seats[0]?.email ?? null);
              props.onSeats(result.seats, result.rejected);
              return;
            }
            const content = await picked[0].text();
            const found = codexEmailFrom(content);
            setFileName(picked[0].name);
            setEmail(found);
            props.onFile(content, found);
          }}
        />
      </label>
      <Show when={seats().length > 1}>
        <div class="seat-preview">
          <p class="muted"><strong>{seats().length} seats</strong> ready — named after each account.</p>
          <div class="seat-chips">
            <For each={seats().slice(0, 12)}>{(seat) => <span class="chip">{seat.email ?? seat.name}</span>}</For>
            <Show when={seats().length > 12}><span class="chip muted">+{seats().length - 12} more</span></Show>
          </div>
        </div>
      </Show>
      <Show when={rejected().length}>
        <p class="form-error" role="alert">{rejected().length} file{rejected().length > 1 ? "s" : ""} skipped: {rejected().slice(0, 3).join("; ")}{rejected().length > 3 ? "…" : ""}</p>
      </Show>
      <Show when={seats().length <= 1}>
        <p class="muted">
          {fileName()
            ? email()
              ? <>Signed in as <strong>{email()}</strong> — the seat is named after it.</>
              : `${fileName()} loaded; no email found in it, using a generic seat name.`
            : <>Upload the <code class="mono">~/.codex/auth.json</code> files the Codex CLI wrote after sign-in
              — select as many as you like. The gateway keeps its own copies and renews the tokens itself.</>}
        </p>
      </Show>
    </Match>
    <Match when={true}>
      <label>Credential
        <input placeholder={props.placeholder} onInput={(e) => props.onToken(e.currentTarget.value)} />
      </label>
    </Match>
  </Switch>;
}

function AddProviderDialog(props: {
  catalog: { presets: CatalogPreset[]; subscriptions: CatalogPreset[]; custom: CatalogPreset; configured: string[] } | undefined;
  onClose: () => void;
  onDone: () => void;
}) {
  escapeCloses(props.onClose);
  const [preset, setPreset] = createSignal("");
  const [name, setName] = createSignal("");
  const [baseUrl, setBaseUrl] = createSignal("");
  const [keyName, setKeyName] = createSignal("main");
  const [value, setValue] = createSignal("");
  const [fileContent, setFileContent] = createSignal("");
  const [seats, setSeats] = createSignal<Seat[]>([]);
  const [error, setError] = createSignal("");
  const [pending, setPending] = createSignal(false);

  const all = createMemo<CatalogPreset[]>(() => [
    ...(props.catalog ? [props.catalog.custom] : []),
    ...(props.catalog?.subscriptions ?? []),
    ...(props.catalog?.presets ?? []),
  ]);
  const chosen = createMemo(() => all().find((p) => p.name === preset()));
  const isCustom = () => Boolean(chosen()?.custom);
  const options = createMemo<Option[]>(() =>
    all().map((p) => ({
      value: p.name,
      label: providerMeta(p.name).label,
      hint: p.custom ? "any OpenAI-shaped endpoint" : p.subscription ? "subscription seats" : (p.base_url ?? "custom endpoint"),
      icon: <ProviderMark name={p.name} size={20} />,
    })),
  );
  createEffect(() => {
    const p = chosen();
    if (!p) return;
    setBaseUrl(p.custom ? "" : (p.base_url ?? ""));
    if (!name() || all().some((c) => c.name === name() || c.name.replace("_subscription", "") === name()) || name() === "custom") {
      setName(p.custom ? "custom" : p.subscription ? p.name.replace("_subscription", "") : p.name);
    }
  });

  return <div class="dialog-backdrop" role="presentation" onMouseDown={(e) => { if (e.target === e.currentTarget) props.onClose(); }}>
    <form class="dialog wide" role="dialog" aria-modal="true" aria-labelledby="add-provider-title" onSubmit={async (e) => {
      e.preventDefault(); setError(""); setPending(true);
      try {
        const p = chosen();
        // Subscription credentials resolve to references first: a pasted
        // setup token is sealed into the store, an uploaded auth.json is
        // persisted server-side so the refresher can write rotations
        // back. The config document only ever carries the reference.
        // A pool of Codex seats is uploaded in one shot: every document
        // is persisted in a single request, then the provider is created
        // with all of its keys in a single config commit.
        if (p?.name === "codex_subscription" && seats().length > 1) {
          const written = await api.putCredentialFiles(seats().map((seat) => ({
            name: `${name()}_${seat.name}`.replace(/[^a-zA-Z0-9_-]/g, "_"),
            content: seat.content,
          })));
          if (!written.written.length) throw new Error("no credential files could be saved");
          await api.createProvider({
            name: name(),
            kind: p.name,
            ...(baseUrl().trim() ? { base_url: baseUrl().trim() } : {}),
            keys: written.written.map((entry, i) => ({
              name: seats()[i]?.name ?? entry.name,
              value: entry.reference,
            })),
          });
          props.onDone();
          return;
        }
        let credential = value();
        if (p?.name === "claude_subscription" && value()) {
          const sealed = await api.putSecret(`${name()}_${keyName()}`.replace(/[^a-zA-Z0-9_-]/g, "_"), value());
          credential = sealed.reference;
        } else if (p?.name === "codex_subscription" && fileContent()) {
          const saved = await api.putCredentialFile(`${name()}_${keyName()}`.replace(/[^a-zA-Z0-9_-]/g, "_"), fileContent());
          credential = saved.reference;
        }
        // No model seeding: model line-ups change weekly, so models are
        // added explicitly on the Models page.
        await api.createProvider({
          name: name(),
          kind: p?.custom ? "openai_compat" : p?.subscription ? p.name : preset(),
          ...(baseUrl().trim() ? { base_url: baseUrl().trim() } : {}),
          ...(!credential && (p?.custom || p?.keyless_ok) ? { auth: "none" } : {}),
          keys: credential ? [{ name: keyName(), value: credential }] : [],
        });
        props.onDone();
      } catch (err) { setError(err instanceof Error ? err.message : "Could not add provider"); }
      finally { setPending(false); }
    }}>
      <header class="dialog-head">
        <div><h2 id="add-provider-title">Add provider</h2><p class="muted">Models are added afterwards, on the Models page.</p></div>
        <button type="button" class="icon-button" aria-label="Close" title="Close (Esc)" onClick={props.onClose}><X size={16} /></button>
      </header>
      <label>Provider
        <Combobox value={preset()} options={options()} onSelect={setPreset} label="Provider" placeholder="Choose a provider…" />
      </label>
      <Show when={chosen()}>
        <div class="field-row">
          <label>Name in this gateway
            <input required value={name()} onInput={(e) => setName(e.currentTarget.value)} />
          </label>
          <label>Base URL {isCustom() ? "" : <span class="optional">Optional override</span>}
            <input
              required={isCustom()}
              placeholder={isCustom() ? "https://llm.internal.example.com/v1" : "provider default"}
              value={baseUrl()}
              onInput={(e) => setBaseUrl(e.currentTarget.value)}
            />
          </label>
        </div>
        <p class="muted">Callers address it as <code class="mono">{name() || "name"}/model</code>.</p>
        <div class="field-row">
          <Show when={seats().length <= 1} fallback={<div><span class="field-label">Credential names</span><p class="muted">Taken from each file's account.</p></div>}>
            <label>Credential name<input value={keyName()} onInput={(e) => setKeyName(e.currentTarget.value)} /></label>
          </Show>
          <div>
            <CredentialField
              kind={chosen()!.name === "claude_subscription" ? "claude" : chosen()!.name === "codex_subscription" ? "codex" : "plain"}
              placeholder={`env.${chosen()!.discovery_env ?? "API_KEY"}`}
              onToken={setValue}
              onFile={(content, email) => {
                setFileContent(content);
                setValue("uploaded");
                setKeyName(seatNameFrom(email, keyName() || "seat-1"));
              }}
              onSeats={(list) => {
                setSeats(list);
                setValue(list.length ? "uploaded" : "");
                if (list.length === 1) {
                  setFileContent(list[0].content);
                  setKeyName(list[0].name);
                }
              }}
            />
          </div>
        </div>
      </Show>
      <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
      <div class="dialog-actions">
        <button type="button" class="button outline" onClick={props.onClose}>Cancel</button>
        <button class="button primary" disabled={pending() || !preset() || !name() || (isCustom() && !baseUrl().trim())}>{pending() ? "Adding…" : seats().length > 1 ? `Add provider with ${seats().length} seats` : "Add provider"}</button>
      </div>
    </form>
  </div>;
}

/// The health of a provider is the state of its worst key: one benched
/// seat in a healthy pool is the thing an operator needs to see, and an
/// average would hide it.
/// A short absolute instant: date for anything beyond today, time for
/// today, because "expires today" and "expires in 40 minutes" are
/// different operational facts.
function formatClock(ms: number): string {
  const date = new Date(ms);
  const sameDay = date.toDateString() === new Date().toDateString();
  return sameDay
    ? date.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })
    : date.toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });
}

/// A wall-clock moment, date and time both. "resets in 4h" makes an
/// operator do the arithmetic against a clock they can already see, and
/// gets it wrong across a day boundary; the moment itself does not.
function formatMoment(ms: number): string {
  const date = new Date(ms);
  const sameDay = date.toDateString() === new Date().toDateString();
  return date.toLocaleString(undefined, {
    ...(sameDay ? {} : { month: "short", day: "numeric" }),
    hour: "numeric",
    minute: "2-digit",
  });
}

/// How long ago, said the way somebody reads a freshness stamp.
///
/// An age is right here where [`formatMoment`] is right for a reset: the
/// question is "is this current", and "checked at 09:14" makes the reader
/// work that out against a clock. The gateway sweeps every sixty seconds,
/// so "just now" has to cover a little more than a minute — otherwise
/// every seat on the page reads as stale in the gap between ticks.
function formatAge(ms: number): string {
  const seconds = Math.max(0, (Date.now() - ms) / 1000);
  if (seconds < 75) return "just now";
  if (seconds < 3600) return `${Math.round(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)}h ago`;
  return `${Math.round(seconds / 86400)}d ago`;
}

/// The check words the gateway returns, in the terms an operator uses.
/// Only ever a fallback: an upstream that explained itself is quoted
/// verbatim instead, because its explanation is the more useful one.
const CHECK_STATUS: Record<string, string> = {
  ok: "ready",
  rate_limited: "rate limited",
  unauthorized: "credential rejected",
  provider_error: "provider error",
  rejected: "request rejected",
  unreachable: "could not reach provider",
};

function providerHealth(provider: Provider): { label: string; tone: "success" | "danger" | "muted" } {
  if (!provider.keys.length) return { label: "No keys", tone: "muted" };
  if (provider.keys.every((k: ProviderKey) => k.health === "benched")) return { label: "Out of quota", tone: "danger" };
  if (provider.keys.some((k: ProviderKey) => k.health === "benched")) return { label: "Partly benched", tone: "danger" };
  if (provider.keys.some((k: ProviderKey) => k.health === "open")) return { label: "Degraded", tone: "danger" };
  return { label: "Ready", tone: "success" };
}

/// Name a plan window by the length the provider reported for it.
///
/// "Primary window" tells an operator nothing at 2am, so these are
/// labelled — but the label has to come from the data, not from the
/// position. Anthropic's two windows are fixed at 5h and 7d and it
/// reports both lengths. Codex reports `x-codex-<window>-window-minutes`
/// per window and the lengths follow the plan: a seat whose primary
/// window is the weekly one has no 5-hour limit at all, and drawing its
/// weekly usage under a "5-hour limit" heading is how a seat with days
/// of room reads as one about to come back.
function planWindowLabel(length_s: number): string {
  if (length_s < 3600) return `${Math.round(length_s / 60)}-minute limit`;
  if (length_s < 86400) return `${Math.round(length_s / 3600)}-hour limit`;
  const days = Math.round(length_s / 86400);
  return days === 7 ? "Weekly limit" : `${days}-day limit`;
}

/// The credential columns an operator can order the table by.
type CredColumn = "credential" | "service" | "headroom" | "token" | "health";
type CredSort = { column: CredColumn; dir: "asc" | "desc" };

/// The direction a column starts in when it is first clicked. Every one
/// of these puts the answer to the column's own question on top: most
/// room left, soonest expiry, healthiest, A first. Clicking again flips.
const CRED_SORT_DIR: Record<CredColumn, "asc" | "desc"> = {
  credential: "asc",
  // A first, so the services group together and Unassigned lands last.
  service: "asc",
  headroom: "desc",
  token: "asc",
  health: "asc",
};

/// How much of its longest plan window a seat has left, 0…1.
///
/// The longest window is the weekly one on the plans that report both,
/// and weekly headroom is what decides whether a seat is worth routing
/// to: one with 90% of its week left is still a good seat at 95% of a
/// 5-hour window, and a seat with the week spent is not, however empty
/// its 5-hour window looks right now.
function planHeadroom(key: ProviderKey): number | null {
  const windows = [key.quota?.primary, key.quota?.secondary]
    .filter((w): w is QuotaWindow => Boolean(w?.length_s));
  if (!windows.length) return null;
  const longest = windows.reduce((a, b) => (b.length_s! > a.length_s! ? b : a));
  return 1 - Math.min(Math.max(longest.utilization, 0), 1);
}

const CRED_HEALTH_RANK: Record<string, number> = {
  ready: 0, healthy: 0, near_limit: 1, probing: 2, open: 3, exhausted: 4, benched: 4,
};

/// What a column sorts on. `null` means this credential has nothing to
/// sort by, and always sinks to the bottom whichever way the column
/// points — a seat that has never reported a window is not the emptiest
/// one, it is the unknown one, and floating it to the top of "most room
/// left" would send traffic at a seat nobody has heard from.
function credSortValue(key: ProviderKey, column: CredColumn, subscription: boolean): number | string | null {
  switch (column) {
    case "credential":
      return (key.credential?.email ?? key.name).toLowerCase();
    case "headroom": {
      if (subscription) return planHeadroom(key);
      // A metered key has no window; its column shows the ceiling left
      // this minute, so that is what its column sorts on.
      const left = key.limits.rpm?.remaining ?? key.limits.tpm?.remaining;
      return left ?? null;
    }
    case "token":
      return key.credential?.expires_at_ms ?? null;
    case "health":
      return CRED_HEALTH_RANK[key.status ?? key.health] ?? 5;
    // Unassigned sorts after every named service rather than before it:
    // on a divided pool those are the accounts serving nobody, and the
    // question the column is clicked to answer is "which are stranded".
    case "service":
      return key.tenant ?? "\uffff";
  }
}

function sortCredentials(keys: ProviderKey[], sort: CredSort, subscription: boolean): ProviderKey[] {
  const dir = sort.dir === "asc" ? 1 : -1;
  return [...keys].sort((a, b) => {
    const left = credSortValue(a, sort.column, subscription);
    const right = credSortValue(b, sort.column, subscription);
    if (left === null || right === null) {
      if (left === right) return a.name.localeCompare(b.name);
      return left === null ? 1 : -1;
    }
    const order = typeof left === "string" || typeof right === "string"
      ? String(left).localeCompare(String(right))
      : left - right;
    // Ties fall back to the name so the order is the same on every
    // poll; a pool of seats all sitting at 0% used would otherwise
    // reshuffle under the cursor every few seconds.
    return order ? order * dir : a.name.localeCompare(b.name);
  });
}

/// The domain half of a seat's account email, lowercased.
///
/// A pool is assembled per organisation, so the domain is the coarsest
/// useful cut through it — "the seats on the corporate tenant, not the
/// personal one somebody added to unblock a demo". Metered keys have no
/// email and so no domain; they get `null` rather than a bucket.
function seatDomain(key: ProviderKey): string | null {
  const email = key.credential?.email;
  if (!email) return null;
  const at = email.lastIndexOf("@");
  return at < 0 ? null : email.slice(at + 1).toLowerCase();
}

/// Which upstream account a seat *is*, as opposed to what it is called.
///
/// This is the whole point of grouping. A key's name is a file name,
/// chosen by whoever imported the credential, so two files holding
/// credentials for one ChatGPT account are one account's quota wearing
/// two names — and nothing else on the row says so. `account_id` is
/// authoritative; the email is the fallback for a credential shape that
/// carries no account claim, lowercased because one mailbox written two
/// ways is still one mailbox.
function seatAccount(key: ProviderKey): string | null {
  const cred = key.credential;
  if (!cred) return null;
  if (cred.account_id) return cred.account_id;
  return cred.email ? `email:${cred.email.toLowerCase()}` : null;
}

/// How much a seat is worth keeping when two of them are one account,
/// most significant first: a credential that can renew itself outlives
/// one that cannot; a later expiry means a more recently refreshed
/// document; then the seat the provider spoke to most recently, and the
/// one that has actually served traffic.
function seatFreshness(key: ProviderKey): number[] {
  const cred = key.credential;
  return [
    cred?.can_refresh ? 1 : 0,
    cred?.expires_at_ms ?? 0,
    key.last_check?.checked_at_ms ?? 0,
    key.leases ?? 0,
  ];
}

/// Freshest first, ties broken by name so a group of identical seats does
/// not reshuffle under the cursor between polls.
function byFreshness(a: ProviderKey, b: ProviderKey): number {
  const left = seatFreshness(a);
  const right = seatFreshness(b);
  for (let i = 0; i < left.length; i += 1) {
    if (left[i] !== right[i]) return right[i] - left[i];
  }
  return a.name.localeCompare(b.name);
}

/// Everything about a seat worth matching a typed query against.
///
/// The account id is in here without being on screen: an operator holding
/// one from a provider dashboard can paste it and get its seats, which is
/// the lookup that otherwise means decoding tokens by hand.
function seatHaystack(key: ProviderKey): string {
  return [
    key.name,
    key.credential?.email ?? "",
    key.credential?.account_id ?? "",
    seatDomain(key) ?? "",
    key.source_path ?? "",
  ]
    .join(" ")
    .toLowerCase();
}

function SortHeader(props: {
  label: string;
  column: CredColumn;
  sort: CredSort;
  onSort: (column: CredColumn) => void;
}) {
  const active = () => props.sort.column === props.column;
  return <th aria-sort={active() ? (props.sort.dir === "asc" ? "ascending" : "descending") : "none"}>
    <button type="button" class="th-sort" classList={{ active: active() }} onClick={() => props.onSort(props.column)}>
      {props.label}
      <Show when={active()} fallback={<ChevronsUpDown size={11} class="th-sort-idle" />}>
        <Show when={props.sort.dir === "asc"} fallback={<ArrowDown size={11} />}><ArrowUp size={11} /></Show>
      </Show>
    </button>
  </th>;
}

/// Sign a Codex seat back in with a one-time code.
///
/// The gateway does the OAuth; this dialog is a display for a code and a
/// link, and a poller for the answer. Closing it does not cancel the
/// login — the exchange rotates a refresh token that has to be written
/// to disk, so it runs server-side and finishes whether or not anyone is
/// watching. Re-opening on the same seat re-attaches to the same code.
function DeviceLoginDialog(props: {
  provider: string;
  credential: string;
  email?: string | null;
  onClose: () => void;
  onSignedIn: () => void;
}) {
  escapeCloses(props.onClose);
  const [login, setLogin] = createSignal<DeviceLogin | null>(null);
  const [error, setError] = createSignal("");
  const [copied, setCopied] = createSignal(false);

  // Narrowed here rather than at each use: `outcome` is a discriminated
  // union and JSX callbacks are not where TypeScript narrows it.
  const signedAs = () => {
    const outcome = login()?.outcome;
    return outcome?.state === "signed" ? outcome.email : null;
  };
  const failure = () => {
    const outcome = login()?.outcome;
    return outcome?.state === "failed" ? outcome.reason : null;
  };
  const state = () => login()?.outcome.state;

  // Registered synchronously, at setup: an `onCleanup` after an `await`
  // has no owner to attach to, and the poller would outlive the dialog.
  let timer: ReturnType<typeof setInterval> | undefined;
  onCleanup(() => clearInterval(timer));

  onMount(async () => {
    let started: DeviceLogin;
    try {
      started = await api.startDeviceLogin(props.provider, props.credential);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not start the login");
      return;
    }
    setLogin(started);
    if (started.outcome.state !== "waiting") return;

    // Polled rather than streamed: a login takes as long as a person
    // takes, and one small request every three seconds for a few minutes
    // is cheaper than holding a connection open for it.
    timer = setInterval(async () => {
      try {
        const next = await api.deviceLoginStatus(props.provider, props.credential, started.session);
        setLogin(next);
        if (next.outcome.state !== "waiting") {
          clearInterval(timer);
          if (next.outcome.state === "signed") props.onSignedIn();
        }
      } catch (err) {
        clearInterval(timer);
        setError(err instanceof Error ? err.message : "Lost track of the login");
      }
    }, 3000);
  });

  return <div class="dialog-backdrop" role="presentation" onMouseDown={(e) => { if (e.target === e.currentTarget) props.onClose(); }}>
    <div class="dialog" role="dialog" aria-modal="true" aria-labelledby="device-login-title">
      <header class="dialog-head">
        <div>
          <h2 id="device-login-title">Sign in again</h2>
          <p class="muted">{props.email ?? props.credential}</p>
        </div>
        <button type="button" class="icon-button" aria-label="Close" title="Close (Esc)" onClick={props.onClose}><X size={16} /></button>
      </header>

      <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>

      <Show when={login()} fallback={<Show when={!error()}><p class="muted">Asking OpenAI for a code…</p></Show>}>
        {(current) => <Switch>
          <Match when={state() === "signed"}>
            <p class="login-done" role="status">
              Signed in{signedAs() ? ` as ${signedAs()}` : ""}. The seat is live again — no restart needed.
            </p>
          </Match>
          <Match when={failure()}>
            <p class="form-error" role="alert">{failure()}</p>
            <p class="muted">Nothing changed: the seat still has the credential it had.</p>
          </Match>
          <Match when={state() === "waiting"}>
            <ol class="login-steps">
              <li>
                Open <a href={current().verification_url} target="_blank" rel="noreferrer noopener">{current().verification_url}</a> and
                sign in as <strong>{props.email ?? "this account"}</strong>.
              </li>
              <li>
                Enter this one-time code:
                <div class="login-code">
                  <code>{current().user_code}</code>
                  <button
                    type="button"
                    class="button outline"
                    onClick={async () => {
                      try {
                        await navigator.clipboard.writeText(current().user_code);
                        setCopied(true);
                        setTimeout(() => setCopied(false), 1500);
                      } catch { setCopied(false); }
                    }}
                  ><Copy size={14} />{copied() ? "Copied" : "Copy"}</button>
                </div>
              </li>
              <li>Come back here. The credential is written for you.</li>
            </ol>
            <p class="muted" role="status">
              <RefreshCw size={12} class="spin" /> Waiting for you to finish — the code expires {formatMoment(current().expires_at_ms)}.
            </p>
            {/* The one thing a code like this is dangerous for. The CLI
                says it too, in the same words. */}
            <p class="muted">Device codes are a common phishing target. Never share this code.</p>
          </Match>
        </Switch>}
      </Show>

      <div class="dialog-actions">
        <button type="button" class="button outline" onClick={props.onClose}>
          {state() === "waiting" ? "Close — the login keeps running" : "Close"}
        </button>
      </div>
    </div>
  </div>;
}

/// One credential as a table row.
/// Assigning an account — or a key — to a service.
///
/// A select rather than a text box: the services are declared, so an
/// operator should never be able to mistype one into a state where the
/// account belongs to nobody.
function ServicePicker(props: {
  value: string | null;
  tenants: string[];
  label: string;
  onChange: (tenant: string | null) => Promise<void>;
}) {
  const [busy, setBusy] = createSignal(false);
  return <select
    class="service-picker"
    aria-label={props.label}
    disabled={busy()}
    onChange={async (e) => {
      const next = e.currentTarget.value;
      setBusy(true);
      try { await props.onChange(next === "" ? null : next); }
      finally { setBusy(false); }
    }}
  >
    {/* `selected` on the option, not `value` on the select. The options are
        rendered by <For>, so they do not exist yet when a value set on the
        select is applied — the browser finds no match, falls back to the
        first option, and never re-applies. That showed every assigned key as
        "Unassigned" while the account counts beside it were right. */}
    <option value="" selected={!props.value}>Unassigned</option>
    <For each={props.tenants}>{(t) =>
      <option value={t} selected={t === props.value}>{t}</option>
    }</For>
  </select>;
}

function CredentialRow(props: {
  providerKey: ProviderKey;
  kind: string;
  subscription: boolean;
  managed: boolean;
  onRemove: () => void;
  onCheck: () => void;
  onLogin: () => void;
  onPick: () => void;
  picked: boolean;
  checking?: boolean;
  probe?: { status: string; detail: string } | null;
  /// Where this seat sits among the others on its account, when there are
  /// others. `null` for the ordinary case of one seat, one account.
  group: { size: number; index: number; first: boolean; last: boolean } | null;
}) {
  const key = () => props.providerKey;
  // Two different ways a seat asks to be signed in again, and neither is
  // reliable alone. The credential itself only knows it cannot renew;
  // a refresh token that has been *revoked* still looks renewable from
  // here and only confesses when something uses it, which is what the
  // probe detail carries back.
  const needsLogin = () => {
    const cred = key().credential;
    if (cred && !cred.can_refresh && cred.expired) return true;
    const detail = props.probe?.detail?.toLowerCase() ?? "";
    return props.probe?.status !== "ok" && (detail.includes("sign") || detail.includes("token is expired"));
  };
  // The server folds breaker, quota and credential into one word; fall
  // back to raw breaker health for a gateway that has not been updated.
  const statusLabel = () => ({
    ready: "ready",
    near_limit: "near limit",
    exhausted: "out of quota",
    probing: "probing",
  } as Record<string, string>)[key().status ?? ""] ?? key().health;
  const statusTone = () => {
    switch (key().status ?? key().health) {
      case "ready":
      case "healthy":
        return "success";
      case "near_limit":
      case "probing":
        return "warning";
      default:
        return "danger";
    }
  };
  // What the provider last said about this seat, in one line.
  //
  // A check running in this tab wins while it is in flight, because it is
  // newer than anything the gateway has recorded yet. Otherwise this is
  // the gateway's own record — written by its sixty-second sweep, by
  // whoever last ran a check, or by the last request the seat served —
  // which is what makes an opened drawer show the state of the fleet
  // rather than the history of one browser session.
  const checkNote = () => {
    const live = props.probe;
    const seen = key().last_check;
    if (!live && !seen) return null;
    const status = live?.status ?? seen!.status;
    const detail = live?.detail ?? seen!.detail;
    const when = live ? "just now" : formatAge(seen!.checked_at_ms);
    // "served" is the stronger claim of the two: it means a real caller
    // was answered, not that we asked on their behalf.
    const how = !live && seen!.probed === false ? "served" : "checked";
    return {
      ok: status === "ok",
      text: status === "ok"
        ? `${how} ${when}`
        : `${detail || CHECK_STATUS[status] || status} · ${when}`,
    };
  };

  // Only the windows the provider sized. The Codex backend answers with
  // an empty `secondary` set when a plan has one window — 0% used, no
  // length, no reset — and a nameless meter reading 0% next to an
  // exhausted one is worse than no row at all.
  const planWindows = () => [key().quota?.primary, key().quota?.secondary]
    .filter((w): w is QuotaWindow => Boolean(w?.length_s));

  const windowCell = (win: QuotaWindow) => {
    const pct = Math.round(Math.min(win.utilization, 1) * 100);
    const meterTone = win.rejected || pct >= 100 ? "danger" : pct >= 80 ? "warning" : "";
    return <div class="cred-window">
      <span class="cred-window-label">{planWindowLabel(win.length_s!)}</span>
      <div class={`meter ${meterTone}`}><i style={{ width: `${pct}%` }} /></div>
      <span class="cred-window-pct">{pct}%{win.resets_in_s ? ` · resets ${formatMoment(Date.now() + win.resets_in_s * 1000)}` : ""}</span>
    </div>;
  };

  return <tr
    classList={{
      picked: props.picked,
      "seat-grouped": Boolean(props.group),
      "seat-group-first": Boolean(props.group?.first),
      "seat-group-last": Boolean(props.group?.last),
    }}
  >
    <td class="seat-pick">
      <input
        type="checkbox"
        checked={props.picked}
        aria-label={`Select ${key().credential?.email ?? key().name}`}
        onChange={props.onPick}
      />
    </td>
    <td>
      <strong>{key().credential?.email ?? key().name}</strong>
      <Show when={props.group} keyed>
        {(group) => (
          <span
            class="seat-dupe"
            classList={{ keep: group.first }}
            title={
              (key().credential?.account_id
                ? `Upstream account ${key().credential!.account_id}. `
                : "")
              + `${group.size} seats on this one account — its quota is shared between them, `
              + "not multiplied."
            }
          >
            <Copy size={11} aria-hidden="true" />
            {group.first
              ? `freshest of ${group.size} on this account`
              : `duplicate ${group.index + 1} of ${group.size}`}
          </span>
        )}
      </Show>
    </td>
    <td>
      {/* Read-only here on purpose. Assigning belongs in one place — the
          service's own drawer — because that is where you can see what a
          service holds and what taking an account would cost the service
          it came from. A picker repeated down 117 rows shows neither. */}
      <Show when={key().tenant} fallback={
        <span class="muted">{props.managed ? "unassigned — serves nobody" : "—"}</span>
      }>
        <span class="pill">{key().tenant}</span>
      </Show>
    </td>
    <td>
      <Show when={props.subscription} fallback={
        <Show when={key().limits.rpm || key().limits.tpm} fallback={<span class="muted">No ceiling set</span>}>
          <small>
            {key().limits.rpm ? `${formatNumber(key().limits.rpm!.remaining ?? 0)} req left this minute` : ""}
            {key().limits.rpm && key().limits.tpm ? " · " : ""}
            {key().limits.tpm ? `${formatNumber(key().limits.tpm!.remaining ?? 0)} tok left` : ""}
          </small>
        </Show>
      }>
        <Show when={planWindows().length} fallback={<span class="muted">Reports after the first request</span>}>
          <For each={planWindows()}>{(win) => windowCell(win)}</For>
        </Show>
      </Show>
    </td>
    <td>
      <Show when={key().credential} fallback={<span class="muted">—</span>}>
        {(() => {
          const cred = key().credential!;
          const at = cred.expires_at_ms;
          // An expired access token that can refresh is the normal
          // resting state of a Codex seat, not a fault: the refresher
          // renews it on the next request. Only a credential that
          // cannot refresh is actually a problem worth alarming about.
          if (cred.can_refresh) {
            return <small class="muted">
              {!at || cred.expired ? "renews on next use" : `valid until ${formatClock(at)}`}
            </small>;
          }
          if (!at) return <small class="muted">No readable expiry</small>;
          const days = Math.round((at - Date.now()) / 86_400_000);
          return <>
            <small classList={{ danger: cred.expired || days <= 7 }}>
              {cred.expired ? "Expired" : days <= 1 ? "Expires today" : `${days} days left`}
            </small>
            <small class="muted">{formatClock(at)}</small>
          </>;
        })()}
      </Show>
    </td>
    <td>
      <span class="pill" classList={{
        success: statusTone() === "success",
        warning: statusTone() === "warning",
        danger: statusTone() === "danger",
      }}>{statusLabel()}</span>
      <Show when={checkNote()} keyed>
        {(note) => <small classList={{ danger: !note.ok, muted: note.ok }}>{note.text}</small>}
      </Show>
      <Show when={key().credential && !key().credential!.can_refresh && key().credential!.expired}>
        <small class="danger">re-authorise needed</small>
      </Show>
    </td>
    <td class="actions">
      {/* Codex seats only: this is Codex's own device-code endpoint. The
          button is always offered rather than shown only when a seat
          looks dead, because the signal for "needs a login" is a probe
          away — a revoked refresh token reads as a perfectly ordinary
          "renews on next use" until something actually tries it. */}
      <Show when={props.kind === "CodexSubscription"}>
        <button
          class="icon-button"
          classList={{ danger: needsLogin() }}
          title={needsLogin() ? `Sign ${key().name} in again — its credential is dead` : `Sign ${key().name} in again`}
          aria-label={`Sign ${key().name} in again`}
          onClick={props.onLogin}
        >
          <LogIn size={14} />
        </button>
      </Show>
      <button
        class="icon-button"
        title={`Check ${key().name}`}
        aria-label={`Check ${key().name}`}
        disabled={props.checking}
        onClick={props.onCheck}
      >
        <Show when={props.checking} fallback={<Stethoscope size={14} />}><RefreshCw size={14} class="spin" /></Show>
      </button>
      <button class="icon-button danger" title={`Remove ${key().name}`} aria-label={`Remove ${key().name}`} onClick={props.onRemove}><Trash2 size={14} /></button>
    </td>
  </tr>;
}

function BaseUrlEditor(props: { provider: Provider; onDone: () => void; onError: (msg: string) => void }) {
  const [value, setValue] = createSignal(props.provider.base_url ?? "");
  const [pending, setPending] = createSignal(false);
  const dirty = () => (value().trim() || "") !== (props.provider.base_url ?? "");
  return <div class="drawer-section">
    <SectionTitle title="Endpoint" subtitle="Base URL requests are sent to; empty uses the provider default" />
    <div class="baseurl-row">
      <input
        placeholder="https://api.example.com/v1"
        value={value()}
        aria-label="Base URL"
        onInput={(e) => setValue(e.currentTarget.value)}
      />
      <button class="button outline" disabled={!dirty() || pending()} onClick={async () => {
        setPending(true); props.onError("");
        try {
          await api.updateProvider(props.provider.name, { base_url: value().trim() });
          props.onDone();
        } catch (err) { props.onError(err instanceof Error ? err.message : "Update failed"); }
        finally { setPending(false); }
      }}>{pending() ? "Saving…" : "Save"}</button>
    </div>
  </div>;
}

/// Adding a credential to an existing provider, with the same per-kind
/// capture as the add dialog: a setup token for Claude Code, an
/// auth.json upload for Codex, a reference for metered keys.
function AddCredential(props: { provider: Provider; tenants: string[]; onDone: () => void; onError: (msg: string) => void }) {
  const [open, setOpen] = createSignal(false);
  const [name, setName] = createSignal("");
  const [value, setValue] = createSignal("");
  const [fileContent, setFileContent] = createSignal("");
  const [seats, setSeats] = createSignal<Seat[]>([]);
  const [rpm, setRpm] = createSignal("");
  const [tpm, setTpm] = createSignal("");
  const [tenant, setTenant] = createSignal("");
  const [pending, setPending] = createSignal(false);
  const kind = () => {
    const k = props.provider.kind.toLowerCase();
    return k.includes("claude") ? "claude" as const : k.includes("codex") ? "codex" as const : "plain" as const;
  };
  return <div class="drawer-section">
    <Show when={open()} fallback={
      <button class="button outline" onClick={() => setOpen(true)}><Plus size={14} />Add credential</button>
    }>
      <form class="panel" onSubmit={async (e) => {
        e.preventDefault(); setPending(true); props.onError("");
        try {
          if (kind() === "codex" && seats().length > 1) {
            const written = await api.putCredentialFiles(seats().map((seat) => ({
              name: `${props.provider.name}_${seat.name}`.replace(/[^a-zA-Z0-9_-]/g, "_"),
              content: seat.content,
            })));
            const result = await api.addProviderKeys(
              props.provider.name,
              written.written.map((entry, i) => ({
                name: seats()[i]?.name ?? entry.name,
                value: entry.reference,
                ...(tenant() ? { tenant: tenant() } : {}),
              })),
            );
            if (result.skipped.length) {
              props.onError(`Added ${result.added.length}; skipped ${result.skipped.length} already present.`);
            }
            setOpen(false); setSeats([]); setName(""); setValue(""); setFileContent("");
            props.onDone();
            return;
          }
          let credential = value();
          const safe = `${props.provider.name}_${name()}`.replace(/[^a-zA-Z0-9_-]/g, "_");
          if (kind() === "claude" && value()) {
            credential = (await api.putSecret(safe, value())).reference;
          } else if (kind() === "codex" && fileContent()) {
            credential = (await api.putCredentialFile(safe, fileContent())).reference;
          }
          await api.addProviderKey(props.provider.name, {
            name: name(), value: credential,
            ...(tenant() ? { tenant: tenant() } : {}),
            ...(rpm() ? { rpm: Number(rpm()) } : {}),
            ...(tpm() ? { tpm: Number(tpm()) } : {}),
          });
          setOpen(false); setName(""); setValue(""); setFileContent(""); setRpm(""); setTpm("");
          props.onDone();
        } catch (err) { props.onError(err instanceof Error ? err.message : "Add failed"); }
        finally { setPending(false); }
      }}>
        <SectionTitle title="New credential" subtitle={
          kind() === "claude" ? "Another subscription seat via its setup token"
          : kind() === "codex" ? "Another seat via its auth.json"
          : "Stored by reference where possible"} />
        <Show when={seats().length <= 1}>
          <label>Name<input required={seats().length <= 1} placeholder="seat-2" value={name()} onInput={(e) => setName(e.currentTarget.value)} /></label>
        </Show>
        <div style={{ "margin-top": "12px" }}>
          <CredentialField
            kind={kind()}
            placeholder="env.OPENAI_API_KEY, file:/path or store.name"
            taken={props.provider.keys.map((k) => k.name)}
            onToken={setValue}
            onFile={(content, email) => {
              setFileContent(content);
              setValue("uploaded");
              if (!name()) setName(seatNameFrom(email, "seat-2"));
            }}
            onSeats={(list) => {
              setSeats(list);
              setValue(list.length ? "uploaded" : "");
              if (list.length === 1) { setFileContent(list[0].content); setName(list[0].name); }
            }}
          />
        </div>
        <Show when={kind() === "plain"}>
          <div class="field-row" style={{ "margin-top": "12px" }}>
            <Show when={props.tenants.length}>
              <label>Service <span class="optional">Optional</span>
                <select value={tenant()} onChange={(e) => setTenant(e.currentTarget.value)}>
                  <option value="">Unassigned</option>
                  <For each={props.tenants}>{(t) => <option value={t}>{t}</option>}</For>
                </select>
              </label>
            </Show>
            <label>Requests / min <span class="optional">Optional</span><input type="number" min="1" value={rpm()} onInput={(e) => setRpm(e.currentTarget.value)} /></label>
            <label>Tokens / min <span class="optional">Optional</span><input type="number" min="1" value={tpm()} onInput={(e) => setTpm(e.currentTarget.value)} /></label>
          </div>
        </Show>
        <div class="dialog-actions" style={{ "margin-top": "14px" }}>
          <button type="button" class="button outline" onClick={() => setOpen(false)}>Cancel</button>
          <button class="button primary" disabled={pending() || (seats().length <= 1 && !name()) || !value()}>
            {pending() ? "Adding…" : seats().length > 1 ? `Add ${seats().length} seats` : "Add credential"}
          </button>
        </div>
      </form>
    </Show>
  </div>;
}
/// Routing groups: the model id a caller sends, and the two weighted
/// pools it dispatches over.
///
/// A group is a split, not a chain. Everything in the primary pool serves
/// live traffic in proportion to its weight — that is how you send 80% of
/// a workload to one provider and 20% to another — and the fallback pool
/// is the reserve, untouched until the primary pool has nothing left to
/// try. So the editor asks for weights rather than an order: the order
/// within a pool is a consequence of the weights, not something to drag
/// into place.
function Routing(props: { refresh: () => number }) {
  const [routes, { refetch }] = createResource(props.refresh, api.routes);
  const [providers] = createResource(props.refresh, api.providers);
  const [editing, setEditing] = createSignal<RouteGroup | null>(null);
  const [search, setSearch] = createSignal("");
  const [error, setError] = createSignal("");

  const targetOptions = createMemo<Option[]>(() => {
    const out: Option[] = [];
    for (const provider of providers()?.data ?? []) {
      const models = new Set<string>();
      for (const key of provider.keys) for (const m of key.models ?? []) models.add(m);
      for (const m of models) out.push({ value: `${provider.name}/${m}`, label: `${provider.name}/${m}`, hint: provider.kind });
    }
    return out;
  });
  const shown = createMemo(() => {
    const needle = search().trim().toLowerCase();
    const all = routes()?.data ?? [];
    return needle
      ? all.filter((r) => `${r.name} ${allTargets(r).join(" ")}`.toLowerCase().includes(needle))
      : all;
  });

  return <div class="stack-lg">
    <FilterBar
      search={search()}
      onSearch={setSearch}
      searchPlaceholder="Search groups (press /)"
      filters={[]}
      extra={<button class="button primary" onClick={() => setEditing({ name: "", primary: [], fallback: [] })}><Plus size={15} />New group</button>}
    />
    <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
    <section class="panel">
      <SectionTitle title="Routing groups" subtitle="One model id for callers; traffic splits across the primary pool by weight, and falls back when it runs out" />
      <Loading when={routes.loading && !routes()} skeleton="table"><Show when={shown().length} fallback={<Empty title="No routing groups" action="A group lets callers ask for `fast` and have the traffic split across providers." />}>
        <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table"><table>
          <thead><tr><th>Group</th><th>Primary split</th><th>Fallback</th><th><span class="sr-only">Actions</span></th></tr></thead>
          <tbody><For each={shown()}>{(group) => <tr class="clickable" onClick={() => setEditing(cloneGroup(group))}>
            <td class="strong mono">{group.name}</td>
            <td><PoolPills pool={group.primary} accent /></td>
            <td><PoolPills pool={group.fallback} /></td>
            <td class="actions">
              <button class="icon-button danger" title={`Delete ${group.name}`} aria-label={`Delete ${group.name}`} onClick={async (e) => {
                e.stopPropagation();
                if (!confirm(`Delete routing group ${group.name}?`)) return;
                setError("");
                try { await api.deleteRoute(group.name); await refetch(); }
                catch (err) { setError(err instanceof Error ? err.message : "Delete failed"); }
              }}><Trash2 size={14} /></button>
            </td>
          </tr>}</For></tbody>
        </table></div>
      </Show>
      </Loading>
    </section>
    <Show when={editing()} keyed>{(group) => (
      <RouteDialog
        group={group}
        options={targetOptions()}
        onClose={() => setEditing(null)}
        onDone={async () => { setEditing(null); await refetch(); }}
      />
    )}</Show>
  </div>;
}

/// Every model a group can reach, primary first — for search and for the
/// pages that only care which models are covered.
function allTargets(group: RouteGroup): string[] {
  return [...group.primary, ...group.fallback].map((t) => t.target);
}

function cloneGroup(group: RouteGroup): RouteGroup {
  return { name: group.name, primary: group.primary.map((t) => ({ ...t })), fallback: group.fallback.map((t) => ({ ...t })) };
}

/// A pool's share of traffic, per member. Weights are ratios, so the
/// number an operator typed is only meaningful next to its siblings —
/// the percentage is what they actually wanted to know.
function shares(pool: RouteTarget[]): number[] {
  const total = pool.reduce((sum, t) => sum + (t.weight > 0 ? t.weight : 0), 0);
  return pool.map((t) => (total > 0 ? (t.weight > 0 ? t.weight : 0) / total : 0));
}

function percent(share: number): string {
  const value = share * 100;
  // Two models at 1:2 are 33.3/66.7, and rounding both to whole numbers
  // loses the point; a clean split should still read as "50%".
  return `${value >= 10 || value === 0 ? Math.round(value) : value.toFixed(1)}%`;
}

function PoolPills(props: { pool: RouteTarget[]; accent?: boolean }) {
  const split = createMemo(() => shares(props.pool));
  return <Show when={props.pool.length} fallback={<span class="muted">—</span>}>
    <div class="route-chain">
      <For each={props.pool}>{(entry, index) => (
        <span class="pill" classList={{ accent: props.accent }} title={`weight ${entry.weight}`}>
          {entry.target}<span class="route-share">{percent(split()[index()])}</span>
        </span>
      )}</For>
    </div>
  </Show>;
}

function RouteDialog(props: { group: RouteGroup; options: Option[]; onClose: () => void; onDone: () => void }) {
  escapeCloses(props.onClose);
  const [name, setName] = createSignal(props.group.name);
  const [primary, setPrimary] = createSignal<RouteTarget[]>(props.group.primary);
  const [fallback, setFallback] = createSignal<RouteTarget[]>(props.group.fallback);
  const [error, setError] = createSignal("");
  const [pending, setPending] = createSignal(false);

  const invalid = createMemo(() =>
    [...primary(), ...fallback()].some((t) => !Number.isFinite(t.weight) || t.weight <= 0));

  return <div class="dialog-backdrop" role="presentation" onMouseDown={(e) => { if (e.target === e.currentTarget) props.onClose(); }}>
    <form class="dialog wide" role="dialog" aria-modal="true" aria-labelledby="route-title" onSubmit={async (e) => {
      e.preventDefault(); setError(""); setPending(true);
      try { await api.putRoute({ name: name(), primary: primary(), fallback: fallback() }); props.onDone(); }
      catch (err) { setError(err instanceof Error ? err.message : "Save failed"); }
      finally { setPending(false); }
    }}>
      <header class="dialog-head">
        <div><h2 id="route-title">{props.group.name ? `Edit ${props.group.name}` : "New routing group"}</h2><p class="muted">Callers send the group name as the model id.</p></div>
        <button type="button" class="icon-button" aria-label="Close" title="Close (Esc)" onClick={props.onClose}><X size={16} /></button>
      </header>
      <label>Group name
        <input required placeholder="fast" value={name()} onInput={(e) => setName(e.currentTarget.value)} />
      </label>
      <PoolEditor
        title="Primary"
        hint="Serves live traffic. Each request goes to one model, picked in proportion to its weight."
        pool={primary()}
        onChange={setPrimary}
        options={props.options}
        addLabel="Add a primary model…"
      />
      <PoolEditor
        title="Fallback"
        hint="Held in reserve. Reached only once every primary model has failed, then tried by weight."
        pool={fallback()}
        onChange={setFallback}
        options={props.options}
        addLabel="Add a fallback model…"
        empty="No fallback — a request that exhausts the primary pool fails."
      />
      <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
      <div class="dialog-actions">
        <button type="button" class="button outline" onClick={props.onClose}>Cancel</button>
        <button class="button primary" disabled={pending() || !name() || !primary().length || invalid()}>{pending() ? "Saving…" : "Save group"}</button>
      </div>
    </form>
  </div>;
}

/// One weighted pool. The share each model ends up with is shown next to
/// its weight, because a weight on its own says nothing — an operator who
/// types 3 against a sibling 1 is asking for 75%, and should be able to
/// see that before saving rather than after traffic moves.
function PoolEditor(props: {
  title: string;
  hint: string;
  pool: RouteTarget[];
  onChange: (pool: RouteTarget[]) => void;
  options: Option[];
  addLabel: string;
  empty?: string;
}) {
  const split = createMemo(() => shares(props.pool));
  // Scoped to this pool, not the group: the same model may serve live
  // traffic and be another provider's reserve, but a pool cannot hold it
  // twice — the gateway rejects that outright.
  const available = createMemo(() => {
    const held = new Set(props.pool.map((t) => t.target));
    return props.options.filter((o) => !held.has(o.value));
  });
  const setWeight = (index: number, raw: string) => {
    const next = props.pool.map((t, i) => (i === index ? { ...t, weight: Number(raw) } : t));
    props.onChange(next);
  };
  return <div>
    <div class="pool-head">
      <h3>{props.title}</h3>
      <p class="muted">{props.hint}</p>
    </div>
    <Show when={props.pool.length} fallback={<p class="muted">{props.empty ?? "No models yet."}</p>}>
      <ol class="route-editor weighted">
        <For each={props.pool}>{(entry, index) => (
          <li>
            <code class="mono">{entry.target}</code>
            <label class="weight-field">
              <span class="sr-only">{`Weight for ${entry.target}`}</span>
              <input
                type="number"
                min="0"
                step="any"
                value={entry.weight}
                aria-invalid={!(entry.weight > 0)}
                onInput={(e) => setWeight(index(), e.currentTarget.value)}
              />
            </label>
            <span class="route-share" aria-label={`${percent(split()[index()])} of ${props.title.toLowerCase()} traffic`}>
              {percent(split()[index()])}
            </span>
            <div class="route-controls">
              <button type="button" class="icon-button danger" aria-label={`Remove ${entry.target}`}
                onClick={() => props.onChange(props.pool.filter((_, i) => i !== index()))}><X size={13} /></button>
            </div>
          </li>
        )}</For>
      </ol>
    </Show>
    <div class="route-add">
      <Combobox
        value=""
        options={available()}
        onSelect={(value) => value && props.onChange([...props.pool, { target: value, weight: 1 }])}
        label={`Add to ${props.title.toLowerCase()}`}
        placeholder={props.addLabel}
      />
    </div>
  </div>;
}

function Keys(props: { refresh: () => number; bump: () => void }) {
  const [keys, { refetch }] = createResource(props.refresh, api.keys);
  const [providers, { refetch: refetchProviders }] = createResource(props.refresh, api.providers);
  const [creating, setCreating] = createSignal(false);
  const [managing, setManaging] = createSignal("");
  const [revealed, setRevealed] = createSignal("");
  const [search, setSearch] = createSignal("");
  const [name, setName] = createSignal("");
  const [models, setModels] = createSignal<string[]>([]);
  const [tenant, setTenant] = createSignal("");
  const [budget, setBudget] = createSignal("");
  const [period, setPeriod] = createSignal("monthly");
  const [rpm, setRpm] = createSignal("");
  const [tpm, setTpm] = createSignal("");
  const [advanced, setAdvanced] = createSignal(false);
  const [error, setError] = createSignal("");
  const [managingServices, setManagingServices] = createSignal(false);
  const [newService, setNewService] = createSignal("");
  const [serviceBusy, setServiceBusy] = createSignal("");

  const tenants = () => providers()?.tenants ?? [];
  // What each service holds, so the roster is not just a list of words —
  // and so the thing that blocks a deletion is visible before you try it.
  const serviceHolds = (name: string) => ({
    accounts: accountsFor(providers()?.data ?? [], name),
    keys: (keys()?.data ?? []).filter((k) => k.tenant === name).length,
  });
  const addService = async () => {
    const name = newService().trim();
    if (!name) return;
    setServiceBusy(name); setError("");
    try { await api.addTenant(name); setNewService(""); await refetchProviders(); }
    catch (err) { setError(err instanceof Error ? err.message : "Could not add the service"); }
    finally { setServiceBusy(""); }
  };
  const removeService = async (name: string) => {
    const held = serviceHolds(name);
    if (held.accounts || held.keys) {
      setError(`\u201c${name}\u201d still has ${held.accounts} account(s) and ${held.keys} key(s). Move them first.`);
      return;
    }
    setServiceBusy(name); setError("");
    try { await api.deleteTenant(name); await refetchProviders(); }
    catch (err) { setError(err instanceof Error ? err.message : "Could not remove the service"); }
    finally { setServiceBusy(""); }
  };
  const reload = async () => { await refetch(); props.bump(); };

  const modelOptions = createMemo<Option[]>(() => {
    const out: Option[] = [];
    for (const provider of providers()?.data ?? []) {
      const names = new Set<string>();
      for (const k of provider.keys) for (const m of k.models ?? []) names.add(m);
      for (const m of names) out.push({ value: `${provider.name}/${m}`, label: `${provider.name}/${m}`, hint: provider.kind });
    }
    return out;
  });
  const shown = createMemo(() => {
    const needle = search().trim().toLowerCase();
    const all = keys()?.data ?? [];
    return needle ? all.filter((k) => `${k.name} ${k.id}`.toLowerCase().includes(needle)) : all;
  });

  const reset = () => { setName(""); setModels([]); setTenant(""); setBudget(""); setRpm(""); setTpm(""); setError(""); };
  createEffect(() => {
    if (!creating()) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (document.querySelector(".combobox-popup")) return;
      setCreating(false);
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });

  return <div class="stack-lg">
    <FilterBar
      search={search()}
      onSearch={setSearch}
      searchPlaceholder="Search keys (press /)"
      filters={[]}
      extra={<>
        {/* Rare enough that it does not get permanent space on a page visited to look at keys. */}
        <button class="button outline" onClick={() => setManagingServices(true)}>Services</button>
        <button class="button primary" onClick={() => setCreating(true)}><Plus size={15} />Create key</button>
      </>}
    />
    <section class="panel">
      <SectionTitle title="Virtual keys" subtitle="Scoped credentials with limits, budgets, and immediate revocation" />
      <Loading when={keys.loading && !keys()} skeleton="table"><Show when={shown().length} fallback={<Empty title="No virtual keys" action="Create a key for an application or team." />}>
        <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table"><table><thead><tr><th>Name</th><th>Scope</th><th>Service</th><th>Accounts</th><th>Rate</th><th>Budget</th><th>Status</th><th><span class="sr-only">Actions</span></th></tr></thead><tbody>
          <For each={shown()}>{(key) => <KeyRow
            key={key}
            tenants={providers()?.tenants ?? []}
            accounts={key.tenant ? accountsFor(providers()?.data ?? [], key.tenant) : null}
            onManage={() => key.tenant && setManaging(key.tenant)}
            reload={reload}
            reveal={setRevealed}
          />}</For>
        </tbody></table></div>
      </Show>
      </Loading>
    </section>

    <Show when={creating()}>
      <div class="dialog-backdrop" role="presentation" onMouseDown={(e) => { if (e.target === e.currentTarget) setCreating(false); }}>
        <form class="dialog" role="dialog" aria-modal="true" aria-labelledby="create-key-title" onSubmit={async (e) => {
          e.preventDefault(); setError("");
          try {
            const input: Record<string, unknown> = { name: name(), models: models() };
            if (tenant()) input.tenant = tenant().trim();
            if (budget()) input.budget = { usd: Number(budget()), period: period() };
            if (rpm() || tpm()) input.rate = { ...(rpm() ? { rpm: Number(rpm()) } : {}), ...(tpm() ? { tpm: Number(tpm()) } : {}) };
            const result = await api.createKey(input);
            setRevealed(result.key); setCreating(false); reset(); await reload();
          } catch (err) { setError(err instanceof Error ? err.message : "Create failed"); }
        }}>
          <header class="dialog-head">
            <div><h2 id="create-key-title">Create virtual key</h2><p class="muted">The secret is shown once after creation.</p></div>
            <button type="button" class="icon-button" aria-label="Close" title="Close (Esc)" onClick={() => { setCreating(false); reset(); }}><X size={16} /></button>
          </header>
          <label>Name<input required value={name()} onInput={(e) => setName(e.currentTarget.value)} /></label>
          <label>Allowed models <span class="optional">Leave empty for all</span>
            <MultiCombobox
              values={models()}
              options={modelOptions()}
              onChange={setModels}
              label="Allowed models"
              emptyMeans="Every model"
            />
          </label>
          <label>Service <span class="optional">Optional</span>
            <select value={tenant()} onChange={(e) => setTenant(e.currentTarget.value)}>
              <option value="">Unassigned</option>
              <For each={providers()?.tenants ?? []}>{(t) => <option value={t}>{t}</option>}</For>
            </select>
            <small class="muted">Which service this key belongs to. It may spend the accounts labelled for that service and no others. Leave unassigned on a pool where nothing is labelled — but note that once any account in a pool is labelled, an unassigned key reaches nothing there.</small>
          </label>
          <div class="disclosure" classList={{ open: advanced() }}>
            <button type="button" class="disclosure-toggle" aria-expanded={advanced()} onClick={() => setAdvanced((v) => !v)}>
              <ChevronRight size={13} class="disclosure-chevron" aria-hidden="true" />
              <span>Budget and rate limits</span>
              <Show when={!advanced()}>
                <span class="disclosure-preview">
                  {budget() || rpm() || tpm()
                    ? [budget() && `$${budget()}/${period()}`, rpm() && `${rpm()} rpm`, tpm() && `${tpm()} tpm`].filter(Boolean).join(" · ")
                    : "None — unlimited"}
                </span>
              </Show>
            </button>
            <Show when={advanced()}>
              <div class="disclosure-body">
                <div class="field-row">
                  <label>Budget (USD) <span class="optional">Optional</span>
                    <input type="number" min="0" step="0.01" value={budget()} onInput={(e) => setBudget(e.currentTarget.value)} />
                  </label>
                  <label>Period
                    <select value={period()} onChange={(e) => setPeriod(e.currentTarget.value)}>
                      <option value="daily">Daily</option><option value="weekly">Weekly</option><option value="monthly">Monthly</option>
                    </select>
                  </label>
                </div>
                <div class="field-row">
                  <label>Requests / min <span class="optional">Optional</span>
                    <input type="number" min="1" value={rpm()} onInput={(e) => setRpm(e.currentTarget.value)} />
                  </label>
                  <label>Tokens / min <span class="optional">Optional</span>
                    <input type="number" min="1" value={tpm()} onInput={(e) => setTpm(e.currentTarget.value)} />
                  </label>
                </div>
              </div>
            </Show>
          </div>
          <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
          <div class="dialog-actions">
            <button type="button" class="button outline" onClick={() => { setCreating(false); reset(); }}>Cancel</button>
            <button class="button primary">Create key</button>
          </div>
        </form>
      </div>
    </Show>

    <Show when={managingServices()}>
      {/* A dialog, not a drawer: this is a short list and a one-field form,
          the same shape as Create key, and a full-height panel for it was
          mostly empty space. */}
      <div class="dialog-backdrop" role="presentation"
           onMouseDown={(e) => { if (e.target === e.currentTarget) { setManagingServices(false); setError(""); } }}>
        <div class="dialog" role="dialog" aria-modal="true" aria-labelledby="services-title">
          <h2 id="services-title">Services</h2>
          <p class="muted">A service owns accounts; keys name one in order to spend them</p>
          <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
          <Show when={tenants().length} fallback={
            <p class="muted">No services yet. Add one, then assign accounts and keys to it.</p>
          }>
            <ul class="account-list service-list">
              <For each={tenants()}>{(name) => {
                const held = () => serviceHolds(name);
                const inUse = () => held().accounts > 0 || held().keys > 0;
                return <li>
                  <span class="account-name">{name}</span>
                  <span class="muted">
                    {held().accounts} account{held().accounts === 1 ? "" : "s"} · {held().keys} key{held().keys === 1 ? "" : "s"}
                  </span>
                  {/* The reason sits on the row, not in a tooltip: a disabled
                      button with no visible cause reads as broken. */}
                  <Show when={inUse()} fallback={
                    <button
                      class="button outline small"
                      disabled={Boolean(serviceBusy())}
                      onClick={() => void removeService(name)}
                    >Remove</button>
                  }>
                    <span class="muted">in use</span>
                  </Show>
                </li>;
              }}</For>
            </ul>
          </Show>
          <label>Add a service
            <input
              type="text"
              value={newService()}
              placeholder="e.g. billing"
              aria-label="Add a service"
              onInput={(e) => setNewService(e.currentTarget.value)}
              onKeyDown={(e) => { if (e.key === "Enter") void addService(); }}
            />
            <small class="muted">Letters, digits, dashes and underscores</small>
          </label>
          <div class="dialog-actions">
            <button class="button ghost" onClick={() => { setManagingServices(false); setError(""); }}>Close</button>
            <button
              class="button primary"
              disabled={!newService().trim() || Boolean(serviceBusy())}
              onClick={() => void addService()}
            >Add service</button>
          </div>
        </div>
      </div>
    </Show>

    <Show when={managing()} keyed>{(service) => (
      <KeyAccounts
        service={service}
        providers={providers()?.data ?? []}
        onClose={() => setManaging("")}
        onChanged={async () => { await refetchProviders(); }}
      />
    )}</Show>

    <Show when={revealed()}>
      <div class="secret-banner" role="status">
        <div><strong>Copy this key now</strong><code>{revealed()}</code></div>
        <button class="icon-button" aria-label="Copy new virtual key" title="Copy key" onClick={() => navigator.clipboard.writeText(revealed())}><Copy size={16} /></button>
      </div>
    </Show>
  </div>;
}

/// Which accounts a key's service owns, and the two operations that change
/// it: take one away, or bring one in.
///
/// Accounts belong to a *service*, not to a key — so this manages the whole
/// service's pool, and every key of that service is affected. Said plainly
/// in the subtitle, because the drawer is opened from one key and it would
/// otherwise read as that key's private list.
function KeyAccounts(props: {
  service: string;
  providers: Provider[];
  onClose: () => void;
  onChanged: () => Promise<void>;
}) {
  const [busy, setBusy] = createSignal("");
  const [error, setError] = createSignal("");
  const [filter, setFilter] = createSignal("");

  const all = createMemo(() =>
    props.providers.flatMap((p) => p.keys.map((k) => ({ provider: p.name, key: k }))),
  );
  const label = (a: { key: ProviderKey }) => a.key.credential?.email ?? a.key.name;
  const mine = createMemo(() =>
    all().filter((a) => a.key.tenant === props.service).sort((x, y) => label(x).localeCompare(label(y))),
  );
  const matchingMine = createMemo(() => {
    const q = filter().trim().toLowerCase();
    return q
      ? mine().filter((a) => label(a).toLowerCase().includes(q) || a.provider.toLowerCase().includes(q))
      : mine();
  });
  // No cap: the list scrolls, so slicing it only hid rows behind a
  // sentence telling you to type. Scrolling is the cheaper gesture.
  const shownMine = matchingMine;

  // Candidates are ranked by what taking one costs: unassigned accounts are
  // free, and one held by another service is a transfer out of that service.
  const candidates = createMemo(() => {
    const q = filter().trim().toLowerCase();
    return all()
      .filter((a) => a.key.tenant !== props.service)
      .filter((a) => !q || label(a).toLowerCase().includes(q) || a.provider.toLowerCase().includes(q))
      .sort((x, y) =>
        Number(Boolean(x.key.tenant)) - Number(Boolean(y.key.tenant))
        || label(x).localeCompare(label(y)));
  });

  const byProvider = createMemo(() => {
    const counts = new Map<string, number>();
    for (const a of mine()) counts.set(a.provider, (counts.get(a.provider) ?? 0) + 1);
    return [...counts].sort();
  });
  const unhealthy = createMemo(() => mine().filter((a) => (a.key.health ?? "") !== "healthy").length);

  const assign = async (provider: string, account: string, tenant: string | null) => {
    // Dividing a pool for the first time is the one irreversible-feeling
    // click here. From that moment only keys naming a service can use the
    // provider, and anything still on a plain gateway key is refused — so
    // ask once, while there is still nothing to undo.
    const pool = props.providers.find((p) => p.name === provider);
    if (tenant !== null && pool && !pool.managed) {
      const ok = confirm(
        `Give ${account} to "${tenant}"?\n\n`
        + `Nothing on ${provider} is assigned yet. Assigning the first account divides `
        + `the pool: from then on only keys naming a service can use it, every account `
        + `left unassigned serves nobody, and any caller without a service is refused.\n\n`
        + `Assign the rest of ${provider} too, in the same sitting.`,
      );
      if (!ok) return;
    }
    setBusy(`${provider}/${account}`); setError("");
    try { await api.setAccountTenant(provider, account, tenant); await props.onChanged(); }
    catch (err) { setError(err instanceof Error ? err.message : "Failed"); }
    finally { setBusy(""); }
  };

  return <Drawer
    open
    title={`Accounts for ${props.service}`}
    subtitle="Accounts belong to the service, so every key of this service shares them"
    onClose={props.onClose}
  >
    <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>

    {/* What this service holds, in one line, because the count alone does not
        answer "can it actually serve" — a pool of ten with eight benched cannot. */}
    <div class="account-summary">
      <div>
        <strong>{mine().length}</strong>
        <span class="muted">{mine().length === 1 ? " account" : " accounts"}</span>
      </div>
      <For each={byProvider()}>{([name, n]) => <span class="pill">{name} · {n}</span>}</For>
      <Show when={unhealthy()}>
        <span class="pill warning">{unhealthy()} not ready</span>
      </Show>
    </div>

    <div class="drawer-section">
      <Show when={mine().length} fallback={
        <Empty title="No accounts yet" action="Until this service is given one, every request it makes is refused." />
      }>
        {/* One search over both lists. With 107 accounts on a service,
            listing them all is a scroll nobody reads — you come here knowing
            which account you want. */}
        <input
          class="account-search"
          type="search"
          placeholder={`Search ${mine().length} account${mine().length === 1 ? "" : "s"}…`}
          value={filter()}
          onInput={(e) => setFilter(e.currentTarget.value)}
          aria-label="Search this service's accounts"
        />
        <ul class="account-list">
          <For each={shownMine()}>{(a) => <li>
            <span class="account-name">{label(a)}</span>
            <span class="muted">{a.provider}</span>
            <span classList={{ dot: true, ok: (a.key.health ?? "") === "healthy" }}
                  title={a.key.status ?? a.key.health ?? "unknown"} />
            {/* "Release", not a bin: the account is not deleted, it goes back
                to being unassigned and can be given to anyone. */}
            <button
              class="button outline small"
              disabled={busy() === `${a.provider}/${a.key.name}`}
              title={`Release ${label(a)} from ${props.service}`}
              onClick={() => assign(a.provider, a.key.name, null)}
            >Release</button>
          </li>}</For>
        </ul>

      </Show>
    </div>

    <div class="drawer-section">
      <SectionTitle
        title="Add an account"
        subtitle="Taking one from another service moves it — nothing is copied"
      />
      {/* No second search box: the one above filters both lists, and a
          <select> of 117 accounts is a scroll rather than a choice. */}
      <Show when={candidates().length} fallback={<p class="muted">No account matches.</p>}>
        <ul class="account-list candidates">
          <For each={candidates()}>{(a) => <li>
            <span class="account-name">{label(a)}</span>
            <span class="muted">{a.provider}</span>
            <Show when={a.key.tenant} fallback={<span class="pill">unassigned</span>}>
              <span class="pill warning">{`takes from ${a.key.tenant}`}</span>
            </Show>
            <button
              class="button outline small"
              disabled={Boolean(busy())}
              onClick={() => assign(a.provider, a.key.name, props.service)}
            >Add</button>
          </li>}</For>
        </ul>

      </Show>
    </div>
  </Drawer>;
}

/// How many accounts one service owns, across every pool.
function accountsFor(providers: Provider[], service: string): number {
  return providers.reduce(
    (total, p) => total + p.keys.filter((k) => k.tenant === service).length,
    0,
  );
}

function KeyRow(props: { key: VirtualKey; tenants: string[]; accounts: number | null; onManage: () => void; reload: () => Promise<void>; reveal: (value: string) => void }) {
  return <tr><td><strong>{props.key.name}</strong><small class="mono">{props.key.id}</small></td><td>{props.key.models.length ? props.key.models.join(", ") : "All models"}</td><td><ServicePicker value={props.key.tenant ?? null} tenants={props.tenants} label={`Service for ${props.key.name}`} onChange={async (tenant) => { await api.updateKey(props.key.id, { tenant }); await props.reload(); }} /></td><td class="accounts-cell">{props.key.tenant ? <button class="button outline small" onClick={props.onManage}>{`${props.accounts ?? 0} account${props.accounts === 1 ? "" : "s"}`}</button> : <span class="muted">—</span>}</td><td>{props.key.rate?.rpm ? `${props.key.rate.rpm} RPM` : "Unlimited"}</td><td>{props.key.budget ? `${formatUsd(props.key.budget.usd)} / ${props.key.budget.period}` : "None"}</td><td><Status text={props.key.enabled ? "Active" : "Revoked"} tone={props.key.enabled ? "success" : "muted"} /></td><td class="actions"><button class="icon-button" title={`Rotate ${props.key.name}`} aria-label={`Rotate ${props.key.name}`} onClick={async () => { const result = await api.rotateKey(props.key.id); props.reveal(result.key); await props.reload(); }}><RefreshCw size={16} /></button><button class="icon-button danger" title={`Delete ${props.key.name}`} aria-label={`Delete ${props.key.name}`} onClick={async () => { if (confirm(`Delete ${props.key.name}?`)) { await api.deleteKey(props.key.id); await props.reload(); } }}><Trash2 size={16} /></button></td></tr>;
}

type TrendPoint = [number, number];
type Slice = {
  name: string;
  requests: number;
  failed: number;
  input: number;
  output: number;
  cost: number;
  /// Total milliseconds, not a mean: a mean cannot be re-totalled, and
  /// this is summed across days before anything divides it.
  latency: number;
};

/// One model's average latency over time, ready for the chart.
type LatencySeries = { name: string; points: TrendPoint[] };

/// One loader for both observability pages.
///
/// Sub-day ranges read the recent-request tail and filter client-side —
/// full cross-filtering at minute resolution, bounded by the tail's
/// depth, and the bound is *reported*, not hidden. Day ranges read the
/// flushed history with the same filters applied server-side against raw
/// records, so any filter composes with any grouping.
function useTelemetry(
  refresh: () => number,
  range: () => TimeRange,
  filters: () => DimFilters,
) {
  const resolved = createMemo(() => resolveRange(range()));
  const dep = () => [refresh(), range(), filters()] as const;
  // The window and the filters go to the gateway, not just into the
  // memo below: `/requests` with no `since_ms` defaults to the last
  // hour, so an unbounded read made "Last 24 hours" mean "last hour",
  // and filtering client-side only narrowed that hour further.
  // Sub-day windows are aggregated by the gateway, not derived from a
  // page of records here. Deriving them meant every figure on this page
  // was really "of the most recent 1,000 requests", so on a busy gateway
  // "Last hour" and "Last 24 hours" reported the same number and neither
  // was the answer. One scan server-side also covers restarts, which an
  // in-memory aggregate would not — and deploys restart the box.
  const [live] = createResource(dep, () => {
    const r = resolveRange(range());
    if (!r.live) return undefined;
    const f = filters();
    return api.usageSummary({
      since_ms: Math.floor(r.startMs),
      until_ms: Math.ceil(r.endMs),
      provider: f.provider || undefined,
      model: f.model || undefined,
      key: f.key || undefined,
    });
  });
  // One request for every grouping this page draws. It used to be three
  // — by model, by key, by provider — which were three identical walks
  // of the same window, fired together on load and again on every
  // refresh. The gateway has all three dimensions on each row, so the
  // split is arithmetic it can do once.
  const [history] = createResource(dep, () => {
    const r = resolveRange(range());
    const f = filters();
    return r.live ? undefined : api.historyAll(r.days, {
      provider: f.provider || undefined,
      model: f.model || undefined,
      key: f.key || undefined,
    });
  });
  const grouping = (by: string) => history()?.data?.[by];

  const sliceDays = (series: Record<string, DayBucket[]> | undefined) => {
    if (!series) return {};
    const r = resolved();
    const from = new Date(r.startMs).toISOString().slice(0, 10);
    const to = new Date(r.endMs).toISOString().slice(0, 10);
    const out: Record<string, DayBucket[]> = {};
    for (const [name, buckets] of Object.entries(series)) {
      const kept = buckets.filter((b) => b.day >= from && b.day <= to);
      if (kept.length) out[name] = kept;
    }
    return out;
  };

  const slicesOf = (series: Record<string, DayBucket[]>): Slice[] =>
    Object.entries(series).map(([name, buckets]) => ({
      name,
      requests: buckets.reduce((n, b) => n + b.requests, 0),
      failed: buckets.reduce((n, b) => n + b.failed, 0),
      input: buckets.reduce((n, b) => n + b.input_tokens, 0),
      output: buckets.reduce((n, b) => n + b.output_tokens, 0),
      cost: buckets.reduce((n, b) => n + b.cost_micro_usd, 0) / 1e6,
      latency: buckets.reduce((n, b) => n + (b.latency_ms_sum ?? 0), 0),
    }));

  const liveSlices = (pick: (s: UsageSummary) => UsageSlice[]): Slice[] =>
    (live() ? pick(live()!) : []).map((s) => ({
      name: s.name,
      requests: s.requests,
      failed: s.failed,
      input: s.input_tokens,
      output: s.output_tokens,
      cost: s.cost_micro_usd / 1e6,
      latency: s.latency_ms_sum ?? 0,
    }));

  const byModel = createMemo<Slice[]>(() => resolved().live
    ? liveSlices((s) => s.by_model)
    : slicesOf(sliceDays(grouping("model"))));
  const byKey = createMemo<Slice[]>(() => resolved().live
    ? liveSlices((s) => s.by_key)
    : slicesOf(sliceDays(grouping("key"))));
  const byProvider = createMemo<Slice[]>(() => resolved().live
    ? liveSlices((s) => s.by_provider)
    : slicesOf(sliceDays(grouping("provider"))));

  /// Time series for the charts: minute buckets from the tail, or day
  /// buckets from history, projected by `value`.
  type Totals = { requests: number; failed: number; input: number; output: number; cost: number; latency: number };
  const trend = (value: (b: Totals) => number) =>
    createMemo<TrendPoint[]>(() => {
      const r = resolved();
      if (r.live) {
        return (live()?.series ?? []).map((b) => [b.ts, value({
          requests: b.requests,
          failed: b.failed,
          input: b.input_tokens,
          output: b.output_tokens,
          cost: b.cost_micro_usd / 1e6,
          latency: b.latency_ms_sum ?? 0,
        })] as TrendPoint);
      }
      const perDay = new Map<string, Totals>();
      // The total, not a sum over the model split: a request that never
      // reached a provider has no model, and summing the split would
      // quietly drop exactly the failures worth charting.
      for (const buckets of Object.values(sliceDays(grouping("")))) {
        for (const bucket of buckets) {
          const b = perDay.get(bucket.day) ?? { requests: 0, failed: 0, input: 0, output: 0, cost: 0, latency: 0 };
          b.requests += bucket.requests;
          b.failed += bucket.failed;
          b.input += bucket.input_tokens;
          b.output += bucket.output_tokens;
          b.cost += bucket.cost_micro_usd / 1e6;
          b.latency += bucket.latency_ms_sum ?? 0;
          perDay.set(bucket.day, b);
        }
      }
      return [...perDay.entries()]
        .sort((a, b) => (a[0] < b[0] ? -1 : 1))
        .map(([day, b]) => [Date.parse(`${day}T00:00:00`) / 1000, value(b)]);
    });

  /// The window's latency, as the three figures that answer different
  /// questions: what a typical request cost, what the slow tail cost,
  /// and what the window cost on average.
  ///
  /// The percentiles come from the gateway either way — a p95 cannot be
  /// derived from a mean, and averaging per-day means would weight a
  /// quiet Sunday the same as a busy Monday. The mean is a total over a
  /// count for the same reason.
  const latency = createMemo(() => {
    const source = resolved().live ? live() : history();
    const total = byModel().reduce(
      (acc, s) => ({ requests: acc.requests + s.requests, ms: acc.ms + s.latency }),
      { requests: 0, ms: 0 },
    );
    return {
      p50: source?.p50_latency_ms ?? 0,
      p95: source?.p95_latency_ms ?? 0,
      avg: total.requests ? total.ms / total.requests : 0,
    };
  });

  /// Average latency per model over time, capped at the few busiest so
  /// the chart stays readable. The gateway caps the live path; the
  /// history path is capped here, over the same rule.
  const latencyByModel = createMemo<LatencySeries[]>(() => {
    if (resolved().live) {
      return (live()?.latency_by_model ?? []).map((series) => ({
        name: series.name,
        points: series.points
          .filter((p) => p.requests)
          .map((p) => [p.ts, p.latency_ms_sum / p.requests] as TrendPoint),
      }));
    }
    return Object.entries(sliceDays(grouping("model")))
      .map(([name, buckets]) => ({
        name,
        requests: buckets.reduce((n, b) => n + b.requests, 0),
        points: buckets
          .filter((b) => b.requests)
          .map((b) => [
            Date.parse(`${b.day}T00:00:00`) / 1000,
            (b.latency_ms_sum ?? 0) / b.requests,
          ] as TrendPoint),
      }))
      .sort((a, b) => b.requests - a.requests || (a.name < b.name ? -1 : 1))
      .slice(0, LATENCY_SERIES_CAP)
      .map(({ name, points }) => ({ name, points }));
  });

  const truncated = createMemo(() => resolved().live && Boolean(live()?.capped));
  // "Still fetching" and "nothing to show" are different answers, and an
  // empty state shown during the first read is the wrong one.
  const loading = createMemo(() => resolved().live
    ? live.loading && !live()
    : history.loading && !history());
  return { resolved, byModel, byKey, byProvider, trend, latency, latencyByModel, truncated, loading, live };
}

/// How many models the per-model latency chart draws — the palette's
/// length, and about where a line chart stops being readable.
const LATENCY_SERIES_CAP = 6;

/// Shared filter row for the observability pages.
function TelemetryFilters(props: {
  filters: DimFilters;
  onChange: (next: DimFilters) => void;
  range: TimeRange;
  onRange: (value: TimeRange) => void;
  refresh: () => number;
}) {
  const [providers] = createResource(props.refresh, api.providers);
  const [keys] = createResource(props.refresh, api.keys);
  const modelOptions = createMemo<Option[]>(() => {
    const out = new Set<string>();
    for (const provider of providers()?.data ?? []) {
      for (const key of provider.keys) for (const m of key.models ?? []) out.add(m);
    }
    return [...out].sort().map((m) => ({ value: m, label: m }));
  });
  return <FilterBar
    search=""
    onSearch={() => {}}
    searchPlaceholder=""
    filters={[
      {
        id: "provider",
        label: "Provider",
        value: props.filters.provider,
        onChange: (v) => props.onChange({ ...props.filters, provider: v }),
        options: (providers()?.data ?? []).map((p) => ({ value: p.name, label: p.name })),
      },
      {
        id: "model",
        label: "Model",
        value: props.filters.model,
        onChange: (v) => props.onChange({ ...props.filters, model: v }),
        options: modelOptions(),
      },
      {
        id: "key",
        label: "Virtual key",
        value: props.filters.key,
        onChange: (v) => props.onChange({ ...props.filters, key: v }),
        options: (keys()?.data ?? []).map((k) => ({ value: k.id, label: k.name, hint: k.id })),
      },
    ]}
    extra={<RangePicker value={props.range} onChange={props.onRange} />}
  />;
}

/// Multi-series line chart over explicit [seconds, value] points, minute
/// or day resolution alike, axis pinned to the selected range.
function TrendChart(props: {
  series: Array<{ name: string; points: TrendPoint[] }>;
  span: { startMs: number; endMs: number };
  /// What the y axis counts. Drives the tick labels and the gutter they
  /// need — uPlot sizes that from a default rather than from the
  /// formatter, so a wider label has to be declared here or it clips.
  unit?: "count" | "usd" | "ms";
  loading?: boolean;
}) {
  let element!: HTMLDivElement;
  let plot: uPlot | undefined;
  const palette = ["--series-1", "--series-2", "--series-3", "--series-4", "--series-5", "--series-6"];
  const hasData = createMemo(() => props.series.some((s) => s.points.length));
  createEffect(() => {
    const series = props.series;
    plot?.destroy();
    plot = undefined;
    if (!hasData() || !element) return;
    const xs = [...new Set(series.flatMap((s) => s.points.map(([t]) => t)))].sort((a, b) => a - b);
    const columns = series.map((s) => {
      const map = new Map(s.points);
      return xs.map((x) => map.get(x) ?? 0);
    });
    const ink = getComputedStyle(document.documentElement);
    plot = new uPlot({
      width: element.clientWidth || 720,
      height: 220,
      padding: [10, 8, 0, 4],
      legend: { show: false },
      cursor: { drag: { x: false, y: false } },
      scales: { x: { time: true, range: [props.span.startMs / 1000, props.span.endMs / 1000] } },
      axes: [
        { stroke: ink.getPropertyValue("--muted"), grid: { stroke: ink.getPropertyValue("--grid") } },
        {
          stroke: ink.getPropertyValue("--muted"),
          grid: { stroke: ink.getPropertyValue("--grid") },
          // Wide enough for the longest label this axis can produce:
          // uPlot reserves the gutter from a default, not from the
          // formatter, so "$80.00" was being clipped at the left edge.
          size: props.unit === "usd" ? 68 : props.unit === "ms" ? 64 : 56,
          values: (_u, splits) => splits.map((v) =>
            props.unit === "usd" ? `$${Number(v).toFixed(v < 10 ? 2 : 0)}`
            : props.unit === "ms" ? formatLatency(Number(v))
            : formatNumber(Number(v))),
        },
      ],
      series: [
        {},
        ...series.map((s, i) => ({
          label: s.name,
          stroke: ink.getPropertyValue(palette[i % palette.length]),
          width: 2,
          points: { show: xs.length < 30 },
          fill: series.length === 1
            ? `color-mix(in srgb, ${ink.getPropertyValue(palette[0]).trim()} 12%, transparent)`
            : undefined,
        })),
      ],
    }, [xs, ...columns], element);
    onCleanup(() => plot?.destroy());
  });
  return <Loading when={Boolean(props.loading)} skeleton="chart">
    <Show when={hasData()} fallback={<Empty title="Nothing in this range" action="Widen the range or clear a filter." />}>
    <div class="chart" ref={element} />
    <Show when={props.series.length > 1}>
      <div class="legend">
        <For each={props.series}>{(s, i) => (
          <div><i style={{ background: `var(${palette[i() % palette.length]})` }} />{s.name}</div>
        )}</For>
      </div>
    </Show>
  </Show>
  </Loading>;
}

/// Usage: what the providers meter — volume and tokens.
function Usage(props: { refresh: () => number }) {
  const [range, setRange] = urlRange({ kind: "relative", seconds: 86400, label: "Last 24 hours" });
  const [filters, setFilters] = urlFilters();
  const t = useTelemetry(props.refresh, range, filters);
  const totals = createMemo(() => t.byModel().reduce((acc, s) => ({
    requests: acc.requests + s.requests, failed: acc.failed + s.failed,
    input: acc.input + s.input, output: acc.output + s.output,
  }), { requests: 0, failed: 0, input: 0, output: 0 }));
  const inputTrend = t.trend((b) => b.input);
  const outputTrend = t.trend((b) => b.output);
  const requestTrend = t.trend((b) => b.requests);
  const failedTrend = t.trend((b) => b.failed);
  // Per bucket, not per window: dividing the window's total by the
  // window's requests and drawing that flat line would say nothing.
  const latencyTrend = t.trend((b) => (b.requests ? b.latency / b.requests : 0));

  return <div class="stack-lg">
    <TelemetryFilters filters={filters()} onChange={setFilters} range={range()} onRange={setRange} refresh={props.refresh} />
    <Loading when={t.loading()} skeleton="stats" rows={6}>
    <section class="stat-row" aria-label="Usage totals">
      <div class="stat"><span>Requests</span><strong>{formatNumber(totals().requests)}</strong></div>
      <div class="stat"><span>Success rate</span><strong>{totals().requests ? `${(((totals().requests - totals().failed) / totals().requests) * 100).toFixed(totals().failed ? 1 : 0)}%` : "—"}</strong></div>
      <div class="stat"><span>Input tokens</span><strong>{formatNumber(totals().input)}</strong></div>
      <div class="stat"><span>Output tokens</span><strong>{formatNumber(totals().output)}</strong></div>
      <div class="stat"><span>Total tokens</span><strong>{formatNumber(totals().input + totals().output)}</strong></div>
      <div class="stat"><span>Tokens / request</span><strong>{totals().requests ? formatNumber((totals().input + totals().output) / totals().requests) : "—"}</strong></div>
    </section>
    <section class="stat-row" aria-label="Latency">
      <div class="stat">
        <span>Median latency</span>
        <strong>{totals().requests ? formatLatency(t.latency().p50) : "—"}</strong>
        <small>half of requests were faster</small>
      </div>
      <div class="stat">
        <span>p95 latency</span>
        <strong>{totals().requests ? formatLatency(t.latency().p95) : "—"}</strong>
        <small>the slow twentieth</small>
      </div>
      <div class="stat">
        <span>Average latency</span>
        <strong>{totals().requests ? formatLatency(t.latency().avg) : "—"}</strong>
        <small>end to end, including the provider</small>
      </div>
    </section>
    </Loading>
    <Show when={t.truncated()}>
      <p class="muted">This window holds more requests than one scan covers; the figures above are a floor. Narrow the range or a filter for exact numbers.</p>
    </Show>
    <section class="flat-section">
      <header><h2>Tokens over time</h2><span class="muted">{t.resolved().label} · input vs output</span></header>
      <TrendChart
        series={[{ name: "Input", points: inputTrend() }, { name: "Output", points: outputTrend() }]}
        span={{ startMs: t.resolved().startMs, endMs: t.resolved().endMs }}
        loading={t.loading()}
      />
    </section>
    <div class="two-up">
      <section class="flat-section">
        <header><h2>Requests over time</h2></header>
        <TrendChart series={[{ name: "Requests", points: requestTrend() }]} span={{ startMs: t.resolved().startMs, endMs: t.resolved().endMs }} loading={t.loading()} />
      </section>
      <section class="flat-section">
        <header><h2>Failures over time</h2></header>
        <TrendChart series={[{ name: "Failed", points: failedTrend() }]} span={{ startMs: t.resolved().startMs, endMs: t.resolved().endMs }} loading={t.loading()} />
      </section>
    </div>
    <div class="two-up">
      <section class="flat-section">
        <header><h2>Latency over time</h2><span class="muted">mean per bucket</span></header>
        <TrendChart
          series={[{ name: "Latency", points: latencyTrend() }]}
          span={{ startMs: t.resolved().startMs, endMs: t.resolved().endMs }}
          unit="ms"
          loading={t.loading()}
        />
      </section>
      <section class="flat-section">
        {/* Split out because the combined line cannot answer "which one
            got slower": one slow model is invisible inside a mean that a
            fast model serving ten times the traffic dominates. */}
        <header><h2>Latency per model</h2><span class="muted">busiest {LATENCY_SERIES_CAP}, mean</span></header>
        <TrendChart
          series={t.latencyByModel()}
          span={{ startMs: t.resolved().startMs, endMs: t.resolved().endMs }}
          unit="ms"
          loading={t.loading()}
        />
      </section>
    </div>
    <div class="two-up">
      <section class="flat-section">
        <header><h2>Per model</h2><span class="muted">by tokens</span></header>
        <TokenTable slices={t.byModel()} label="Model" loading={t.loading()} />
      </section>
      <section class="flat-section">
        <header><h2>Per virtual key</h2><span class="muted">by tokens</span></header>
        <TokenTable slices={t.byKey()} label="Key" loading={t.loading()} />
      </section>
    </div>
  </div>;
}

/// Cost: what the pricing produces — spend, efficiency, and where the
/// money concentrates.
function Cost(props: { refresh: () => number }) {
  const [range, setRange] = urlRange({ kind: "relative", seconds: 7 * 86400, label: "Last 7 days" });
  const [filters, setFilters] = urlFilters();
  const t = useTelemetry(props.refresh, range, filters);
  const totals = createMemo(() => t.byModel().reduce((acc, s) => ({
    requests: acc.requests + s.requests,
    tokens: acc.tokens + s.input + s.output,
    cost: acc.cost + s.cost,
    failedCost: acc.failedCost + (s.requests ? (s.cost * s.failed) / s.requests : 0),
  }), { requests: 0, tokens: 0, cost: 0, failedCost: 0 }));
  const spendTrend = t.trend((b) => b.cost);
  const cumulative = createMemo<TrendPoint[]>(() => {
    let sum = 0;
    return spendTrend().map(([ts, v]) => { sum += v; return [ts, sum] as TrendPoint; });
  });

  return <div class="stack-lg">
    <TelemetryFilters filters={filters()} onChange={setFilters} range={range()} onRange={setRange} refresh={props.refresh} />
    <Loading when={t.loading()} skeleton="stats" rows={4}>
    <section class="stat-row" aria-label="Cost totals">
      <div class="stat"><span>Total spend</span><strong>{formatUsd(totals().cost)}</strong></div>
      <div class="stat"><span>Avg cost / request</span><strong>{formatUsd(totals().requests ? totals().cost / totals().requests : 0)}</strong></div>
      <div class="stat"><span>Cost / 1M tokens</span><strong>{totals().tokens ? formatUsd((totals().cost / totals().tokens) * 1e6) : "—"}</strong></div>
      <div class="stat"><span>Top model share</span><strong>{(() => {
        const rows = [...t.byModel()].sort((a, b) => b.cost - a.cost);
        return totals().cost && rows[0] ? `${Math.round((rows[0].cost / totals().cost) * 100)}%` : "—";
      })()}</strong></div>
    </section>
    </Loading>
    <Show when={t.truncated()}>
      <p class="muted">This window holds more requests than one scan covers; the figures above are a floor. Narrow the range or a filter for exact numbers.</p>
    </Show>
    <div class="two-up">
      <section class="flat-section">
        <header><h2>Spend over time</h2><span class="muted">{t.resolved().label}</span></header>
        <TrendChart series={[{ name: "Spend", points: spendTrend() }]} span={{ startMs: t.resolved().startMs, endMs: t.resolved().endMs }} unit="usd" loading={t.loading()} />
      </section>
      <section class="flat-section">
        <header><h2>Cumulative spend</h2><span class="muted">running total</span></header>
        <TrendChart series={[{ name: "Cumulative", points: cumulative() }]} span={{ startMs: t.resolved().startMs, endMs: t.resolved().endMs }} unit="usd" loading={t.loading()} />
      </section>
    </div>
    <section class="flat-section">
      <header><h2>Per model</h2><span class="muted">spend, and what a token costs there</span></header>
      <CostTable slices={t.byModel()} label="Model" loading={t.loading()} />
    </section>
    <div class="two-up">
      <section class="flat-section">
        <header><h2>Per provider</h2></header>
        <CostTable slices={t.byProvider()} label="Provider" loading={t.loading()} />
      </section>
      <section class="flat-section">
        <header><h2>Per virtual key</h2></header>
        <CostTable slices={t.byKey()} label="Key" loading={t.loading()} />
      </section>
    </div>
  </div>;
}

function TokenTable(props: { slices: Slice[]; label: string; loading?: boolean }) {
  const rows = createMemo(() => [...props.slices].sort((a, b) => (b.input + b.output) - (a.input + a.output)));
  const max = createMemo(() => (rows()[0] ? rows()[0].input + rows()[0].output : 0));
  return <Loading when={Boolean(props.loading)} skeleton="table"><Show when={rows().length} fallback={<Empty title="No traffic" action="Nothing served in this range." />}>
    <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table">
      <table class="dense">
        <thead><tr><th>{props.label}</th><th class="num">Requests</th><th class="num">Input</th><th class="num">Output</th><th class="num">Tok / req</th><th class="num">Avg latency</th><th class="share-col"><span class="sr-only">Share</span></th></tr></thead>
        <tbody><For each={rows().slice(0, 8)}>{(row) => <tr>
          <td class="strong mono">{row.name}</td>
          <td class="num">{formatNumber(row.requests)}</td>
          <td class="num">{formatNumber(row.input)}</td>
          <td class="num">{formatNumber(row.output)}</td>
          <td class="num">{row.requests ? formatNumber((row.input + row.output) / row.requests) : "—"}</td>
          <td class="num">{row.requests ? formatLatency(row.latency / row.requests) : "—"}</td>
          <td class="share-col"><div class="meter"><i style={{ width: `${max() ? Math.round(((row.input + row.output) / max()) * 100) : 0}%` }} /></div></td>
        </tr>}</For></tbody>
      </table>
    </div>
  </Show>
  </Loading>;
}

function CostTable(props: { slices: Slice[]; label: string; loading?: boolean }) {
  const rows = createMemo(() => [...props.slices].sort((a, b) => b.cost - a.cost));
  const max = createMemo(() => rows()[0]?.cost ?? 0);
  return <Loading when={Boolean(props.loading)} skeleton="table"><Show when={rows().length} fallback={<Empty title="No spend" action="Costs appear once priced models serve traffic." />}>
    <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table">
      <table class="dense">
        <thead><tr><th>{props.label}</th><th class="num">Requests</th><th class="num">Spend</th><th class="num">$ / 1M tok</th><th class="share-col"><span class="sr-only">Share</span></th></tr></thead>
        <tbody><For each={rows().slice(0, 8)}>{(row) => <tr>
          <td class="strong mono">{row.name}</td>
          <td class="num">{formatNumber(row.requests)}</td>
          <td class="num">{formatUsd(row.cost)}</td>
          <td class="num">{row.input + row.output ? formatUsd((row.cost / (row.input + row.output)) * 1e6) : "—"}</td>
          <td class="share-col"><div class="meter"><i style={{ width: `${max() ? Math.round((row.cost / max()) * 100) : 0}%` }} /></div></td>
        </tr>}</For></tbody>
      </table>
    </div>
  </Show>
  </Loading>;
}

type ModelRow = { model: string; requests: number; failed: number; tokens: number; cost: number };

function ActivityTable(props: { rows: ModelRow[]; loading?: boolean }) {
  const max = createMemo(() => props.rows[0]?.cost ?? 0);
  return <Loading when={Boolean(props.loading)} skeleton="table"><Show when={props.rows.length} fallback={<Empty title="No traffic" action="Nothing served in this range." />}>
    <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table">
      <table class="dense">
        <thead><tr><th>Model</th><th class="num">Requests</th><th class="num">Tokens</th><th class="num">Spend</th><th class="share-col"><span class="sr-only">Share</span></th></tr></thead>
        <tbody><For each={props.rows.slice(0, 10)}>{(row) => <tr>
          <td class="strong mono">{row.model}</td>
          <td class="num">{formatNumber(row.requests)}</td>
          <td class="num">{formatNumber(row.tokens)}</td>
          <td class="num">{formatUsd(row.cost)}</td>
          <td class="share-col"><div class="meter"><i style={{ width: `${max() ? Math.round((row.cost / max()) * 100) : 0}%` }} /></div></td>
        </tr>}</For></tbody>
      </table>
    </div>
  </Show>
  </Loading>;
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

function Requests(props: { refresh: () => number }) {
  const [search, setSearch] = urlSearch();
  const [range, setRange] = urlRange({ kind: "relative", seconds: 3600, label: "Last hour" });
  const [status, setStatus] = urlParam("status");
  const [provider, setProvider] = urlParam("provider");
  const [model, setModel] = urlParam("model");
  const [vkey, setVkey] = urlParam("key");
  // The window is sent to the gateway rather than filtered here: the
  // in-memory tail is ~90 seconds at a million requests a day, so
  // anything older has to be read from the flushed partitions, and only
  // the gateway can do that.
  // Pages are addressed by cursor, not offset: new requests arrive at
  // the head constantly, so an offset would shift under the reader and
  // show the same row twice. `stack` keeps the cursors walked through so
  // "previous" is a step back rather than a re-query from the start.
  const [cursor, setCursor] = createSignal<string | null>(null);
  const [stack, setStack] = createSignal<string[]>([]);

  // Every query derives from one place, so the range picker and the
  // filters cannot move the table and the header out of step — which is
  // exactly what happened when the header was computed from the page.
  const query = createMemo(() => {
    const r = resolveRange(range());
    return {
      errors: status() === "errors",
      since_ms: Math.floor(r.startMs),
      until_ms: Math.ceil(r.endMs),
      provider: provider() || undefined,
      model: model() || undefined,
      key: vkey() || undefined,
    };
  });

  // Changing a filter or the range invalidates the page walk: cursor N of
  // the old query means nothing under the new one.
  createEffect(
    on(
      () => [range(), status(), provider(), model(), vkey()] as const,
      () => { setCursor(null); setStack([]); },
      { defer: true },
    ),
  );

  const [records] = createResource(
    () => [props.refresh(), query(), cursor()] as const,
    ([, q, after]) => api.requests(q.errors, PAGE_SIZE, { ...q, after: after ?? undefined }),
  );
  // Totals for the whole window, independent of which page is open.
  const [summary] = createResource(
    () => [props.refresh(), query()] as const,
    ([, q]) => api.requestsSummary(q.errors, q),
  );
  const [selected, setSelected] = createSignal<UsageRecord | null>(null);
  const [keys] = createResource(props.refresh, api.keys);

  const all = () => records()?.data ?? [];
  // Filters are faceted off whatever actually arrived, so a provider with
  // no traffic in the window never appears as a choice that returns
  // nothing.
  const facet = (pick: (r: UsageRecord) => string | undefined): Option[] =>
    [...new Set(all().map(pick).filter(Boolean) as string[])]
      .sort()
      .map((v) => ({ value: v, label: v }));

  // Only the free-text search narrows client-side; everything else is
  // already applied by the gateway, so re-applying it here would only
  // risk the two disagreeing.
  const shown = createMemo(() => {
    const needle = search().trim().toLowerCase();
    if (!needle) return all();
    return all().filter((record) =>
      `${record.requested} ${routeLabel(record)} ${record.vkey ?? ""} ${record.status} ${record.request_id} ${record.prompt ?? ""}`
        .toLowerCase()
        .includes(needle),
    );
  });

  const totals = () => summary();
  const pages = () => {
    const total = totals()?.requests ?? 0;
    return Math.max(1, Math.ceil(total / PAGE_SIZE));
  };

  return <div class="stack-lg">
    <FilterBar
      search={search()}
      onSearch={setSearch}
      searchPlaceholder="Search route, key, prompt, request id (press /)"
      extra={<RangePicker value={range()} onChange={setRange} />}
      filters={[
        {
          id: "status",
          label: "Status",
          value: status(),
          onChange: setStatus,
          options: [
            { value: "errors", label: "Errors only" },
            { value: "ok", label: "Successful only" },
          ],
        },
        { id: "provider", label: "Provider", value: provider(), onChange: setProvider, options: facet((r) => r.provider) },
        { id: "model", label: "Model", value: model(), onChange: setModel, options: facet((r) => r.model) },
        {
          id: "vkey",
          label: "Virtual key",
          value: vkey(),
          onChange: setVkey,
          options: (keys()?.data ?? []).map((k) => ({ value: k.id, label: k.name, hint: k.id })),
        },
      ]}
    />
    <Loading when={summary.loading && !summary()} skeleton="stats" rows={5}>
    <section class="stat-row wide" aria-label="Totals for the selected range">
      <div class="stat">
        <span>Requests</span>
        <strong>{formatNumber(totals()?.requests ?? 0)}</strong>
        <small>{formatNumber(shown().length)} on this page</small>
      </div>
      <div class="stat">
        <span>Upstream calls</span>
        <strong>{formatNumber(totals()?.attempts ?? 0)}</strong>
        <small>{retryNote(totals())}</small>
      </div>
      <div class="stat" classList={{ danger: (totals()?.errors ?? 0) > 0 }}>
        <span>Errors</span>
        <strong>{formatNumber(totals()?.errors ?? 0)}</strong>
        <small>{errorRate(totals())}</small>
      </div>
      <div class="stat">
        <span>Tokens</span>
        <strong>{formatNumber((totals()?.input_tokens ?? 0) + (totals()?.output_tokens ?? 0))}</strong>
        <small>{formatNumber(totals()?.input_tokens ?? 0)} in · {formatNumber(totals()?.output_tokens ?? 0)} out</small>
      </div>
      <div class="stat">
        <span>Spend</span>
        <strong>{formatUsd((totals()?.cost_micro_usd ?? 0) / 1e6)}</strong>
        <small>{formatNumber(totals()?.cached_tokens ?? 0)} cached tokens</small>
      </div>
      <div class="stat">
        <span>Latency</span>
        <strong>{formatNumber(totals()?.p50_latency_ms ?? 0)} ms</strong>
        <small>p50 · p95 {formatNumber(totals()?.p95_latency_ms ?? 0)} ms</small>
      </div>
    </section>
    <Show when={totals()?.capped}>
      <p class="muted">
        This window holds more requests than one summary scan covers; the totals above are a floor,
        not the whole range. Narrow the range or a filter for exact figures.
      </p>
    </Show>
    </Loading>
    <section class="flat-section">
      <header>
        <h2>Requests</h2>
        <span class="muted">click a row to open what was sent and returned</span>
        <span class="pager">
          <button
            class="button outline"
            disabled={!stack().length}
            onClick={() => {
              const previous = [...stack()];
              const back = previous.pop() ?? null;
              setStack(previous);
              setCursor(back);
            }}
          >Previous</button>
          <span class="muted">Page {stack().length + 1} of {formatNumber(pages())}</span>
          <button
            class="button outline"
            disabled={!records()?.next}
            onClick={() => {
              const next = records()?.next;
              if (!next) return;
              setStack([...stack(), cursor() ?? ""]);
              setCursor(next);
            }}
          >Next</button>
        </span>
      </header>
      <RequestRows
        records={shown()}
        loading={records.loading && !records()}
        onOpen={setSelected}
      />
    </section>
    <RequestDrawer record={selected()} onClose={() => setSelected(null)} />
  </div>;
}

/// The gap between client requests and upstream calls is the cost of
/// unhealthy seats, so it is named rather than left to arithmetic.
function retryNote(summary?: RequestsSummary): string {
  if (!summary || !summary.requests) return "—";
  const extra = summary.attempts - summary.requests;
  if (extra <= 0) return "no retries";
  return `${formatNumber(extra)} retried or failed over`;
}

function errorRate(summary?: RequestsSummary): string {
  if (!summary || !summary.requests) return "—";
  return `${((summary.errors / summary.requests) * 100).toFixed(1)}% of requests`;
}

/// The route a request took, readable even when it never took one.
///
/// `model` is only filled in once a seat has been picked, so a request
/// that died before that — a 503 with no attempts, which is what an
/// exhausted provider looks like — carried an empty model and rendered as
/// a bare "codex/". The model the caller *asked* for is known from the
/// start, so it stands in: the row says what was wanted even when nothing
/// served it. `requested` may already be provider-qualified, so only the
/// last segment is taken and the provider is never printed twice.
function routeLabel(record: UsageRecord): string {
  const model = record.model || record.requested.split("/").pop() || "";
  if (!record.provider) return record.requested || "—";
  return model ? `${record.provider}/${model}` : record.provider;
}

/// Bodies already fetched, keyed by request id.
///
/// Bodies are immutable once written, so this never goes stale — a
/// request's transcript is what it was. Bounded because a long session
/// of scrolling the log would otherwise accumulate every row hovered.
type Bodies = Awaited<ReturnType<typeof api.requestBodies>>;
const bodyCache = new Map<string, Promise<Bodies>>();
const BODY_CACHE_CAP = 200;

function fetchBodies(record: UsageRecord): Promise<Bodies> {
  const hit = bodyCache.get(record.request_id);
  if (hit) return hit;
  const pending = api.requestBodies(record.request_id, record.ts);
  // A failure must not be cached as an answer: the next open should try
  // again rather than re-serving the error forever.
  pending.catch(() => bodyCache.delete(record.request_id));
  if (bodyCache.size >= BODY_CACHE_CAP) {
    const oldest = bodyCache.keys().next().value;
    if (oldest !== undefined) bodyCache.delete(oldest);
  }
  bodyCache.set(record.request_id, pending);
  return pending;
}

/// Start the fetch and ignore the result — the cache is the point.
function prefetchBodies(record: UsageRecord): void {
  void fetchBodies(record).catch(() => {});
}

/// One request, opened: what was sent, what came back, and the metadata
/// that describes the trip.
///
/// Bodies are fetched per request rather than listed with the page: a
/// page of a hundred would otherwise carry megabytes nobody reads. They
/// are fetched on *hover* rather than on click, so the round trip
/// happens while the operator is still deciding.
function RequestDrawer(props: { record: UsageRecord | null; onClose: () => void }) {
  const [bodies] = createResource(
    () => props.record,
    (record) => fetchBodies(record),
  );
  const [tab, setTab] = createSignal<"input" | "output">("input");
  // Rendered by default, because the parsed transcript is what the drawer
  // is for; the raw bytes stay one click away, since the parse is an
  // interpretation and the JSON is the truth.
  const [view, setView] = createSignal<"rendered" | "json">("rendered");

  const raw = () => (tab() === "input" ? bodies()?.input : bodies()?.output);
  const conversation = createMemo(() => {
    const text = bodies()?.input;
    return text ? parseConversation(text) : null;
  });
  const answer = createMemo(() => {
    const text = bodies()?.output;
    return text ? parseAnswer(text) : [];
  });
  // Nothing to render means nothing was recognised; the JSON view is then
  // the only honest thing to show, so the toggle is not offered.
  const renderable = createMemo(() =>
    tab() === "input" ? Boolean(conversation()?.turns.length) : answer().length > 0,
  );

  return <Drawer
    open={Boolean(props.record)}
    title={props.record?.requested ?? ""}
    subtitle={props.record ? new Date(props.record.ts).toLocaleString() : ""}
    onClose={props.onClose}
  >
    <Show when={props.record} keyed>{(record) => <>
      <div class="drawer-cards">
        <section class="drawer-card">
          <h3>Routing</h3>
          <dl>
            <div><dt>Status</dt><dd><Status text={String(record.status)} tone={record.status < 400 ? "success" : "danger"} /></dd></div>
            <div><dt>Route</dt><dd class="mono">{routeLabel(record)}</dd></div>
            <div><dt>Requested</dt><dd class="mono">{record.requested}</dd></div>
            <div><dt>Endpoint</dt><dd class="mono">{record.endpoint}</dd></div>
            <div><dt>Virtual key</dt><dd class="mono">{record.vkey ?? "—"}</dd></div>
            <div><dt>Attempts</dt><dd>{record.attempts}{record.attempts > 1 ? " (retried)" : ""}</dd></div>
            <div><dt>Streaming</dt><dd>{record.stream ? "Yes" : "No"}</dd></div>
            <div><dt>Request id</dt><dd class="mono wrap">{record.request_id}</dd></div>
          </dl>
        </section>
        <section class="drawer-card">
          <h3>Cost and timing</h3>
          <dl>
            <div><dt>Input tokens</dt><dd>{formatNumber(record.input_tokens)}</dd></div>
            <div><dt>Output tokens</dt><dd>{formatNumber(record.output_tokens)}</dd></div>
            <div><dt>Cached tokens</dt><dd>{formatNumber(record.cached_tokens)}</dd></div>
            <div><dt>Total tokens</dt><dd>{formatNumber(record.input_tokens + record.output_tokens)}</dd></div>
            <div><dt>Cost</dt><dd>{formatUsd(record.cost_micro_usd / 1e6)}</dd></div>
            <div><dt>Latency</dt><dd>{formatNumber(record.latency_ms)} ms</dd></div>
            <div><dt>Gateway overhead</dt><dd>{formatNumber(record.overhead_us)} µs</dd></div>
            <div><dt>Time</dt><dd>{new Date(record.ts).toLocaleString()}</dd></div>
          </dl>
        </section>
      </div>
      <div class="drawer-section">
        <SectionTitle
          title="Conversation"
          subtitle="What crossed the wire, read as a transcript"
          action={
            <div class="body-controls">
              <div class="segmented" role="group" aria-label="Body">
                <button type="button" aria-pressed={tab() === "input"} onClick={() => setTab("input")}>Input</button>
                <button type="button" aria-pressed={tab() === "output"} onClick={() => setTab("output")}>Output</button>
              </div>
              {/* Always mounted, disabled when the body did not parse.
                  Unmounting it made switching Input → Output reflow the
                  control row and slide the tabs sideways, which read as
                  the drawer glitching rather than as "nothing to render". */}
              <div class="segmented" role="group" aria-label="View">
                <button
                  type="button"
                  disabled={!renderable()}
                  title={renderable() ? undefined : "This body did not parse into a transcript"}
                  aria-pressed={renderable() && view() === "rendered"}
                  onClick={() => setView("rendered")}
                >Rendered</button>
                <button
                  type="button"
                  aria-pressed={!renderable() || view() === "json"}
                  onClick={() => setView("json")}
                >JSON</button>
              </div>
            </div>
          }
        />
        <Show when={!bodies.loading} fallback={<Skeleton variant="table" rows={4} />}>
          <Show
            when={raw()}
            fallback={<Empty title="Nothing stored" action={bodies()?.reason ?? "This request has no stored body."} />}
          >
            <Show when={renderable() && view() === "rendered"} fallback={<JsonView text={raw()!} label={tab() === "input" ? "Request" : "Response"} />}>
              <Show
                when={tab() === "input"}
                fallback={<Transcript turns={answer()} />}
              >
                <Transcript turns={conversation()!.turns} tools={conversation()!.tools} />
              </Show>
            </Show>
          </Show>
          <Show when={bodies()?.truncated}>
            <p class="muted">Stored up to the configured size cap; the rest was not kept.</p>
          </Show>
        </Show>
      </div>
    </>}</Show>
  </Drawer>;
}


function RequestRows(props: {
  records: UsageRecord[];
  compact?: boolean;
  loading?: boolean;
  onOpen?: (record: UsageRecord) => void;
}) {
  return <Loading when={Boolean(props.loading)} skeleton="table">
    <Show when={props.records.length} fallback={<Empty title="No requests yet" action="Send a request through the gateway." />}>
      <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table">
        <table class="dense logs">
          <thead><tr>
            <th>Time</th>
            <th>Route</th>
            <Show when={!props.compact}><th>Prompt</th></Show>
            <Show when={!props.compact}><th>Tokens</th><th>Cost</th></Show>
            <th>Latency</th>
            <th>Status</th>
          </tr></thead>
          <tbody><For each={props.records}>{(record) => <tr
            classList={{ clickable: Boolean(props.onOpen) }}
            onClick={() => props.onOpen?.(record)}
            // Fetched on intent rather than on click. Pointing at a row
            // is a reliable signal that it is about to be opened, and it
            // buys the round trip back — by the time the drawer has
            // finished animating the transcript is already in hand,
            // instead of the panel opening empty and filling in.
            onPointerEnter={() => props.onOpen && prefetchBodies(record)}
            onFocusIn={() => props.onOpen && prefetchBodies(record)}
          >
            <td class="mono nowrap">{new Date(record.ts).toLocaleTimeString()}</td>
            {/* One line, not two: the alias and the resolved route were
                stacked, which doubled every row's height to show a string
                that is usually the same one twice. */}
            <td class="mono route-cell" title={`${record.requested} → ${routeLabel(record)}`}>
              {routeLabel(record)}
            </td>
            <Show when={!props.compact}>
              <td class="prompt-cell" title={record.prompt ?? ""}>
                <Show when={record.prompt} fallback={<span class="muted">—</span>}>{record.prompt}</Show>
              </td>
            </Show>
            <Show when={!props.compact}>
              <td class="nowrap">{formatNumber(record.input_tokens + record.output_tokens)}</td>
              <td class="nowrap">{formatUsd(record.cost_micro_usd / 1e6)}</td>
            </Show>
            <td class="nowrap">{record.latency_ms} ms</td>
            <td><Status text={String(record.status)} tone={record.status < 400 ? "success" : "danger"} /></td>
          </tr>}</For></tbody>
        </table>
      </div>
    </Show>
  </Loading>;
}

type ChatMessage = { role: "user" | "assistant"; content: string; meta?: string; error?: boolean };

/// The playground is a conversation, not a one-shot form: system prompt
/// pinned above the thread, messages in the middle, composer at the
/// bottom, and everything tunable in a parameters rail on the right —
/// the shape people already know from the OpenAI playground.
function Playground() {
  const [providers] = createResource(api.providers);
  const [keys] = createResource(api.keys);
  const [routes] = createResource(api.routes);
  const [model, setModel] = createSignal("");
  const [vkey, setVkey] = createSignal("");
  const [system, setSystem] = createSignal("");
  const [systemOpen, setSystemOpen] = createSignal(false);
  const [temperature, setTemperature] = createSignal("");
  const [maxTokens, setMaxTokens] = createSignal("");
  const [draft, setDraft] = createSignal("");
  const [messages, setMessages] = createSignal<ChatMessage[]>([]);
  const [pending, setPending] = createSignal(false);
  let thread!: HTMLDivElement;

  // Everything callable: concrete provider/model pairs, plus routing
  // groups — a group is exactly what an SDK would send, so the
  // playground must accept it too.
  const modelOptions = createMemo<Option[]>(() => {
    const out: Option[] = [];
    for (const group of routes()?.data ?? []) {
      const split = group.primary.length > 1 ? `${group.primary.length} models` : (group.primary[0]?.target ?? "");
      out.push({ value: group.name, label: group.name, hint: `group · ${split}` });
    }
    for (const provider of providers()?.data ?? []) {
      const models = new Set<string>();
      for (const k of provider.keys) for (const m of k.models ?? []) models.add(m);
      for (const m of models) out.push({ value: `${provider.name}/${m}`, label: `${provider.name}/${m}`, hint: provider.kind });
    }
    return out;
  });
  const keyOptions = createMemo<Option[]>(() =>
    (keys()?.data ?? []).filter((k) => k.enabled).map((k) => ({ value: k.id, label: k.name, hint: k.id })),
  );

  const run = async () => {
    const content = draft().trim();
    if (!content || !model() || pending()) return;
    const history: ChatMessage[] = [...messages(), { role: "user", content }];
    setMessages(history);
    setDraft("");
    setPending(true);
    queueMicrotask(() => thread?.scrollTo({ top: thread.scrollHeight }));
    const started = performance.now();
    try {
      const response = await fetch("/v1/chat/completions", {
        method: "POST",
        headers: {
          "content-type": "application/json",
          authorization: `Bearer ${sessionToken()}`,
          ...(vkey() ? { "x-rapid-vkey": vkey() } : {}),
        },
        body: JSON.stringify({
          model: model(),
          messages: [
            ...(system().trim() ? [{ role: "system", content: system().trim() }] : []),
            ...history.map((m) => ({ role: m.role, content: m.content })),
          ],
          ...(temperature() !== "" ? { temperature: Number(temperature()) } : {}),
          ...(maxTokens() !== "" ? { max_tokens: Number(maxTokens()) } : {}),
        }),
      });
      const data = await response.json();
      if (!response.ok) throw new Error(data?.error?.message ?? `HTTP ${response.status}`);
      const tokens = (data.usage?.prompt_tokens ?? 0) + (data.usage?.completion_tokens ?? 0);
      setMessages([...history, {
        role: "assistant",
        content: data.choices?.[0]?.message?.content ?? JSON.stringify(data, null, 2),
        meta: `${Math.round(performance.now() - started)} ms · ${response.headers.get("x-rapid-provider") ?? "?"} · ${formatNumber(tokens)} tokens · ${response.headers.get("x-rapid-overhead-us") ?? "0"} µs gateway`,
      }]);
    } catch (err) {
      setMessages([...history, {
        role: "assistant",
        content: err instanceof Error ? err.message : "Request failed",
        error: true,
      }]);
    } finally {
      setPending(false);
      queueMicrotask(() => thread?.scrollTo({ top: thread.scrollHeight, behavior: "smooth" }));
    }
  };

  return <div class="playground">
    <section class="pg-thread" aria-label="Conversation">
      <div class="pg-system" classList={{ open: systemOpen() }}>
        <button
          type="button"
          class="pg-system-toggle"
          aria-expanded={systemOpen()}
          onClick={() => setSystemOpen((v) => !v)}
        >
          <ChevronRight size={13} class="pg-system-chevron" aria-hidden="true" />
          <span class="pg-system-label">System prompt</span>
          <Show when={!systemOpen()}>
            <span class="pg-system-preview">{system().trim() || "None — add one to steer every turn"}</span>
          </Show>
        </button>
        <Show when={systemOpen()}>
          <textarea
            rows={3}
            autofocus
            placeholder="You are a helpful assistant…"
            value={system()}
            onInput={(e) => setSystem(e.currentTarget.value)}
          />
        </Show>
      </div>
      <div class="pg-messages" ref={thread} aria-live="polite" tabindex="0" role="region" aria-label="Conversation messages">
        <Show when={messages().length} fallback={
          <div class="pg-empty">
            <strong>Test a route end to end</strong>
            <p class="muted">Pick a model on the right and send a message. Responses arrive through the gateway exactly as an SDK would see them.</p>
          </div>
        }>
          <For each={messages()}>{(message) => (
            <div class="pg-msg" classList={{ user: message.role === "user", error: Boolean(message.error) }}>
              <span class="pg-role">{message.role === "user" ? "You" : "Assistant"}</span>
              <div class="pg-content">{message.content}</div>
              <Show when={message.meta}><span class="pg-meta">{message.meta}</span></Show>
            </div>
          )}</For>
          <Show when={pending()}>
            <div class="pg-msg"><span class="pg-role">Assistant</span><div class="pg-content pg-waiting">…</div></div>
          </Show>
        </Show>
      </div>
      <div class="pg-composer">
        <div class="pg-composer-inner">
        <textarea
          rows={2}
          placeholder="Send a message  ·  ⌘↵ to run"
          aria-label="Message"
          value={draft()}
          onInput={(e) => setDraft(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
              e.preventDefault();
              run();
            }
          }}
        />
        <button class="button primary" disabled={pending() || !model() || !draft().trim()} onClick={run}>
          <Play size={14} />{pending() ? "Running…" : "Run"}
        </button>
        </div>
      </div>
    </section>

    <aside class="pg-params" aria-label="Parameters">
      <label>Model
        <Combobox value={model()} options={modelOptions()} onSelect={setModel} label="Model" placeholder="Select a model…" />
      </label>
      <label>Virtual key <span class="optional">Optional</span>
        <Combobox value={vkey()} options={keyOptions()} onSelect={setVkey} label="Virtual key" placeholder="Console session" allowEmpty />
      </label>
      <div class="field-row">
        <label>Temperature
          <input type="number" min="0" max="2" step="0.1" placeholder="default" value={temperature()} onInput={(e) => setTemperature(e.currentTarget.value)} />
        </label>
        <label>Max tokens
          <input type="number" min="1" placeholder="default" value={maxTokens()} onInput={(e) => setMaxTokens(e.currentTarget.value)} />
        </label>
      </div>
      <Show when={vkey()}>
        <p class="muted">Runs spend this key's budget and count against its limits.</p>
      </Show>
      <button class="button outline" disabled={!messages().length} onClick={() => setMessages([])}>
        Clear conversation
      </button>
      <span class="pg-params-spacer" />
      <p class="muted">Requests leave through the same path an SDK uses — retries, fallbacks and limits included.</p>
    </aside>
  </div>;
}

function Fleet(props: { refresh: () => number }) {
  const [fleet] = createResource(props.refresh, api.fleet);
  const nodes = createMemo(() => fleet()?.nodes ?? []);
  const age = (ms: number) => (ms < 1000 ? "just now" : `${Math.round(ms / 1000)}s ago`);
  return <div class="stack-lg">
    <section class="summary-strip" aria-label="Cluster summary">
      <Metric label="Live nodes" value={String(fleet()?.live ?? 1)} />
      <Metric label="Store backend" value={fleet()?.backend ?? "local"} />
      <Metric label="Config version" value={String(fleet()?.version ?? 0)} />
      <Metric
        label="Store"
        value={fleet()?.reachable === false ? "Unreachable" : "Reachable"}
        tone={fleet()?.reachable === false ? "danger" : "default"}
      />
    </section>

    <Show when={fleet() && fleet()!.reachable === false}>
      <div class="notice" role="status">
        This node cannot reach the control-plane store. It keeps serving traffic from the
        configuration it last loaded, and refuses configuration changes until the store is
        back. Nothing needs to be done to the node itself.
      </div>
    </Show>

    <div class="two-column">
      <section class="panel">
        <SectionTitle title="This node" subtitle="Identical to every other; the view is the same from any of them" />
        <dl class="facts">
          <Fact label="Node id" value={String(fleet()?.node ?? "local").slice(0, 18) + "…"} />
          <Fact label="Mode" value={fleet()?.mode === "file" ? "File (read-only)" : "Managed"} />
          <Fact label="Rate-limit shares" value={String(fleet()?.shares ?? 1)} />
        </dl>
        <p class="muted">
          Point several nodes at the same S3 bucket, DynamoDB table or shared file and they
          form a fleet — no leader, no quorum, no join step.
        </p>
      </section>
      <section class="panel">
        <SectionTitle title="Nodes" subtitle="Everyone who has heartbeated against this store recently" />
        <Show
          when={nodes().length}
          fallback={fleet.loading && !fleet() ? <Skeleton variant="table" rows={3} /> : <Empty title="Running alone" action="This node keeps its state locally. Shared-store nodes appear here as they heartbeat." />}
        >
          <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table">
            <table>
              <thead><tr><th>Node</th><th>Address</th><th class="num">Last heartbeat</th></tr></thead>
              <tbody>
                <For each={nodes()}>{(node: any) => <tr>
                  <td>
                    <span style={{ display: "flex", "align-items": "center", gap: "7px" }}>
                      <strong class="mono">{String(node.id).slice(0, 13)}…</strong>
                      <Show when={node.self}><span class="pill accent">this node</span></Show>
                    </span>
                  </td>
                  <td class="mono" style={{ color: "var(--muted)" }}>{node.address ?? "—"}</td>
                  <td class="num">{age(node.age_ms ?? 0)}</td>
                </tr>}</For>
              </tbody>
            </table>
          </div>
        </Show>
      </section>
    </div>
  </div>;
}

function UsersPage(props: { refresh: () => number }) {
  const [users, { refetch }] = createResource(props.refresh, api.users);
  const [search, setSearch] = createSignal("");
  const [creating, setCreating] = createSignal(false);
  const [resetting, setResetting] = createSignal<InternalUser | null>(null);
  const [error, setError] = createSignal("");
  const shown = createMemo(() => {
    const needle = search().trim().toLowerCase();
    const all = users()?.data ?? [];
    return needle ? all.filter((u) => u.email.toLowerCase().includes(needle)) : all;
  });
  return <div class="stack-lg">
    <FilterBar
      search={search()}
      onSearch={setSearch}
      searchPlaceholder="Search users (press /)"
      filters={[]}
      extra={<button class="button primary" onClick={() => setCreating(true)}><Plus size={15} />Add user</button>}
    />
    <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
    <section class="panel">
      <SectionTitle title="Internal users" subtitle="Who may sign in to this console, and with what role" />
      <Loading when={users.loading && !users()} skeleton="table"><Show when={shown().length} fallback={<Empty title="No users yet" action="Only the admin key can sign in. Add a user to give someone their own account." />}>
        <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table"><table>
          <thead><tr><th>Email</th><th>Role</th><th>Teams</th><th>Added</th><th><span class="sr-only">Actions</span></th></tr></thead>
          <tbody><For each={shown()}>{(user) => <tr>
            <td class="strong">{user.email}</td>
            <td><span class="pill" classList={{ accent: user.role === "admin" }}>{user.role}</span></td>
            <td>
              <Show when={user.teams.length} fallback={<span class="muted">—</span>}>
                <For each={user.teams}>{(team) => <span class="pill" style={{ "margin-right": "4px" }}>{team.name}</span>}</For>
              </Show>
            </td>
            <td class="muted">{new Date(user.created_ms).toLocaleDateString()}</td>
            <td class="actions">
              <button class="icon-button" title={`Reset password for ${user.email}`} aria-label={`Reset password for ${user.email}`} onClick={() => setResetting(user)}><KeyRound size={14} /></button>
              <button class="icon-button danger" title={`Delete ${user.email}`} aria-label={`Delete ${user.email}`} onClick={async () => {
                if (!confirm(`Delete ${user.email}? Their sessions end immediately.`)) return;
                setError("");
                try { await api.deleteUser(user.id); await refetch(); }
                catch (err) { setError(err instanceof Error ? err.message : "Delete failed"); }
              }}><Trash2 size={14} /></button>
            </td>
          </tr>}</For></tbody>
        </table></div>
      </Show>
      </Loading>
    </section>
    <Show when={creating()}>
      <UserDialog onClose={() => setCreating(false)} onDone={async () => { setCreating(false); await refetch(); }} />
    </Show>
    <Show when={resetting()} keyed>{(user) => (
      <ResetPasswordDialog user={user} onClose={() => setResetting(null)} onDone={async () => { setResetting(null); await refetch(); }} />
    )}</Show>
  </div>;
}

function UserDialog(props: { onClose: () => void; onDone: () => void }) {
  escapeCloses(props.onClose);
  const [email, setEmail] = createSignal("");
  const [password, setPassword] = createSignal("");
  const [role, setRole] = createSignal("member");
  const [error, setError] = createSignal("");
  const [pending, setPending] = createSignal(false);
  return <div class="dialog-backdrop" role="presentation" onMouseDown={(e) => { if (e.target === e.currentTarget) props.onClose(); }}>
    <form class="dialog" role="dialog" aria-modal="true" aria-labelledby="add-user-title" onSubmit={async (e) => {
      e.preventDefault(); setError(""); setPending(true);
      try { await api.createUser({ email: email(), password: password(), role: role() }); props.onDone(); }
      catch (err) { setError(err instanceof Error ? err.message : "Could not add user"); }
      finally { setPending(false); }
    }}>
      <header class="dialog-head">
        <div><h2 id="add-user-title">Add user</h2><p class="muted">They sign in with these credentials; put them on a team to grant access.</p></div>
        <button type="button" class="icon-button" aria-label="Close" title="Close (Esc)" onClick={props.onClose}><X size={16} /></button>
      </header>
      <label>Email<input required type="email" value={email()} onInput={(e) => setEmail(e.currentTarget.value)} /></label>
      <label>Password <span class="optional">Minimum 8 characters</span>
        <input required type="password" minLength={8} autocomplete="new-password" value={password()} onInput={(e) => setPassword(e.currentTarget.value)} />
      </label>
      <label>Role
        <select value={role()} onChange={(e) => setRole(e.currentTarget.value)}>
          <option value="member">Member — whatever their teams grant</option>
          <option value="admin">Admin — everything, including users and teams</option>
        </select>
      </label>
      <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
      <div class="dialog-actions">
        <button type="button" class="button outline" onClick={props.onClose}>Cancel</button>
        <button class="button primary" disabled={pending()}>{pending() ? "Adding…" : "Add user"}</button>
      </div>
    </form>
  </div>;
}

function ResetPasswordDialog(props: { user: InternalUser; onClose: () => void; onDone: () => void }) {
  escapeCloses(props.onClose);
  const [password, setPassword] = createSignal("");
  const [error, setError] = createSignal("");
  const [pending, setPending] = createSignal(false);
  return <div class="dialog-backdrop" role="presentation" onMouseDown={(e) => { if (e.target === e.currentTarget) props.onClose(); }}>
    <form class="dialog" role="dialog" aria-modal="true" aria-labelledby="reset-pw-title" onSubmit={async (e) => {
      e.preventDefault(); setError(""); setPending(true);
      try { await api.updateUser(props.user.id, { password: password() }); props.onDone(); }
      catch (err) { setError(err instanceof Error ? err.message : "Reset failed"); }
      finally { setPending(false); }
    }}>
      <header class="dialog-head">
        <div><h2 id="reset-pw-title">Reset password</h2><p class="muted">{props.user.email} keeps their sessions; the old password stops working.</p></div>
        <button type="button" class="icon-button" aria-label="Close" title="Close (Esc)" onClick={props.onClose}><X size={16} /></button>
      </header>
      <label>New password <span class="optional">Minimum 8 characters</span>
        <input required autofocus type="password" minLength={8} autocomplete="new-password" value={password()} onInput={(e) => setPassword(e.currentTarget.value)} />
      </label>
      <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
      <div class="dialog-actions">
        <button type="button" class="button outline" onClick={props.onClose}>Cancel</button>
        <button class="button primary" disabled={pending()}>{pending() ? "Saving…" : "Set password"}</button>
      </div>
    </form>
  </div>;
}

const ACCESS_LABELS: Record<string, string> = {
  full: "Full access",
  keys: "Manage keys",
  read_only: "Read only",
};

function TeamsPage(props: { refresh: () => number }) {
  const [teams, { refetch }] = createResource(props.refresh, api.teams);
  const [users] = createResource(props.refresh, api.users);
  const [providers] = createResource(props.refresh, api.providers);
  const [routes] = createResource(props.refresh, api.routes);
  const [search, setSearch] = createSignal("");
  const [editing, setEditing] = createSignal<Team | null>(null);
  const [error, setError] = createSignal("");

  const modelOptions = createMemo<Option[]>(() => {
    const out: Option[] = [];
    for (const group of routes()?.data ?? []) out.push({ value: group.name, label: group.name, hint: "routing group" });
    for (const provider of providers()?.data ?? []) {
      const models = new Set<string>();
      for (const k of provider.keys) for (const m of k.models ?? []) models.add(m);
      for (const m of models) out.push({ value: `${provider.name}/${m}`, label: `${provider.name}/${m}`, hint: provider.kind });
    }
    return out;
  });
  const userOptions = createMemo<Option[]>(() =>
    (users()?.data ?? []).map((u) => ({ value: u.id, label: u.email, hint: u.role })));
  const shown = createMemo(() => {
    const needle = search().trim().toLowerCase();
    const all = teams()?.data ?? [];
    return needle ? all.filter((t) => t.name.toLowerCase().includes(needle)) : all;
  });
  return <div class="stack-lg">
    <FilterBar
      search={search()}
      onSearch={setSearch}
      searchPlaceholder="Search teams (press /)"
      filters={[]}
      extra={<button class="button primary" onClick={() => setEditing({ id: "", name: "", members: [], models: [], access: "keys", created_ms: 0 })}><Plus size={15} />Create team</button>}
    />
    <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
    <section class="panel">
      <SectionTitle title="Teams" subtitle="Members get the models a team lists and the access level it grants" />
      <Loading when={teams.loading && !teams()} skeleton="table"><Show when={teams.loading || shown().length} fallback={<Empty title="No teams yet" action="A team scopes its members to specific models and decides what they may manage." />}>
        <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table"><table>
          <thead><tr><th>Team</th><th>Access</th><th>Members</th><th>Models</th><th><span class="sr-only">Actions</span></th></tr></thead>
          <tbody><For each={shown()}>{(team) => <tr class="clickable" onClick={() => setEditing({ ...team })}>
            <td class="strong">{team.name}</td>
            <td><span class="pill" classList={{ accent: team.access === "full", warning: team.access === "read_only" }}>{ACCESS_LABELS[team.access]}</span></td>
            <td>{team.members.length ? `${team.members.length} member${team.members.length > 1 ? "s" : ""}` : <span class="muted">Empty</span>}</td>
            <td>
              <Show when={team.models.length} fallback={<span class="pill">every model</span>}>
                <For each={team.models.slice(0, 3)}>{(m) => <span class="pill" style={{ "margin-right": "4px" }}>{m}</span>}</For>
                <Show when={team.models.length > 3}><span class="muted">+{team.models.length - 3} more</span></Show>
              </Show>
            </td>
            <td class="actions">
              <button class="icon-button danger" title={`Delete ${team.name}`} aria-label={`Delete ${team.name}`} onClick={async (e) => {
                e.stopPropagation();
                if (!confirm(`Delete team ${team.name}? Its members drop to read-only unless another team covers them.`)) return;
                setError("");
                try { await api.deleteTeam(team.id); await refetch(); }
                catch (err) { setError(err instanceof Error ? err.message : "Delete failed"); }
              }}><Trash2 size={14} /></button>
            </td>
          </tr>}</For></tbody>
        </table></div>
      </Show>
      </Loading>
    </section>
    <Show when={editing()} keyed>{(team) => (
      <TeamDialog
        team={team}
        userOptions={userOptions()}
        modelOptions={modelOptions()}
        onClose={() => setEditing(null)}
        onDone={async () => { setEditing(null); await refetch(); }}
      />
    )}</Show>
    <p class="muted">
      Access levels: <strong>Full</strong> operates the whole console, <strong>Manage keys</strong> creates
      virtual keys within the team's models, <strong>Read only</strong> observes. A member of several teams
      gets the strongest access and the union of models.
    </p>
  </div>;
}

function TeamDialog(props: {
  team: Team;
  userOptions: Option[];
  modelOptions: Option[];
  onClose: () => void;
  onDone: () => void;
}) {
  escapeCloses(props.onClose);
  const [name, setName] = createSignal(props.team.name);
  const [members, setMembers] = createSignal<string[]>(props.team.members);
  const [models, setModels] = createSignal<string[]>(props.team.models);
  const [access, setAccess] = createSignal(props.team.access);
  const [error, setError] = createSignal("");
  const [pending, setPending] = createSignal(false);
  return <div class="dialog-backdrop" role="presentation" onMouseDown={(e) => { if (e.target === e.currentTarget) props.onClose(); }}>
    <form class="dialog wide" role="dialog" aria-modal="true" aria-labelledby="team-title" onSubmit={async (e) => {
      e.preventDefault(); setError(""); setPending(true);
      const body = { name: name(), members: members(), models: models(), access: access() };
      try {
        if (props.team.id) await api.updateTeam(props.team.id, body);
        else await api.createTeam(body);
        props.onDone();
      } catch (err) { setError(err instanceof Error ? err.message : "Save failed"); }
      finally { setPending(false); }
    }}>
      <header class="dialog-head">
        <div><h2 id="team-title">{props.team.id ? `Edit ${props.team.name}` : "Create team"}</h2><p class="muted">Members inherit the team's models and access level.</p></div>
        <button type="button" class="icon-button" aria-label="Close" title="Close (Esc)" onClick={props.onClose}><X size={16} /></button>
      </header>
      <label>Name<input required value={name()} onInput={(e) => setName(e.currentTarget.value)} /></label>
      <label>Members
        <MultiCombobox values={members()} options={props.userOptions} onChange={setMembers} label="Members" emptyMeans="No members yet" />
      </label>
      <label>Models <span class="optional">Leave empty for every model</span>
        <MultiCombobox values={models()} options={props.modelOptions} onChange={setModels} label="Models" emptyMeans="Every model" />
      </label>
      <label>Access level
        <select value={access()} onChange={(e) => setAccess(e.currentTarget.value as Team["access"])}>
          <option value="read_only">Read only — observe usage, logs and configuration</option>
          <option value="keys">Manage keys — create virtual keys within the team's models</option>
          <option value="full">Full — operate the whole console except users and teams</option>
        </select>
      </label>
      <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
      <div class="dialog-actions">
        <button type="button" class="button outline" onClick={props.onClose}>Cancel</button>
        <button class="button primary" disabled={pending() || !name()}>{pending() ? "Saving…" : props.team.id ? "Save team" : "Create team"}</button>
      </div>
    </form>
  </div>;
}

function Settings(props: { refresh: () => number }) {
  const [config] = createResource(props.refresh, api.config);
  const [fleet] = createResource(props.refresh, api.fleet);
  const [theme, setTheme] = createSignal(localStorage.getItem("rapid-theme") ?? "system");
  createEffect(() => {
    const choice = theme();
    localStorage.setItem("rapid-theme", choice);
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
    anchor.download = "rapid-router.toml";
    anchor.click();
    URL.revokeObjectURL(url);
  };
  return <div class="stack-lg settings-layout">
    <div class="settings-grid">
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
        <Fact label="Request history" value={`${setting("retention_days")} days`} />
        <Fact label="Captured bodies" value={`${setting("body_retention_days")} days`} />
        <Fact label="Flush interval" value={`${setting("flush_interval_secs")} s`} />
        <Fact label="Per-key metrics" value={setting("per_key_metrics") === "true" ? "On" : "Off"} />
      </dl>
      <p class="muted">
        History is metadata — volume, tokens, spend, latency and errors — and is what every
        chart and total is drawn from. Bodies are the request and response payloads behind
        the log drawer; they are far larger, so they are kept for a much shorter window.
        Change either under <code>[usage]</code> in the configuration document.
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
    </div>
  <section aria-label="Cluster">
      <SectionTitle title="Cluster" subtitle="Store, nodes and fleet state for this gateway" />
      <Fleet refresh={props.refresh} />
    </section>
  </div>;
}

function Metric(props: { label: string; value: string; tone?: "default" | "danger" }) { return <div classList={{ metric: true, danger: props.tone === "danger" }}><span>{props.label}</span><strong>{props.value ?? "0"}</strong></div>; }
function SectionTitle(props: { title: string; subtitle: string; action?: any }) { return <div class="section-title"><div><h2>{props.title}</h2><p>{props.subtitle}</p></div>{props.action}</div>; }
function Fact(props: { label: string; value: string }) { return <div><dt>{props.label}</dt><dd>{props.value}</dd></div>; }
function Status(props: { text: string; tone: "success" | "danger" | "muted" }) { return <span class={`status ${props.tone}`}><span />{props.text}</span>; }
function Empty(props: { title: string; action: string }) { return <div class="empty"><strong>{props.title}</strong><p>{props.action}</p></div>; }

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
      padding: [12, 8, 0, 4],
      legend: { show: false },
      axes: [
        { stroke: ink.getPropertyValue("--muted"), grid: { stroke: ink.getPropertyValue("--grid") } },
        {
          stroke: ink.getPropertyValue("--muted"),
          grid: { stroke: ink.getPropertyValue("--grid") },
          // Same reason as the trend chart: the gutter has to fit the
          // widest label the formatter can emit, not uPlot's default.
          size: 68,
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
  const [range, setRange] = urlRange({ kind: "relative", seconds: 7 * 86400, label: "Last 7 days" });
  const resolved = createMemo(() => resolveRange(range()));
  const [history] = createResource(
    () => [props.refresh(), range()] as const,
    () => api.history(resolveRange(range()).days, "model"),
  );
  const [measure, setMeasure] = urlParam("measure", "cost");
  const [search, setSearch] = urlSearch();

  const sliced = createMemo(() => {
    const source = history()?.data ?? {};
    const r = resolved();
    const from = new Date(r.startMs).toISOString().slice(0, 10);
    const to = new Date(r.endMs).toISOString().slice(0, 10);
    const needle = search().trim().toLowerCase();
    const out: Record<string, DayBucket[]> = {};
    for (const [model, buckets] of Object.entries(source)) {
      if (needle && !model.toLowerCase().includes(needle)) continue;
      const kept = buckets.filter((b) => b.day >= from && b.day <= to);
      if (kept.length) out[model] = kept;
    }
    return out;
  });
  const shaped = createMemo(() => {
    const out: Record<string, DayBucket[]> = {};
    for (const [model, buckets] of Object.entries(sliced())) {
      out[model] = buckets.map((bucket) => ({
        ...bucket,
        cost_micro_usd:
          measure() === "cost" ? bucket.cost_micro_usd
          : measure() === "requests" ? bucket.requests * 1e6
          : (bucket.input_tokens + bucket.output_tokens) * 1e6,
      }));
    }
    return out;
  });
  const rows = createMemo(() => rowsFromHistory(sliced()));
  const totals = createMemo(() => rows().reduce((acc, r) => ({
    requests: acc.requests + r.requests, tokens: acc.tokens + r.tokens, cost: acc.cost + r.cost,
  }), { requests: 0, tokens: 0, cost: 0 }));

  return <div class="stack-lg">
    <FilterBar
      search={search()}
      onSearch={setSearch}
      searchPlaceholder="Search models (press /)"
      filters={[{
        id: "measure",
        label: "Measure",
        value: measure(),
        onChange: (value) => setMeasure(value || "cost"),
        options: [
          { value: "cost", label: "Cost" },
          { value: "requests", label: "Requests" },
          { value: "tokens", label: "Tokens" },
        ],
      }]}
      extra={<RangePicker value={range()} onChange={setRange} />}
    />
    <Loading when={history.loading && !history()} skeleton="stats" rows={4}>
    <section class="stat-row" aria-label="Totals">
      <div class="stat"><span>Models active</span><strong>{rows().length}</strong></div>
      <div class="stat"><span>Requests</span><strong>{formatNumber(totals().requests)}</strong></div>
      <div class="stat"><span>Tokens</span><strong>{formatNumber(totals().tokens)}</strong></div>
      <div class="stat"><span>Spend</span><strong>{formatUsd(totals().cost)}</strong></div>
    </section>
    </Loading>
    <section class="flat-section">
      <header><h2>{measure() === "cost" ? "Cost per day" : `${measure()} per day`}</h2><span class="muted">{resolved().label} · top six by volume</span></header>
      <Loading when={history.loading && !history()} skeleton="chart"><Show when={Object.keys(shaped()).length} fallback={<Empty title="No history in this range" action="Usage is written to disk periodically; widen the range or send traffic." />}>
        <DailySpendChart series={shaped()} />
      </Show></Loading>
    </section>
    <section class="flat-section">
      <header><h2>Per model</h2><span class="muted">sorted by spend</span></header>
      <ActivityTable rows={rows()} loading={history.loading && !history()} />
    </section>
  </div>;
}

function Models(props: { refresh: () => number }) {
  const [providers, { refetch }] = createResource(props.refresh, api.providers);
  const [catalog] = createResource(props.refresh, api.catalog);
  const [routes] = createResource(props.refresh, api.routes);
  const [search, setSearch] = createSignal("");
  const [providerFilter, setProviderFilter] = createSignal("");
  const [adding, setAdding] = createSignal(false);
  const [error, setError] = createSignal("");

  // The format a model is called with. Not a config field — it follows
  // the provider's adapter — so the catalog is the only place that knows
  // a model is Responses-only, and it is shown rather than chosen.
  const formatOf = (provider: string, kind: string, model: string): string => {
    const preset = [...(catalog()?.presets ?? []), ...(catalog()?.subscriptions ?? [])]
      .find((p) => p.name === provider || p.name === kind.toLowerCase());
    return preset?.models.find((m) => m.id === model)?.format.replace("_", " ") ?? "chat completions";
  };

  const rows = createMemo(() => {
    const out: Array<{ model: string; provider: string; kind: string; format: string; groups: string[] }> = [];
    for (const provider of providers()?.data ?? []) {
      if (providerFilter() && provider.name !== providerFilter()) continue;
      const models = new Set<string>();
      for (const key of provider.keys) for (const model of key.models ?? []) models.add(model);
      for (const model of models) {
        const target = `${provider.name}/${model}`;
        out.push({
          model,
          provider: provider.name,
          kind: provider.kind,
          format: formatOf(provider.name, provider.kind, model),
          groups: (routes()?.data ?? []).filter((r) => allTargets(r).includes(target)).map((r) => r.name),
        });
      }
    }
    const needle = search().trim().toLowerCase();
    return needle ? out.filter((r) => `${r.model} ${r.provider}`.toLowerCase().includes(needle)) : out;
  });

  return <div class="stack-lg">
    <FilterBar
      search={search()}
      onSearch={setSearch}
      searchPlaceholder="Search models (press /)"
      filters={[{
        id: "provider",
        label: "Provider",
        value: providerFilter(),
        onChange: setProviderFilter,
        options: (providers()?.data ?? []).map((p) => ({ value: p.name, label: p.name })),
      }]}
      extra={<button class="button primary" disabled={!providers()?.data.length} onClick={() => setAdding(true)}><Plus size={15} />Add model</button>}
    />
    <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
    <section class="panel">
      <SectionTitle title="Models" subtitle="What callers can ask for, and which provider answers" />
      <Loading when={providers.loading && !providers()} skeleton="table"><Show when={rows().length} fallback={<Empty title="No models yet" action="Models are declared here by hand — nothing is assumed. Add the ids you want callers to use." />}>
        <div class="table-wrap" tabindex="0" role="region" aria-label="Scrollable table"><table>
          <thead><tr><th>Model</th><th>Provider</th><th>Format</th><th>Routing groups</th><th><span class="sr-only">Actions</span></th></tr></thead>
          <tbody><For each={rows()}>{(row) => <tr>
            <td class="strong mono">{row.model}</td>
            <td>{row.provider}</td>
            <td><span class="pill">{row.format}</span></td>
            <td>{row.groups.length ? <For each={row.groups}>{(g) => <span class="pill accent" style={{ "margin-right": "4px" }}>{g}</span>}</For> : <span class="muted">—</span>}</td>
            <td class="actions">
              <button class="icon-button danger" title={`Remove ${row.model}`} aria-label={`Remove ${row.model}`} onClick={async () => {
                if (!confirm(`Remove ${row.provider}/${row.model}?`)) return;
                setError("");
                try { await api.deleteModel(row.provider, row.model); await refetch(); }
                catch (err) { setError(err instanceof Error ? err.message : "Remove failed"); }
              }}><Trash2 size={14} /></button>
            </td>
          </tr>}</For></tbody>
        </table></div>
      </Show>
      </Loading>
    </section>
    <Show when={adding()}>
      <AddModelDialog
        providers={providers()?.data ?? []}
        catalog={catalog()}
        onClose={() => setAdding(false)}
        onDone={async () => { setAdding(false); await refetch(); }}
      />
    </Show>
  </div>;
}

function AddModelDialog(props: {
  providers: Provider[];
  catalog: { presets: CatalogPreset[]; subscriptions: CatalogPreset[] } | undefined;
  onClose: () => void;
  onDone: () => void;
}) {
  escapeCloses(props.onClose);
  const [provider, setProvider] = createSignal(props.providers[0]?.name ?? "");
  const [model, setModel] = createSignal("");
  const [error, setError] = createSignal("");
  const [pending, setPending] = createSignal(false);
  return <div class="dialog-backdrop" role="presentation" onMouseDown={(e) => { if (e.target === e.currentTarget) props.onClose(); }}>
    <form class="dialog" role="dialog" aria-modal="true" aria-labelledby="add-model-title" onSubmit={async (e) => {
      e.preventDefault(); setError(""); setPending(true);
      try { await api.addModel(provider(), model()); props.onDone(); }
      catch (err) { setError(err instanceof Error ? err.message : "Could not add model"); }
      finally { setPending(false); }
    }}>
      <header class="dialog-head">
        <div><h2 id="add-model-title">Add model</h2><p class="muted">Anything the provider serves can be added, listed or not.</p></div>
        <button type="button" class="icon-button" aria-label="Close" title="Close (Esc)" onClick={props.onClose}><X size={16} /></button>
      </header>
      <label>Provider
        <Combobox
          value={provider()}
          options={props.providers.map((p) => ({ value: p.name, label: p.name, hint: p.kind }))}
          onSelect={setProvider}
          label="Provider"
        />
      </label>
      <label>Model id
        <input required placeholder="gpt-4.1-mini" value={model()} onInput={(e) => setModel(e.currentTarget.value)} />
      </label>
      <p class="muted">
        The API shape follows the provider's adapter — {" "}
        {props.providers.find((p) => p.name === provider())?.subscription ? "responses/messages" : "chat completions"} for this one.
        A per-model override is not a config field yet.
      </p>
      <Show when={error()}><p class="form-error" role="alert">{error()}</p></Show>
      <div class="dialog-actions">
        <button type="button" class="button outline" onClick={props.onClose}>Cancel</button>
        <button class="button primary" disabled={pending() || !model() || !provider()}>{pending() ? "Adding…" : "Add model"}</button>
      </div>
    </form>
  </div>;
}

// Pinned to en-US, not the viewer's locale: an operator reading an
// en-IN browser was shown "1.2L" for 120,000 tokens, which is correct
// for that locale and useless for a dollar-denominated API where every
// price is quoted per million. Thousands/millions/billions it is.
const NUMBER = new Intl.NumberFormat("en-US", { notation: "compact", maximumFractionDigits: 1 });
/// Durations are not counts. `formatNumber` is compact notation, which
/// renders 1,240 ms as "1.2K ms" — correct, and not how anybody reads a
/// latency. Whole milliseconds throughout, including on an axis, so the
/// unit never changes between one tick and the next.
const MILLISECONDS = new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 });
const USD = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD", minimumFractionDigits: 2, maximumFractionDigits: 4 });
function formatNumber(value: number | undefined) { return NUMBER.format(value ?? 0); }
function formatUsd(value: number | undefined) { return USD.format(value ?? 0); }
function formatLatency(value: number | undefined) { return `${MILLISECONDS.format(Math.max(0, value ?? 0))} ms`; }
