/// Page state that lives in the URL rather than in a component.
///
/// Filters used to be plain signals, which meant a reload threw them
/// away: you narrowed to one model over the last 30 days, hit refresh —
/// or followed a link, or came back to the tab after the session
/// expired — and landed back on an unfiltered last-24-hours view with no
/// sign that anything had been discarded. It also made a filtered view
/// impossible to send to anybody, which is most of what an operator
/// wants to do with one once they have found something.
///
/// So the URL is the state: `#usage?range=30d&model=gpt-4o`. Reload
/// restores it, the address bar says what is being shown, and a
/// screenshot of a chart can be paired with the link that reproduces it.
///
/// Written with `replaceState`, not by assigning to `location.hash`:
/// tweaking a filter four times should not put four entries in the
/// history, and Back should leave the page rather than walk backwards
/// through the operator's own adjustments. Navigating *between* pages
/// still uses ordinary `#page` links, so Back does what it always did.

import { createMemo, createSignal, onCleanup } from "solid-js";
import { RANGE_PRESETS, type TimeRange } from "./ui";

/// The hash, minus its `#`, as a signal the whole console reads.
///
/// A plain module-level signal rather than a context: there is one
/// address bar, every page reads the same one, and threading a provider
/// through the tree would buy nothing.
const [hash, setHash] = createSignal(location.hash.slice(1));

addEventListener("hashchange", () => setHash(location.hash.slice(1)));

/// The part before the query — which page is open.
export function hashPath(): string {
  return hash().split("?")[0];
}

function params(): URLSearchParams {
  const [, ...rest] = hash().split("?");
  return new URLSearchParams(rest.join("?"));
}

/// Rewrite the hash from a path and a set of parameters.
function write(path: string, next: URLSearchParams): void {
  const query = next.toString();
  const target = `#${path}${query ? `?${query}` : ""}`;
  if (target === location.hash) return;
  history.replaceState(history.state, "", target);
  setHash(target.slice(1));
}

/// Point the URL at `path`, keeping whatever filters are on it.
///
/// For normalising an alias or an empty hash onto the page actually
/// being rendered, so the filters written next have somewhere to hang.
export function setHashPath(path: string): void {
  write(path, params());
}

/// Apply changes to the hash's parameters. An empty value removes the
/// parameter rather than writing `name=`, which keeps a default-valued
/// page at a bare `#usage`.
function patch(changes: Record<string, string>): void {
  const next = params();
  for (const [name, value] of Object.entries(changes)) {
    if (value) next.set(name, value);
    else next.delete(name);
  }
  write(hashPath(), next);
}

/// A getter that only reports a change when the value really changed.
///
/// Everything below derives from one signal — the hash — so any write
/// recomputes every getter and hands back a fresh object. Resources
/// keyed on those objects would then refetch over an unrelated filter:
/// typing in the log search box would re-query the gateway for records
/// the search never touches. Comparing by value stops that at the
/// source rather than at each of a dozen call sites.
function stable<T>(compute: () => T): () => T {
  return createMemo(compute, undefined, {
    equals: (a, b) => JSON.stringify(a) === JSON.stringify(b),
  });
}

/// One query parameter, read and written like a signal.
///
/// The setter stores nothing when the value is the default, so a page
/// showing its defaults has a clean URL and the parameters present are
/// exactly the choices somebody made.
export function urlParam(
  name: string,
  fallback = "",
): [() => string, (value: string) => void] {
  return [
    stable(() => params().get(name) || fallback),
    (value) => patch({ [name]: value === fallback ? "" : value }),
  ];
}

/// How long typing has to pause before the URL catches up.
const SEARCH_SETTLE_MS = 350;

/// A parameter that is typed rather than picked.
///
/// The field updates on the keystroke and the URL a moment later,
/// because Safari rate-limits `replaceState` to a hundred calls per
/// thirty seconds — a limit one sentence in a search box would cross,
/// and past it the address bar silently stops tracking the page.
///
/// While a write is pending the local value wins; once it lands the URL
/// takes authority back, so a Back or a page change is never fighting a
/// stale copy held here.
export function urlSearch(name = "q"): [() => string, (value: string) => void] {
  // Wrapped rather than a bare `string | undefined`, so "nothing
  // pending" and "pending, and it is the empty string" stay distinct.
  const [pending, setPending] = createSignal<{ text: string } | null>(null);
  let timer: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(timer));
  return [
    stable(() => pending()?.text ?? params().get(name) ?? ""),
    (value) => {
      setPending({ text: value });
      clearTimeout(timer);
      timer = setTimeout(() => {
        patch({ [name]: value });
        setPending(null);
      }, SEARCH_SETTLE_MS);
    },
  ];
}

/// The caller dimensions on the log page, as `meta.*` parameters.
///
/// An open set rather than named fields: which dimensions exist is
/// gateway config, so this reads whatever `meta.` prefixed parameters
/// are on the URL instead of naming them here. That keeps a filtered log
/// — "the ICD_EXTRACTION step of this workflow, for this chart" —
/// linkable in the same way a filtered chart already is, which is most
/// of what anyone wants to do once they have found the requests they
/// were looking for.
export function urlMeta(): [
  () => Record<string, string>,
  (next: Record<string, string>) => void,
] {
  return [
    stable(() => {
      const out: Record<string, string> = {};
      for (const [name, value] of params()) {
        if (name.startsWith("meta.") && value) out[name.slice(5)] = value;
      }
      return out;
    }),
    (next) => {
      // Every currently-set dimension is cleared first, so removing one
      // removes it from the URL rather than leaving a stale parameter
      // behind that the next read would resurrect.
      const changes: Record<string, string> = {};
      for (const [name] of params()) {
        if (name.startsWith("meta.")) changes[name] = "";
      }
      for (const [key, value] of Object.entries(next)) changes[`meta.${key}`] = value;
      patch(changes);
    },
  ];
}

/// The provider / model / key trio the observability pages share, moved
/// as a unit so one write covers a change to any of them.
export type DimFilters = { provider: string; model: string; key: string };

export function urlFilters(): [() => DimFilters, (next: DimFilters) => void] {
  return [
    stable(() => {
      const current = params();
      return {
        provider: current.get("provider") ?? "",
        model: current.get("model") ?? "",
        key: current.get("key") ?? "",
      };
    }),
    (next) => patch({ provider: next.provider, model: next.model, key: next.key }),
  ];
}

/// A time range in the URL: `24h`, `30d`, or `2026-08-01..2026-08-25`.
///
/// Units rather than raw seconds because this is a thing people read and
/// edit by hand — `range=7d` is obvious in a way `range=604800` is not.
export function urlRange(
  fallback: TimeRange,
  name = "range",
): [() => TimeRange, (value: TimeRange) => void] {
  return [
    stable(() => decodeRange(params().get(name) ?? "") ?? fallback),
    (value) => {
      const encoded = encodeRange(value);
      patch({ [name]: encoded === encodeRange(fallback) ? "" : encoded });
    },
  ];
}

function encodeRange(range: TimeRange): string {
  if (range.kind === "absolute") return `${range.start}..${range.end}`;
  const { seconds } = range;
  if (seconds % 86400 === 0) return `${seconds / 86400}d`;
  if (seconds % 3600 === 0) return `${seconds / 3600}h`;
  return `${seconds}s`;
}

const DAY_PATTERN = /^\d{4}-\d{2}-\d{2}$/;

function decodeRange(raw: string): TimeRange | undefined {
  if (!raw) return undefined;
  const [start, end] = raw.split("..");
  if (end && DAY_PATTERN.test(start) && DAY_PATTERN.test(end)) {
    return { kind: "absolute", start, end };
  }
  const relative = /^(\d+)([hds])$/.exec(raw);
  if (!relative) return undefined;
  const count = Number(relative[1]);
  if (!count) return undefined;
  const seconds = count * { s: 1, h: 3600, d: 86400 }[relative[2] as "s" | "h" | "d"];
  return { kind: "relative", seconds, label: rangeLabel(seconds) };
}

/// What the picker calls this window, so a hand-typed `range=3d` still
/// shows a sentence rather than a number of seconds.
function rangeLabel(seconds: number): string {
  const preset = RANGE_PRESETS.find((option) => option.seconds === seconds);
  if (preset) return preset.label;
  if (seconds % 86400 === 0) {
    const days = seconds / 86400;
    return `Last ${days} ${days === 1 ? "day" : "days"}`;
  }
  const hours = Math.max(1, Math.round(seconds / 3600));
  return `Last ${hours} ${hours === 1 ? "hour" : "hours"}`;
}
