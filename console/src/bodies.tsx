/// Reading a captured request body, rather than dumping it.
///
/// The gateway speaks three inbound dialects and they disagree about
/// where everything lives: Chat Completions puts the whole conversation
/// in `messages`, the Responses API splits it into `instructions` plus an
/// `input` array of mixed item types, and Anthropic keeps `system` out of
/// `messages` and models tool calls as content blocks. A drawer that
/// pretty-prints JSON makes the reader do that translation by eye, every
/// time.
///
/// This module normalizes all three into one shape (`Conversation`) and
/// renders it as a transcript. The raw JSON stays one click away, because
/// the parsed view is an interpretation and the bytes are the truth.

import { For, Show, createMemo, createSignal } from "solid-js";
import { Copy } from "lucide-solid";

export type Attachment = {
  kind: "image" | "document" | "audio";
  /** What to show when there is nothing to render — a filename or a type. */
  label: string;
  /** Inline data or a remote URL; images with one are rendered. */
  url?: string;
  mediaType?: string;
};

export type ToolCall = { id: string; name: string; args: string };

export type Turn = {
  role: "system" | "user" | "assistant" | "tool" | "developer";
  text: string;
  attachments: Attachment[];
  toolCalls: ToolCall[];
  /** Which call a `tool` turn answers. */
  toolCallId?: string;
};

export type Conversation = {
  dialect: "chat" | "anthropic" | "responses" | "unknown";
  turns: Turn[];
  tools: Array<{ name: string; description?: string }>;
};

const EMPTY_TURN = (role: Turn["role"]): Turn => ({
  role,
  text: "",
  attachments: [],
  toolCalls: [],
});

function isRole(value: unknown): value is Turn["role"] {
  return (
    value === "system" ||
    value === "user" ||
    value === "assistant" ||
    value === "tool" ||
    value === "developer"
  );
}

/// Which dialect a body is, decided by the fields only that dialect has.
///
/// Order matters: an Anthropic body also has `messages`, so it has to be
/// tested before Chat Completions rather than after.
function detect(body: any): Conversation["dialect"] {
  if (Array.isArray(body?.input) || typeof body?.instructions === "string") return "responses";
  if (body?.system !== undefined || body?.anthropic_version !== undefined) return "anthropic";
  if (Array.isArray(body?.messages)) return "chat";
  return "unknown";
}

/// A `data:` URI's media type, for labelling an attachment we cannot show.
function mediaTypeOf(url: string): string | undefined {
  if (!url.startsWith("data:")) return undefined;
  const header = url.slice(5).split(",")[0] ?? "";
  return header.split(";")[0] || undefined;
}

function attachmentFromUrl(url: string, kind: Attachment["kind"], name?: string): Attachment {
  const mediaType = mediaTypeOf(url);
  return { kind, url, mediaType, label: name ?? mediaType ?? kind };
}

/// Content parts, in whichever spelling arrived. The three dialects use
/// different `type` names for identical things, so they are folded here
/// rather than in three near-identical parsers.
function readParts(parts: any[], turn: Turn): void {
  const text: string[] = [];
  for (const part of parts) {
    const type = part?.type;
    if (type === "text" || type === "input_text" || type === "output_text") {
      if (typeof part.text === "string") text.push(part.text);
    } else if (type === "image_url" || type === "input_image") {
      const url = typeof part.image_url === "string" ? part.image_url : part.image_url?.url;
      if (typeof url === "string") turn.attachments.push(attachmentFromUrl(url, "image"));
    } else if (type === "image") {
      // Anthropic: base64 or url under `source`.
      const source = part.source ?? {};
      const url =
        source.type === "base64"
          ? `data:${source.media_type ?? "image/png"};base64,${source.data ?? ""}`
          : source.url;
      if (typeof url === "string") turn.attachments.push(attachmentFromUrl(url, "image"));
    } else if (type === "file" || type === "input_file") {
      const file = part.file ?? part;
      const url = file.file_data ?? file.file_url;
      turn.attachments.push(
        typeof url === "string"
          ? attachmentFromUrl(url, "document", file.filename)
          : { kind: "document", label: file.filename ?? file.file_id ?? "document" },
      );
    } else if (type === "document") {
      const source = part.source ?? {};
      const url =
        source.type === "base64"
          ? `data:${source.media_type ?? "application/pdf"};base64,${source.data ?? ""}`
          : source.url;
      turn.attachments.push(
        typeof url === "string"
          ? attachmentFromUrl(url, "document", part.title)
          : { kind: "document", label: part.title ?? "document" },
      );
    } else if (type === "input_audio") {
      turn.attachments.push({ kind: "audio", label: "audio" });
    } else if (type === "tool_use") {
      turn.toolCalls.push({
        id: part.id ?? "",
        name: part.name ?? "",
        args: JSON.stringify(part.input ?? {}, null, 2),
      });
    }
  }
  turn.text = [turn.text, ...text].filter(Boolean).join("\n");
}

/// Anthropic's `tool_result` is a content block on a *user* turn, but it
/// reads as its own turn — so it is lifted into one, which is also how
/// the other two dialects model it.
function readAnthropicBlocks(blocks: any[], role: Turn["role"], out: Turn[]): void {
  const turn = EMPTY_TURN(role);
  const plain: any[] = [];
  for (const block of blocks) {
    if (block?.type === "tool_result") {
      const result = EMPTY_TURN("tool");
      result.toolCallId = block.tool_use_id;
      if (typeof block.content === "string") result.text = block.content;
      else if (Array.isArray(block.content)) readParts(block.content, result);
      out.push(result);
    } else {
      plain.push(block);
    }
  }
  readParts(plain, turn);
  if (turn.text || turn.attachments.length || turn.toolCalls.length) out.push(turn);
}

function readContent(content: unknown, turn: Turn): void {
  if (typeof content === "string") turn.text = content;
  else if (Array.isArray(content)) readParts(content, turn);
}

/// Normalize any captured request body into a transcript.
///
/// Never throws: a body that does not match a known shape yields an empty
/// conversation, and the caller falls back to the JSON view. A parser
/// that threw would blank the drawer for exactly the unusual requests
/// someone opened the drawer to understand.
export function parseConversation(raw: string): Conversation {
  let body: any;
  try {
    body = JSON.parse(raw);
  } catch {
    return { dialect: "unknown", turns: [], tools: [] };
  }
  const dialect = detect(body);
  const turns: Turn[] = [];

  // The system prompt, wherever this dialect keeps it.
  if (dialect === "anthropic" && body.system !== undefined) {
    const turn = EMPTY_TURN("system");
    if (typeof body.system === "string") turn.text = body.system;
    else if (Array.isArray(body.system)) readParts(body.system, turn);
    if (turn.text || turn.attachments.length) turns.push(turn);
  }
  if (dialect === "responses" && typeof body.instructions === "string" && body.instructions) {
    turns.push({ ...EMPTY_TURN("system"), text: body.instructions });
  }

  const items: any[] = Array.isArray(body.messages)
    ? body.messages
    : Array.isArray(body.input)
      ? body.input
      : [];

  for (const item of items) {
    // Responses carries tool activity as top-level items, not as turns.
    if (item?.type === "function_call") {
      turns.push({
        ...EMPTY_TURN("assistant"),
        toolCalls: [{ id: item.call_id ?? "", name: item.name ?? "", args: item.arguments ?? "{}" }],
      });
      continue;
    }
    if (item?.type === "function_call_output") {
      turns.push({ ...EMPTY_TURN("tool"), toolCallId: item.call_id, text: String(item.output ?? "") });
      continue;
    }
    if (item?.type === "reasoning") continue;

    const role = isRole(item?.role) ? item.role : "user";
    if (dialect === "anthropic" && Array.isArray(item?.content)) {
      readAnthropicBlocks(item.content, role, turns);
      continue;
    }
    const turn = EMPTY_TURN(role);
    if (typeof item?.tool_call_id === "string") turn.toolCallId = item.tool_call_id;
    readContent(item?.content, turn);
    for (const call of item?.tool_calls ?? []) {
      turn.toolCalls.push({
        id: call?.id ?? "",
        name: call?.function?.name ?? call?.name ?? "",
        args: call?.function?.arguments ?? "{}",
      });
    }
    if (turn.text || turn.attachments.length || turn.toolCalls.length || turn.role === "tool") {
      turns.push(turn);
    }
  }

  const tools = (body.tools ?? []).map((tool: any) => ({
    name: tool?.function?.name ?? tool?.name ?? "tool",
    description: tool?.function?.description ?? tool?.description,
  }));

  return { dialect, turns, tools };
}

/// The assistant's answer, pulled out of a response body.
///
/// Used for the rendered view of the output tab; the JSON view is the
/// fallback and always available.
export function parseAnswer(raw: string): Turn[] {
  let body: any;
  try {
    body = JSON.parse(raw);
  } catch {
    return [];
  }
  const turn = EMPTY_TURN("assistant");

  // Chat Completions.
  const message = body?.choices?.[0]?.message;
  if (message) {
    readContent(message.content, turn);
    for (const call of message.tool_calls ?? []) {
      turn.toolCalls.push({
        id: call?.id ?? "",
        name: call?.function?.name ?? "",
        args: call?.function?.arguments ?? "{}",
      });
    }
  }
  // Anthropic.
  if (Array.isArray(body?.content)) readParts(body.content, turn);
  // Responses.
  for (const item of body?.output ?? []) {
    if (item?.type === "message") readParts(item.content ?? [], turn);
    else if (item?.type === "function_call") {
      turn.toolCalls.push({
        id: item.call_id ?? "",
        name: item.name ?? "",
        args: item.arguments ?? "{}",
      });
    }
  }
  return turn.text || turn.attachments.length || turn.toolCalls.length ? [turn] : [];
}

// ---------------------------------------------------------------------------
// JSON viewer
// ---------------------------------------------------------------------------

/// Above this, syntax highlighting is skipped and the text shown plain.
///
/// A captured body can be megabytes of base64 (a rasterized chart is
/// exactly that), and tokenizing it would build hundreds of thousands of
/// DOM nodes and lock the tab. The reader still gets the bytes.
const HIGHLIGHT_LIMIT = 400_000;

type Token = { text: string; cls: string };

/// Tokenize JSON for display.
///
/// Deliberately a lexer over the pretty-printed text rather than a walk
/// of the parsed value: it keeps the caller's own key order and spacing,
/// and it still produces something readable when the body is *not* valid
/// JSON (a truncated capture, an upstream error page).
const JSON_TOKEN =
  /("(?:\\.|[^"\\])*")(\s*:)?|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)|\b(true|false)\b|\b(null)\b/g;

function tokenize(text: string): Token[] {
  const out: Token[] = [];
  let last = 0;
  for (const match of text.matchAll(JSON_TOKEN)) {
    const at = match.index ?? 0;
    if (at > last) out.push({ text: text.slice(last, at), cls: "jp" });
    const [whole, str, colon, num, bool, nul] = match;
    if (str !== undefined) {
      out.push({ text: str, cls: colon ? "jk" : "js" });
      if (colon) out.push({ text: colon, cls: "jp" });
    } else if (num !== undefined) out.push({ text: num, cls: "jn" });
    else if (bool !== undefined) out.push({ text: bool, cls: "jb" });
    else if (nul !== undefined) out.push({ text: nul, cls: "jz" });
    last = at + whole.length;
  }
  if (last < text.length) out.push({ text: text.slice(last), cls: "jp" });
  return out;
}

/// A read-only JSON view: line numbers, syntax colour, copy, and wrap.
///
/// Not an editor. Monaco and CodeMirror both cost more gzipped than this
/// whole console is allowed to weigh (the bundle budget is a CI gate), and
/// every editor feature beyond "read it and copy it" is one nobody needs
/// on a log body.
export function JsonView(props: { text: string; label?: string }) {
  const [wrap, setWrap] = createSignal(false);
  const [copied, setCopied] = createSignal(false);

  // Re-indent when the capture is minified, which it usually is; leave it
  // alone when it is not valid JSON so a truncated body still shows.
  const pretty = createMemo(() => {
    try {
      return JSON.stringify(JSON.parse(props.text), null, 2);
    } catch {
      return props.text;
    }
  });
  const lines = createMemo(() => pretty().split("\n"));
  const big = createMemo(() => pretty().length > HIGHLIGHT_LIMIT);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(pretty());
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    } catch {
      /* clipboard unavailable; the text is selectable either way */
    }
  };

  return <div class="json-view">
    <div class="json-toolbar">
      <span class="muted">{props.label ?? "JSON"} · {lines().length} lines</span>
      <span class="json-actions">
        <button type="button" class="button ghost sm" aria-pressed={wrap()} onClick={() => setWrap(!wrap())}>
          {wrap() ? "No wrap" : "Wrap"}
        </button>
        <button type="button" class="button ghost sm" onClick={copy}>
          <Copy size={13} /> {copied() ? "Copied" : "Copy"}
        </button>
      </span>
    </div>
    <Show
      when={!big()}
      fallback={<pre class="json-body plain" classList={{ wrap: wrap() }}>{pretty()}</pre>}
    >
      <div class="json-body" classList={{ wrap: wrap() }}>
        <For each={lines()}>{(line, i) => <div class="json-line">
          <span class="json-gutter" aria-hidden="true">{i() + 1}</span>
          <code>
            <For each={tokenize(line)}>{(token) => <span class={token.cls}>{token.text}</span>}</For>
          </code>
        </div>}</For>
      </div>
    </Show>
  </div>;
}

// ---------------------------------------------------------------------------
// Transcript
// ---------------------------------------------------------------------------

const ROLE_LABEL: Record<Turn["role"], string> = {
  system: "System",
  developer: "Developer",
  user: "User",
  assistant: "Assistant",
  tool: "Tool result",
};

function AttachmentCard(props: { attachment: Attachment }) {
  const [open, setOpen] = createSignal(false);
  const isImage = () => props.attachment.kind === "image" && Boolean(props.attachment.url);
  return <div class="attachment" classList={{ expandable: isImage() }}>
    <Show when={isImage()} fallback={
      <div class="attachment-chip">
        <span class="attachment-kind">{props.attachment.kind}</span>
        <span class="mono">{props.attachment.label}</span>
      </div>
    }>
      <button type="button" class="attachment-thumb" onClick={() => setOpen(!open())}>
        <img src={props.attachment.url} alt={props.attachment.label} classList={{ full: open() }} />
      </button>
    </Show>
  </div>;
}

function TurnCard(props: { turn: Turn }) {
  return <article class={`turn turn-${props.turn.role}`}>
    <header>
      <span class="turn-role">{ROLE_LABEL[props.turn.role]}</span>
      <Show when={props.turn.toolCallId}>
        <span class="mono muted">{props.turn.toolCallId}</span>
      </Show>
    </header>
    <Show when={props.turn.text}>
      <div class="turn-text">{props.turn.text}</div>
    </Show>
    <Show when={props.turn.attachments.length}>
      <div class="attachments">
        <For each={props.turn.attachments}>{(a) => <AttachmentCard attachment={a} />}</For>
      </div>
    </Show>
    <Show when={props.turn.toolCalls.length}>
      <div class="tool-calls">
        <For each={props.turn.toolCalls}>{(call) => <div class="tool-call">
          <header><span class="tool-name mono">{call.name}</span><span class="mono muted">{call.id}</span></header>
          <pre>{call.args}</pre>
        </div>}</For>
      </div>
    </Show>
  </article>;
}

/// The rendered transcript, or nothing when the body did not parse into
/// one — the caller shows the JSON view in that case.
export function Transcript(props: { turns: Turn[]; tools?: Array<{ name: string; description?: string }> }) {
  return <div class="transcript">
    <Show when={props.tools?.length}>
      <div class="tool-declarations">
        <span class="muted">Tools offered</span>
        <div>
          <For each={props.tools}>{(tool) => <span class="tool-pill mono" title={tool.description}>{tool.name}</span>}</For>
        </div>
      </div>
    </Show>
    <For each={props.turns}>{(turn) => <TurnCard turn={turn} />}</For>
  </div>;
}
