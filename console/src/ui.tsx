/// Shared controls. These exist because the same three shapes kept being
/// approximated with a bare `<input>`: pick one of a known set, pick
/// several of a known set, and inspect one row without losing the list.
import { CalendarDays, Check, ChevronDown, Filter, Search, X } from "lucide-solid";
import { For, Match, Show, Switch, createEffect, createMemo, createSignal, onCleanup, onMount } from "solid-js";
import { Portal } from "solid-js/web";

export type Option = { value: string; label: string; hint?: string; icon?: any };

/// Close when the pointer goes down outside every element in `parts`, and
/// on Escape. Both are needed: a click-away alone leaves a keyboard user
/// with no way to dismiss the popup without choosing something.
///
/// `parts` is a list rather than one element because a popup is portalled
/// out of its trigger's subtree — a `contains` check against the trigger
/// alone would treat clicking the popup as clicking away, closing it
/// before the click landed.
function dismissable(
  parts: () => Array<HTMLElement | undefined>,
  close: () => void,
  /// Treat a click inside any element matching this selector as inside.
  ///
  /// The filter panel hosts comboboxes whose popups are portalled to the
  /// body, so they are not descendants of the panel: without this, picking
  /// a filter value closes the panel out from under the click and the
  /// choice never lands.
  alsoInside?: string,
) {
  const onPointer = (event: PointerEvent) => {
    const target = event.target as Node;
    if (parts().some((el) => el?.contains(target))) return;
    if (alsoInside && target instanceof Element && target.closest(alsoInside)) return;
    close();
  };
  const onKey = (event: KeyboardEvent) => {
    if (event.key === "Escape") close();
  };
  document.addEventListener("pointerdown", onPointer);
  document.addEventListener("keydown", onKey);
  onCleanup(() => {
    document.removeEventListener("pointerdown", onPointer);
    document.removeEventListener("keydown", onKey);
  });
}

/// Close on Escape — unless a combobox popup is open, in which case the
/// keypress belongs to the popup. Without the guard one Escape collapses
/// two layers at once: the popup closes *and* the dialog under it goes.
///
/// The popup check is a DOM query because popups are portalled: they only
/// exist in the document while open, and the dialog's listener registered
/// first, so it runs while the popup is still present and defers.
export function escapeCloses(close: () => void) {
  const onKey = (event: KeyboardEvent) => {
    if (event.key !== "Escape") return;
    if (document.querySelector(".combobox-popup")) return;
    close();
  };
  document.addEventListener("keydown", onKey);
  onCleanup(() => document.removeEventListener("keydown", onKey));
}

/// Where a popup should sit, in viewport coordinates.
///
/// Popups render in a portal on `document.body` rather than inside their
/// trigger. Any ancestor that scrolls or hides overflow — a dialog with
/// `max-height`, the drawer, a table wrapper, the filter panel — clips an
/// absolutely-positioned child, and there is no combination of `overflow`
/// values that fixes that for every container a control might land in.
/// Fixed positioning off the trigger's rect is clipped by nothing.
function anchorTo(trigger: () => HTMLElement | undefined, open: () => boolean) {
  const [style, setStyle] = createSignal<Record<string, string>>({});
  const place = () => {
    const el = trigger();
    if (!el) return;
    const rect = el.getBoundingClientRect();
    const margin = 8;
    const below = window.innerHeight - rect.bottom - margin;
    const above = rect.top - margin;
    // Flip above the trigger when the space below cannot hold a usable
    // list, which is what happens to a control near the foot of a dialog.
    const flip = below < 220 && above > below;
    const maxHeight = Math.max(160, Math.min(340, flip ? above : below));
    const width = Math.max(rect.width, 240);
    const left = Math.min(Math.max(margin, rect.left), window.innerWidth - width - margin);
    setStyle({
      position: "fixed",
      left: `${left}px`,
      "min-width": `${width}px`,
      "max-height": `${maxHeight}px`,
      ...(flip
        ? { bottom: `${window.innerHeight - rect.top + 4}px` }
        : { top: `${rect.bottom + 4}px` }),
    });
  };
  createEffect(() => {
    if (!open()) return;
    place();
    // Recompute rather than follow: a popup that stays put while the page
    // scrolls under it points at the wrong row.
    window.addEventListener("scroll", place, true);
    window.addEventListener("resize", place);
    onCleanup(() => {
      window.removeEventListener("scroll", place, true);
      window.removeEventListener("resize", place);
    });
  });
  return style;
}

/// Pick one of a known set, with a filter over the options.
///
/// A model id or a virtual key is not free text — it either exists on
/// this gateway or the request fails — so typing one is a chance to make
/// a typo, not a feature.
export function Combobox(props: {
  value: string;
  options: Option[];
  onSelect: (value: string) => void;
  placeholder?: string;
  label?: string;
  allowEmpty?: boolean;
  disabled?: boolean;
}) {
  const [open, setOpen] = createSignal(false);
  const [query, setQuery] = createSignal("");
  let root!: HTMLDivElement;
  let field!: HTMLInputElement;
  const [popup, setPopup] = createSignal<HTMLDivElement>();
  const style = anchorTo(() => root, open);
  onMount(() => dismissable(() => [root, popup()], () => setOpen(false)));

  const shown = createMemo(() => {
    const needle = query().trim().toLowerCase();
    if (!needle) return props.options;
    return props.options.filter(
      (o) =>
        o.label.toLowerCase().includes(needle) ||
        o.value.toLowerCase().includes(needle) ||
        (o.hint ?? "").toLowerCase().includes(needle),
    );
  });
  const current = createMemo(() => props.options.find((o) => o.value === props.value));

  const choose = (value: string) => {
    props.onSelect(value);
    setQuery("");
    setOpen(false);
  };

  return (
    <div class="combobox" ref={root}>
      <button
        type="button"
        class="combobox-trigger"
        disabled={props.disabled}
        aria-haspopup="listbox"
        aria-expanded={open()}
        aria-label={props.label}
        onPointerDown={(e) => {
          e.preventDefault();
          setOpen((v) => !v);
          if (open()) queueMicrotask(() => field?.focus());
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            setOpen((v) => !v);
            if (open()) queueMicrotask(() => field?.focus());
          }
        }}
      >
        <Show when={current()?.icon}><span class="combobox-option-icon">{current()!.icon}</span></Show>
        <span classList={{ "combobox-value": true, placeholder: !current() }}>
          {current()?.label ?? props.placeholder ?? "Select…"}
        </span>
        <ChevronDown size={14} aria-hidden="true" />
      </button>
      <Show when={open()}>
        <Portal>
        <div class="combobox-popup" role="listbox" ref={setPopup} style={style()}>
          <div class="combobox-search">
            <Search size={13} aria-hidden="true" />
            <input
              ref={field}
              autofocus
              value={query()}
              placeholder="Search…"
              aria-label="Search options"
              onInput={(e) => setQuery(e.currentTarget.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && shown().length) {
                  e.preventDefault();
                  choose(shown()[0].value);
                }
              }}
            />
          </div>
          <div class="combobox-list">
            <Show when={props.allowEmpty}>
              <button type="button" class="combobox-option" onClick={() => choose("")}>
                <span class="combobox-option-label">Any</span>
                <Show when={!props.value}><Check size={13} /></Show>
              </button>
            </Show>
            <For each={shown()} fallback={<p class="combobox-empty">No matches</p>}>
              {(option) => (
                <button
                  type="button"
                  class="combobox-option"
                  role="option"
                  aria-selected={option.value === props.value}
                  onClick={() => choose(option.value)}
                >
                  <Show when={option.icon}><span class="combobox-option-icon">{option.icon}</span></Show>
                  <span class="combobox-option-label">
                    {option.label}
                    <Show when={option.hint}><small>{option.hint}</small></Show>
                  </span>
                  <Show when={option.value === props.value}><Check size={13} /></Show>
                </button>
              )}
            </For>
          </div>
        </div>
        </Portal>
      </Show>
    </div>
  );
}

/// Pick several of a known set. Selections show as removable chips, so
/// what is chosen stays readable once there are more than a couple —
/// which a comma-separated text field stops being immediately.
export function MultiCombobox(props: {
  values: string[];
  options: Option[];
  onChange: (values: string[]) => void;
  placeholder?: string;
  label?: string;
  /// Shown when nothing is selected — for a scope field, "none" and
  /// "everything" are opposite meanings and the control must say which.
  emptyMeans?: string;
}) {
  const [open, setOpen] = createSignal(false);
  const [query, setQuery] = createSignal("");
  let root!: HTMLDivElement;
  const [popup, setPopup] = createSignal<HTMLDivElement>();
  const style = anchorTo(() => root, open);
  onMount(() => dismissable(() => [root, popup()], () => setOpen(false)));

  const shown = createMemo(() => {
    const needle = query().trim().toLowerCase();
    const pool = props.options;
    if (!needle) return pool;
    return pool.filter(
      (o) => o.label.toLowerCase().includes(needle) || (o.hint ?? "").toLowerCase().includes(needle),
    );
  });

  const toggle = (value: string) => {
    props.onChange(
      props.values.includes(value)
        ? props.values.filter((v) => v !== value)
        : [...props.values, value],
    );
  };

  return (
    <div class="combobox" ref={root}>
      <div class="multi-field">
        <For each={props.values}>
          {(value) => (
            <span class="chip">
              {props.options.find((o) => o.value === value)?.label ?? value}
              <button
                type="button"
                aria-label={`Remove ${value}`}
                onPointerDown={(e) => {
                  e.preventDefault();
                  props.onChange(props.values.filter((v) => v !== value));
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    props.onChange(props.values.filter((v) => v !== value));
                  }
                }}
              >
                <X size={11} />
              </button>
            </span>
          )}
        </For>
        <button
          type="button"
          class="multi-add"
          aria-haspopup="listbox"
          aria-expanded={open()}
          aria-label={props.label ?? "Add"}
          onPointerDown={(e) => {
            e.preventDefault();
            setOpen((v) => !v);
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              setOpen((v) => !v);
            }
          }}
        >
          {props.values.length ? "Add…" : (props.emptyMeans ?? props.placeholder ?? "Select…")}
          <ChevronDown size={13} aria-hidden="true" />
        </button>
      </div>
      <Show when={open()}>
        <Portal>
        <div class="combobox-popup" role="listbox" aria-multiselectable="true" ref={setPopup} style={style()}>
          <div class="combobox-search">
            <Search size={13} aria-hidden="true" />
            <input
              autofocus
              value={query()}
              placeholder="Search…"
              aria-label="Search options"
              onInput={(e) => setQuery(e.currentTarget.value)}
            />
          </div>
          <div class="combobox-list">
            <For each={shown()} fallback={<p class="combobox-empty">No matches</p>}>
              {(option) => (
                <button
                  type="button"
                  class="combobox-option"
                  role="option"
                  aria-selected={props.values.includes(option.value)}
                  onClick={() => toggle(option.value)}
                >
                  <span class="combobox-option-label">
                    {option.label}
                    <Show when={option.hint}><small>{option.hint}</small></Show>
                  </span>
                  <Show when={props.values.includes(option.value)}><Check size={13} /></Show>
                </button>
              )}
            </For>
          </div>
        </div>
        </Portal>
      </Show>
    </div>
  );
}

/// A right-hand drawer for inspecting one row of a list.
///
/// Replaces the master/detail split this console used to have: at rest
/// that layout spent half the width on a permanently-open detail pane
/// which was empty until something was picked, and squeezed the list it
/// was meant to serve.
export function Drawer(props: {
  open: boolean;
  title: string;
  subtitle?: string;
  onClose: () => void;
  actions?: any;
  children?: any;
}) {
  createEffect(() => {
    if (!props.open) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (document.querySelector(".combobox-popup")) return;
      props.onClose();
    };
    document.addEventListener("keydown", onKey);
    onCleanup(() => document.removeEventListener("keydown", onKey));
  });
  return (
    <Show when={props.open}>
      <div
        class="drawer-backdrop"
        role="presentation"
        onMouseDown={(e) => {
          if (e.target === e.currentTarget) props.onClose();
        }}
      >
        <aside class="drawer" role="dialog" aria-modal="true" aria-label={props.title}>
          <header class="drawer-head">
            <div class="drawer-title">
              <h2>{props.title}</h2>
              <Show when={props.subtitle}><p>{props.subtitle}</p></Show>
            </div>
            <div class="drawer-actions">
              {props.actions}
              <button class="icon-button" aria-label="Close" title="Close (Esc)" onClick={props.onClose}>
                <X size={16} />
              </button>
            </div>
          </header>
          <div class="drawer-body">{props.children}</div>
        </aside>
      </div>
    </Show>
  );
}

/// A time range that means the same thing every time it is read.
///
/// Relative presets resolve against "now" at render; absolute ranges are
/// dates the operator typed. Every consumer receives the *resolved*
/// endpoints and shows them, so "Last 7 days" is never a mystery window —
/// the control itself says "Aug 10 → Aug 17".
export type TimeRange =
  | { kind: "relative"; seconds: number; label: string }
  | { kind: "absolute"; start: string; end: string };

export type ResolvedRange = {
  startMs: number;
  endMs: number;
  /// Whole days covered, for the day-bucketed history endpoint.
  days: number;
  /// True when the range fits the minute-bucketed live aggregate.
  live: boolean;
  label: string;
  detail: string;
};

export const RANGE_PRESETS: Array<{ label: string; seconds: number }> = [
  { label: "Last hour", seconds: 3600 },
  { label: "Last 24 hours", seconds: 86400 },
  { label: "Last 7 days", seconds: 7 * 86400 },
  { label: "Last 30 days", seconds: 30 * 86400 },
];

const DAY_MS = 86_400_000;

function fmtDay(ms: number): string {
  return new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export function resolveRange(range: TimeRange): ResolvedRange {
  const now = Date.now();
  if (range.kind === "relative") {
    const startMs = now - range.seconds * 1000;
    return {
      startMs,
      endMs: now,
      days: Math.max(1, Math.ceil(range.seconds / 86400) + 1),
      live: range.seconds <= 86400,
      label: range.label,
      detail: range.seconds <= 86400
        ? `${new Date(startMs).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })} → now`
        : `${fmtDay(startMs)} → now`,
    };
  }
  const startMs = Date.parse(`${range.start}T00:00:00`);
  const endMs = Math.min(Date.parse(`${range.end}T23:59:59.999`), now);
  return {
    startMs,
    endMs,
    days: Math.max(1, Math.ceil((now - startMs) / DAY_MS) + 1),
    live: false,
    label: `${fmtDay(startMs)} → ${fmtDay(endMs)}`,
    detail: "custom range",
  };
}

/// Month to date, materialised as an absolute range at click time so it
/// stays what it said even as the month rolls on.
function monthToDate(): TimeRange {
  const now = new Date();
  const pad = (n: number) => String(n).padStart(2, "0");
  const first = `${now.getFullYear()}-${pad(now.getMonth() + 1)}-01`;
  const today = `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`;
  return { kind: "absolute", start: first, end: today };
}

/// The range control: presets on the left, explicit dates on the right,
/// and a trigger that always states the resolved range.
export function RangePicker(props: { value: TimeRange; onChange: (value: TimeRange) => void }) {
  const [open, setOpen] = createSignal(false);
  const [start, setStart] = createSignal("");
  const [end, setEnd] = createSignal("");
  let root!: HTMLDivElement;
  const [popup, setPopup] = createSignal<HTMLDivElement>();
  const style = anchorTo(() => root, open);
  onMount(() => dismissable(() => [root, popup()], () => setOpen(false)));
  const resolved = createMemo(() => resolveRange(props.value));

  const pick = (range: TimeRange) => {
    props.onChange(range);
    setOpen(false);
  };

  return (
    <div class="combobox" ref={root}>
      <button
        type="button"
        class="range-trigger"
        aria-haspopup="dialog"
        aria-expanded={open()}
        aria-label="Time range"
        onPointerDown={(e) => {
          e.preventDefault();
          setOpen((v) => !v);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            setOpen((v) => !v);
          }
        }}
      >
        <CalendarDays size={13} aria-hidden="true" />
        <strong>{resolved().label}</strong>
        <span>{resolved().detail}</span>
        <ChevronDown size={13} aria-hidden="true" />
      </button>
      <Show when={open()}>
        <Portal>
          <div class="combobox-popup range-popup" role="dialog" aria-label="Pick a time range" ref={setPopup} style={style()}>
            <div class="range-presets">
              <For each={RANGE_PRESETS}>{(preset) => (
                <button
                  type="button"
                  class="combobox-option"
                  aria-selected={props.value.kind === "relative" && props.value.seconds === preset.seconds}
                  onClick={() => pick({ kind: "relative", seconds: preset.seconds, label: preset.label })}
                >
                  <span class="combobox-option-label">{preset.label}</span>
                  <Show when={props.value.kind === "relative" && (props.value as any).seconds === preset.seconds}><Check size={13} /></Show>
                </button>
              )}</For>
              <button type="button" class="combobox-option" onClick={() => pick(monthToDate())}>
                <span class="combobox-option-label">Month to date</span>
              </button>
            </div>
            <div class="range-custom">
              <p>Start and end dates</p>
              <label>From<input type="date" value={start()} onInput={(e) => setStart(e.currentTarget.value)} /></label>
              <label>To<input type="date" value={end()} onInput={(e) => setEnd(e.currentTarget.value)} /></label>
              <button
                type="button"
                class="button primary"
                disabled={!start() || !end() || start() > end()}
                onClick={() => pick({ kind: "absolute", start: start(), end: end() })}
              >
                Apply
              </button>
            </div>
          </div>
        </Portal>
      </Show>
    </div>
  );
}

export type FilterSpec = {
  id: string;
  label: string;
  options: Option[];
  /// Empty string = no constraint.
  value: string;
  onChange: (value: string) => void;
};

/// The toolbar above a list: filters and a range on the left, search hard
/// right, full width.
///
/// The filters live behind one button rather than strung along the bar,
/// because the number of them varies by page and a row of naked selects
/// pushes the search around as they come and go. The button carries a
/// count so an active filter is never invisible — a list silently
/// excluding rows is worse than no filter at all.
export function FilterBar(props: {
  filters: FilterSpec[];
  search: string;
  onSearch: (value: string) => void;
  searchPlaceholder?: string;
  range?: { value: string; options: Option[]; onChange: (value: string) => void };
  extra?: any;
}) {
  const [open, setOpen] = createSignal(false);
  let panel!: HTMLDivElement;
  const [popup, setPopup] = createSignal<HTMLDivElement>();
  const style = anchorTo(() => panel, open);
  onMount(() => dismissable(() => [panel, popup()], () => setOpen(false), ".combobox-popup"));
  const active = createMemo(() => props.filters.filter((f) => f.value).length);

  return (
    <div class="filter-bar">
      <div class="search-field">
        <Search size={14} aria-hidden="true" />
        <input
          data-filter
          value={props.search}
          placeholder={props.searchPlaceholder ?? "Search (press /)"}
          aria-label="Search"
          onInput={(e) => props.onSearch(e.currentTarget.value)}
        />
        <Show when={props.search}>
          <button type="button" aria-label="Clear search" onClick={() => props.onSearch("")}>
            <X size={13} />
          </button>
        </Show>
      </div>
      <div class="filter-bar-left">
        <Show when={props.filters.length}>
        <div class="combobox" ref={panel}>
          <button
            type="button"
            class="button outline"
            aria-expanded={open()}
            onClick={() => setOpen((v) => !v)}
          >
            <Filter size={14} aria-hidden="true" />
            Filters
            <Show when={active()}><span class="chip-count">{active()}</span></Show>
          </button>
          <Show when={open()}>
            <Portal>
            <div class="combobox-popup wide" role="group" aria-label="Filters" ref={setPopup} style={style()}>
              <For each={props.filters}>
                {(filter) => (
                  <div class="filter-row">
                    <span>{filter.label}</span>
                    <Combobox
                      value={filter.value}
                      options={filter.options}
                      onSelect={filter.onChange}
                      label={filter.label}
                      placeholder="Any"
                      allowEmpty
                    />
                  </div>
                )}
              </For>
              <Show when={active()}>
                <button
                  type="button"
                  class="button ghost filter-clear"
                  onClick={() => props.filters.forEach((f) => f.onChange(""))}
                >
                  Clear all
                </button>
              </Show>
            </div>
            </Portal>
          </Show>
        </div>
        </Show>
        <For each={props.filters.filter((f) => f.value)}>{(filter) => (
          <span class="chip filter-chip">
            <span class="filter-chip-name">{filter.label}</span>
            {filter.options.find((o) => o.value === filter.value)?.label ?? filter.value}
            <button type="button" aria-label={`Clear ${filter.label} filter`} onClick={() => filter.onChange("")}>
              <X size={11} />
            </button>
          </span>
        )}</For>
        <Show when={props.range}>
          {(range) => (
            <div class="segmented" role="group" aria-label="Date range">
              <For each={range().options}>
                {(option) => (
                  <button
                    aria-pressed={range().value === option.value}
                    onClick={() => range().onChange(option.value)}
                  >
                    {option.label}
                  </button>
                )}
              </For>
            </div>
          )}
        </Show>
      </div>
      <Show when={props.extra}><div class="filter-bar-actions">{props.extra}</div></Show>
    </div>
  );
}

/// A placeholder for content that is on its way.
///
/// An empty state and a loading state look identical if loading renders
/// nothing, and "No virtual keys" on a page that is still fetching is a
/// lie the operator acts on. These shapes say "something is coming"
/// without pretending to know what.
export function Skeleton(props: { rows?: number; variant?: "table" | "stats" | "cards" | "chart" }) {
  const rows = () => props.rows ?? 5;
  return <Switch fallback={
    <div class="skeleton-table" aria-hidden="true">
      <For each={Array.from({ length: rows() })}>{() => <div class="skeleton-row"><span /><span /><span /></div>}</For>
    </div>
  }>
    <Match when={props.variant === "stats"}>
      <div class="skeleton-stats" aria-hidden="true">
        <For each={Array.from({ length: rows() })}>{() => <div class="skeleton-stat"><span /><span /></div>}</For>
      </div>
    </Match>
    <Match when={props.variant === "chart"}>
      <div class="skeleton-chart" aria-hidden="true" />
    </Match>
    <Match when={props.variant === "cards"}>
      <div class="skeleton-cards" aria-hidden="true">
        <For each={Array.from({ length: rows() })}>{() => <div class="skeleton-card" />}</For>
      </div>
    </Match>
  </Switch>;
}

/// Content, its skeleton, and its empty state — in the one order that
/// tells the truth: loading first, then "nothing here", then the data.
export function Loading(props: {
  when: boolean;
  skeleton?: "table" | "stats" | "cards" | "chart";
  rows?: number;
  children: any;
}) {
  return <Show when={!props.when} fallback={<Skeleton variant={props.skeleton} rows={props.rows} />}>
    {props.children}
  </Show>;
}
