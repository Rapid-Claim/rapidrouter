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
  credential: { expires_at_ms: number | null; can_refresh: boolean; expired: boolean } | null;
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
};

const TOKEN_KEY = "caret-admin-session";

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

export async function login(key: string): Promise<void> {
  const result = await request<{ token: string }>("/session", {
    method: "POST",
    body: JSON.stringify({ key }),
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
  requests: (errors = false) =>
    request<{ data: UsageRecord[] }>(`/requests?limit=200&errors=${errors}`),
  fleet: () => request<any>("/fleet"),
  providers: () => request<{ data: Provider[] }>("/providers"),
  /** Daily totals from the flushed usage files; `by` splits into series. */
  history: (days = 30, by = "") =>
    request<{ data: Record<string, DayBucket[]> }>(`/history?days=${days}&by=${by}`),
};
