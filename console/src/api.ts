export type ConfigDocument = {
  mode: "file" | "managed";
  read_only: boolean;
  version: number;
  text: string;
};

export type VirtualKey = {
  id: string;
  name: string;
  models: string[];
  budget?: { usd: number; period: "daily" | "weekly" | "monthly" };
  rate?: { rpm?: number; tpm?: number };
  expires_ms?: number;
  tags: Record<string, string>;
  enabled: boolean;
  created_ms: number;
};

/** One credential's live state, as the Providers page shows it. */
export type ProviderKey = {
  name: string;
  weight: number;
  models: string[] | null;
  health: "healthy" | "probing" | "open" | "benched";
  /** Breaker, plan quota and credential validity folded into one word. */
  status?: "ready" | "near_limit" | "exhausted" | "probing" | "open" | "benched";
  benched_until_ms: number | null;
  limits: {
    rpm: { remaining: number | null } | null;
    tpm: { remaining: number | null } | null;
  };
  /** Present only once the provider has reported a window for this seat. */
  quota: {
    observed_ms: number;
    peak_utilization: number | null;
    primary: QuotaWindow | null;
    secondary: QuotaWindow | null;
  } | null;
  /** Subscription seats only; metered keys have no expiry to report. */
  credential: { email?: string | null; expires_at_ms: number | null; can_refresh: boolean; expired: boolean } | null;
  /**
   * The last thing the provider said about this credential — from the
   * gateway's own sweep, an operator's check, or real traffic. Held
   * server-side, so it survives a reload and is the same for everyone
   * looking; `probed` is false when it was simply the last live request.
   */
  last_check: {
    status: "ok" | "rate_limited" | "unauthorized" | "provider_error" | "rejected" | "unreachable";
    detail: string;
    http_status: number | null;
    probed: boolean;
    checked_at_ms: number;
  } | null;
  /** Requests dispatched against this key, ever. */
  leases?: number;
  source_path: string | null;
};

export type QuotaWindow = {
  utilization: number;
  resets_in_s: number | null;
  length_s: number | null;
  rejected: boolean;
};

export type Provider = {
  name: string;
  kind: string;
  subscription: boolean;
  base_url: string | null;
  keys: ProviderKey[];
};

export type CatalogModel = { id: string; format: string };
export type CatalogPreset = {
  name: string;
  custom?: boolean;
  base_url?: string | null;
  discovery_env?: string | null;
  keyless_ok?: boolean;
  subscription?: boolean;
  models: CatalogModel[];
};
/** One model in a routing group's pool, with its share of that pool. */
export type RouteTarget = { target: string; weight: number };
/**
 * A routing group: the model id callers send. `primary` is the live
 * traffic split, weighted; `fallback` is the reserve, only reached once
 * the primary pool is exhausted.
 */
export type RouteGroup = { name: string; primary: RouteTarget[]; fallback: RouteTarget[] };

export type Me = {
  principal: "admin_key" | "user";
  email: string | null;
  is_admin: boolean;
  access: "full" | "keys" | "read_only";
  /** null = every model. */
  models: string[] | null;
  teams: string[];
};
export type InternalUser = {
  id: string;
  email: string;
  role: "admin" | "member";
  created_ms: number;
  teams: Array<{ id: string; name: string }>;
};
export type Team = {
  id: string;
  name: string;
  members: string[];
  models: string[];
  access: "full" | "keys" | "read_only";
  created_ms: number;
};

export type DayBucket = {
  day: string;
  requests: number;
  failed: number;
  input_tokens: number;
  output_tokens: number;
  cost_micro_usd: number;
};

export type UsageRecord = {
  ts: number;
  request_id: string;
  endpoint: string;
  requested: string;
  provider: string;
  model: string;
  vkey?: string;
  status: number;
  input_tokens: number;
  output_tokens: number;
  cost_micro_usd: number;
  latency_ms: number;
  stream: boolean;
  attempts: number;
  cached_tokens: number;
  /** Time inside the gateway itself, as opposed to waiting on a provider. */
  overhead_us: number;
  tag?: string;
  /** First user turn (or the system prompt when there is none), truncated.
   * Extracted by the gateway at record time — absent on records written
   * before that shipped. */
  prompt?: string;
};

export type UsageSlice = {
  name: string;
  requests: number;
  failed: number;
  input_tokens: number;
  output_tokens: number;
  cost_micro_usd: number;
};

export type UsageBucket = {
  ts: number;
  requests: number;
  failed: number;
  input_tokens: number;
  output_tokens: number;
  cost_micro_usd: number;
};

/** Everything the Usage page needs for a window, from one server-side scan. */
export type UsageSummary = {
  requests: number;
  errors: number;
  attempts: number;
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  cost_micro_usd: number;
  p50_latency_ms: number;
  p95_latency_ms: number;
  capped: boolean;
  by_model: UsageSlice[];
  by_provider: UsageSlice[];
  by_key: UsageSlice[];
  series: UsageBucket[];
  bucket_secs: number;
};

/** Totals for a whole window, not the page on screen. */
export type RequestsSummary = {
  requests: number;
  errors: number;
  /** Upstream calls made; exceeds `requests` when retries or failover ran. */
  attempts: number;
  input_tokens: number;
  output_tokens: number;
  cached_tokens: number;
  cost_micro_usd: number;
  p50_latency_ms: number;
  p95_latency_ms: number;
  /** The scan hit its ceiling, so these are a floor rather than exact. */
  capped: boolean;
};

const TOKEN_KEY = "rapid-admin-session";

export function sessionToken(): string {
  return sessionStorage.getItem(TOKEN_KEY) ?? "";
}

export function clearSession(): void {
  sessionStorage.removeItem(TOKEN_KEY);
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const headers = new Headers(init?.headers);
  if (sessionToken()) headers.set("authorization", `Bearer ${sessionToken()}`);
  if (init?.body && !headers.has("content-type")) headers.set("content-type", "application/json");
  const response = await fetch(`/admin/api${path}`, { ...init, headers });
  const body = await response.json().catch(() => ({}));
  if (!response.ok) {
    if (response.status === 401) clearSession();
    throw new Error(body?.error?.message ?? `Request failed with ${response.status}`);
  }
  return body as T;
}

export async function login(
  credentials: { key: string } | { email: string; password: string },
): Promise<void> {
  const result = await request<{ token: string }>("/session", {
    method: "POST",
    body: JSON.stringify(credentials),
  });
  sessionStorage.setItem(TOKEN_KEY, result.token);
}

export const api = {
  config: () => request<ConfigDocument>("/config"),
  saveConfig: (version: number, text: string) =>
    request<{ version: number }>("/config", {
      method: "PUT",
      body: JSON.stringify({ version, text }),
    }),
  keys: () => request<{ data: VirtualKey[] }>("/keys"),
  createKey: (input: Record<string, unknown>) =>
    request<{ key: string; data: VirtualKey }>("/keys", {
      method: "POST",
      body: JSON.stringify(input),
    }),
  updateKey: (id: string, input: Record<string, unknown>) =>
    request<{ data: VirtualKey }>(`/keys/${id}`, {
      method: "PUT",
      body: JSON.stringify(input),
    }),
  rotateKey: (id: string) =>
    request<{ key: string; grace_until_ms: number }>(`/keys/${id}/rotate`, { method: "POST" }),
  deleteKey: (id: string) => request(`/keys/${id}`, { method: "DELETE" }),
  usage: (window = 3600, by = "provider") => request<any>(`/usage?window=${window}&by=${by}`),
  /** Requests in a window. The gateway serves the ring first and reads
   * flushed partitions for anything older, so a range beyond the live
   * tail is a normal query rather than an empty one. */
  requests: (
    errors = false,
    limit = 200,
    window?: {
      since_ms?: number;
      until_ms?: number;
      provider?: string;
      model?: string;
      key?: string;
      after?: string;
    },
  ) => {
    const params = new URLSearchParams({ limit: String(limit), errors: String(errors) });
    for (const [name, value] of Object.entries(window ?? {})) {
      if (value !== undefined && value !== "") params.set(name, String(value));
    }
    return request<{ data: UsageRecord[]; next: string | null }>(`/requests?${params}`);
  },
  /** Totals for the selected range and filters, independent of paging. */
  requestsSummary: (
    errors = false,
    window?: { since_ms?: number; until_ms?: number; provider?: string; model?: string; key?: string },
  ) => {
    const params = new URLSearchParams({ errors: String(errors) });
    for (const [name, value] of Object.entries(window ?? {})) {
      if (value !== undefined && value !== "") params.set(name, String(value));
    }
    return request<RequestsSummary>(`/requests/summary?${params}`);
  },
  /** Totals, groupings and a trend series for a window — one scan, one trip. */
  usageSummary: (
    window?: { since_ms?: number; until_ms?: number; provider?: string; model?: string; key?: string },
  ) => {
    const params = new URLSearchParams({ errors: "false" });
    for (const [name, value] of Object.entries(window ?? {})) {
      if (value !== undefined && value !== "") params.set(name, String(value));
    }
    return request<UsageSummary>(`/usage/summary?${params}`);
  },
  /** What was sent and what came back, for one request. */
  requestBodies: (id: string, ts: number) =>
    request<{
      request_id: string;
      input: string | null;
      output: string | null;
      truncated?: boolean;
      reason?: string;
    }>(`/requests/${encodeURIComponent(id)}/bodies?ts=${ts}`),
  fleet: () => request<any>("/fleet"),
  providers: () => request<{ data: Provider[] }>("/providers"),
  catalog: () =>
    request<{
      presets: CatalogPreset[];
      subscriptions: CatalogPreset[];
      custom: CatalogPreset;
      configured: string[];
    }>("/catalog"),
  createProvider: (input: Record<string, unknown>) =>
    request<{ version: number }>("/providers", { method: "POST", body: JSON.stringify(input) }),
  putSecret: (name: string, value: string) =>
    request<{ reference: string }>("/secrets", {
      method: "POST",
      body: JSON.stringify({ name, value }),
    }),
  putCredentialFile: (name: string, content: string) =>
    request<{ reference: string }>("/credential-files", {
      method: "POST",
      body: JSON.stringify({ name, content }),
    }),
  putCredentialFiles: (files: Array<{ name: string; content: string }>) =>
    request<{
      written: Array<{ name: string; reference: string }>;
      failed: Array<{ name: string; error: string }>;
    }>("/credential-files/bulk", { method: "POST", body: JSON.stringify({ files }) }),
  addProviderKeys: (provider: string, keys: Array<Record<string, unknown>>) =>
    request<{ added: string[]; skipped: string[] }>(
      `/providers/${encodeURIComponent(provider)}/keys/bulk`,
      { method: "POST", body: JSON.stringify({ keys }) },
    ),
  probeProvider: (name: string, input: { key?: string; model?: string } = {}) =>
    request<{ results: Array<{ key: string; model?: string; status: string; detail: string; http_status: number | null }> }>(
      `/providers/${encodeURIComponent(name)}/probe`,
      { method: "POST", body: JSON.stringify(input) },
    ),
  updateProvider: (name: string, input: { base_url?: string }) =>
    request<{ version: number }>(`/providers/${encodeURIComponent(name)}`, {
      method: "PUT",
      body: JSON.stringify(input),
    }),
  deleteProvider: (name: string) =>
    request(`/providers/${encodeURIComponent(name)}`, { method: "DELETE" }),
  addProviderKey: (name: string, input: Record<string, unknown>) =>
    request<{ version: number }>(`/providers/${encodeURIComponent(name)}/keys`, {
      method: "POST",
      body: JSON.stringify(input),
    }),
  deleteProviderKey: (name: string, key: string) =>
    request(`/providers/${encodeURIComponent(name)}/keys/${encodeURIComponent(key)}`, {
      method: "DELETE",
    }),
  addModel: (provider: string, id: string) =>
    request<{ version: number }>(`/providers/${encodeURIComponent(provider)}/models`, {
      method: "POST",
      body: JSON.stringify({ id }),
    }),
  deleteModel: (provider: string, id: string) =>
    request(`/providers/${encodeURIComponent(provider)}/models/${encodeURIComponent(id)}`, {
      method: "DELETE",
    }),
  routes: () => request<{ data: RouteGroup[] }>("/routes"),
  me: () => request<Me>("/me"),
  users: () => request<{ data: InternalUser[] }>("/users"),
  createUser: (input: { email: string; password: string; role: string }) =>
    request<{ data: { id: string } }>("/users", { method: "POST", body: JSON.stringify(input) }),
  updateUser: (id: string, input: { email?: string; password?: string; role?: string }) =>
    request(`/users/${encodeURIComponent(id)}`, { method: "PUT", body: JSON.stringify({ email: "", ...input }) }),
  deleteUser: (id: string) => request(`/users/${encodeURIComponent(id)}`, { method: "DELETE" }),
  teams: () => request<{ data: Team[] }>("/teams"),
  createTeam: (input: Record<string, unknown>) =>
    request<{ data: { id: string } }>("/teams", { method: "POST", body: JSON.stringify(input) }),
  updateTeam: (id: string, input: Record<string, unknown>) =>
    request(`/teams/${encodeURIComponent(id)}`, { method: "PUT", body: JSON.stringify(input) }),
  deleteTeam: (id: string) => request(`/teams/${encodeURIComponent(id)}`, { method: "DELETE" }),
  putRoute: (input: RouteGroup) =>
    request<{ version: number }>("/routes", { method: "POST", body: JSON.stringify(input) }),
  deleteRoute: (name: string) =>
    request(`/routes/${encodeURIComponent(name)}`, { method: "DELETE" }),
  /** Daily totals from the flushed usage files; `by` splits into series,
   * and the optional filters constrain records before bucketing. */
  history: (days = 30, by = "", filters?: { provider?: string; model?: string; key?: string }) => {
    const params = new URLSearchParams({ days: String(days), by });
    if (filters?.provider) params.set("provider", filters.provider);
    if (filters?.model) params.set("model", filters.model);
    if (filters?.key) params.set("key", filters.key);
    return request<{ data: Record<string, DayBucket[]> }>(`/history?${params}`);
  },
};
