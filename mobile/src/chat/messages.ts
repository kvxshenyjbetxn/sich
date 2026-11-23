// Pure chat model logic: list merging, optimistic echo, display items, tool rows, scroll helpers.
import type { ChatBlock, ChatEvent, ChatMessage, ChatStatus } from "./types";

/** Merge `incoming` into `list`: replace by id, drop duplicates, sort by `order`. Never mutates. */
export function mergeMessages(list: ChatMessage[], incoming: ChatMessage[]): ChatMessage[] {
  if (incoming.length === 0) return list;
  const byId = new Map<string, ChatMessage>();
  for (const m of list) byId.set(m.id, m);
  for (const m of incoming) byId.set(m.id, m);
  return Array.from(byId.values()).sort((a, b) => a.order - b.order);
}

export interface ChatState {
  status: ChatStatus | null;
  messages: ChatMessage[];
  hasMore: boolean;
  loadingEarlier: boolean;
}

export const INITIAL_CHAT: ChatState = { status: null, messages: [], hasMore: false, loadingEarlier: false };

export function reduceChat(s: ChatState, ev: ChatEvent): ChatState {
  switch (ev.kind) {
    case "snapshot":
      return { status: ev.status, messages: mergeMessages([], ev.messages), hasMore: ev.has_more, loadingEarlier: false };
    case "update":
      return { ...s, messages: mergeMessages(s.messages, ev.messages) };
    case "earlier":
      return { ...s, messages: mergeMessages(s.messages, ev.messages), hasMore: ev.has_more, loadingEarlier: false };
  }
}

/** Smallest `order` in the list (the cursor for "load earlier"), or null when empty. */
export function earliestOrder(messages: ChatMessage[]): number | null {
  return messages.length === 0 ? null : messages[0].order;
}

// ---- optimistic echo ----

export interface Echo {
  key: number;
  text: string;
  /** Highest user-message order known when the text was sent; only newer messages can match. */
  baseOrder: number;
  sentAt: number;
}

export function normText(s: string): string {
  return s.replace(/\s+/g, " ").trim();
}

export function userText(m: ChatMessage): string {
  return normText(
    m.blocks
      .filter((b): b is Extract<ChatBlock, { type: "text" }> => b.type === "text")
      .map((b) => b.markdown)
      .join(" "),
  );
}

export function lastUserOrder(messages: ChatMessage[]): number {
  let o = -1;
  for (const m of messages) if (m.role === "user" && m.order > o) o = m.order;
  return o;
}

/** Echoes that are not yet confirmed by a newer user message with the same (whitespace-normalised) text. */
export function pruneEchoes(echoes: Echo[], messages: ChatMessage[]): Echo[] {
  if (echoes.length === 0) return echoes;
  const used = new Set<string>();
  const left: Echo[] = [];
  for (const e of echoes) {
    const want = normText(e.text);
    const hit = messages.find(
      (m) => m.role === "user" && m.order > e.baseOrder && !used.has(m.id) && want.length > 0 && userText(m).includes(want),
    );
    if (hit) used.add(hit.id);
    else left.push(e);
  }
  return left.length === echoes.length ? echoes : left;
}

/** Echoes older than `maxAgeMs` are dropped (the message never showed up, e.g. text was empty/only images). */
export function expireEchoes(echoes: Echo[], now: number, maxAgeMs = 60_000): Echo[] {
  const keep = echoes.filter((e) => now - e.sentAt < maxAgeMs);
  return keep.length === echoes.length ? echoes : keep;
}

// ---- display items ----

export interface ToolResultInfo {
  output: string;
  isError: boolean;
}

export type ChatItem =
  | { kind: "user"; key: string; text: string; images: { ref: string; alt: string }[]; pending?: boolean }
  | { kind: "assistant"; key: string; markdown: string }
  | { kind: "reasoning"; key: string; text: string }
  | { kind: "system"; key: string; text: string }
  | { kind: "tool"; key: string; callId: string; name: string; summary: string; result: ToolResultInfo | null }
  | { kind: "image"; key: string; ref: string; alt: string };

/** One-line preview of a tool call summary. */
export function toolSummaryLine(summary: string, max = 160): string {
  const one = summary.replace(/\s+/g, " ").trim();
  return one.length > max ? `${one.slice(0, max - 1)}…` : one;
}

/** Icon id for a tool name (resolved to a lucide icon in the view). */
export type ToolIcon = "terminal" | "file" | "edit" | "search" | "globe" | "list" | "bot" | "wrench";
export function toolIcon(name: string): ToolIcon {
  const n = name.toLowerCase();
  if (/(bash|shell|exec|command|powershell|run)/.test(n)) return "terminal";
  if (/(edit|write|patch|notebook|apply)/.test(n)) return "edit";
  if (/(read|view|cat)/.test(n)) return "file";
  if (/(grep|glob|search|find|ls)/.test(n) && !/web/.test(n)) return "search";
  if (/(web|fetch|http|browser)/.test(n)) return "globe";
  if (/(todo|plan|task)/.test(n)) return "list";
  if (/(agent|subagent)/.test(n)) return "bot";
  return "wrench";
}

/** Toggle membership of `key` in an expanded-set, returning a new set. */
export function toggleSet(set: ReadonlySet<string>, key: string): Set<string> {
  const next = new Set(set);
  if (next.has(key)) next.delete(key);
  else next.add(key);
  return next;
}

/** Result of a tool row is shown collapsed; this is the text of its first non-empty line. */
export function resultPreview(output: string, max = 80): string {
  const first = output.split("\n").find((l) => l.trim().length > 0) ?? "";
  return toolSummaryLine(first, max);
}

/**
 * Flatten messages into display items. Tool calls pair with their results by `call_id` (the
 * result may live in another message); results without a visible call become their own row.
 */
export function buildItems(messages: ChatMessage[], echoes: Echo[] = []): ChatItem[] {
  const results = new Map<string, ToolResultInfo>();
  const calls = new Set<string>();
  for (const m of messages) {
    for (const b of m.blocks) {
      if (b.type === "tool_result") results.set(b.call_id, { output: b.output, isError: b.is_error });
      else if (b.type === "tool_call") calls.add(b.call_id);
    }
  }

  const items: ChatItem[] = [];
  for (const m of messages) {
    if (m.role === "user") {
      const text = m.blocks
        .filter((b): b is Extract<ChatBlock, { type: "text" }> => b.type === "text")
        .map((b) => b.markdown)
        .join("\n\n");
      const images = m.blocks
        .filter((b): b is Extract<ChatBlock, { type: "image" }> => b.type === "image")
        .map((b) => ({ ref: b.image_ref, alt: b.alt }));
      if (text.trim() || images.length) items.push({ kind: "user", key: m.id, text, images });
      continue;
    }
    m.blocks.forEach((b, i) => {
      const key = `${m.id}:${i}`;
      switch (b.type) {
        case "text":
          if (!b.markdown.trim()) break;
          if (m.role === "reasoning") items.push({ kind: "reasoning", key, text: b.markdown });
          else if (m.role === "system") items.push({ kind: "system", key, text: b.markdown });
          else items.push({ kind: "assistant", key, markdown: b.markdown });
          break;
        case "tool_call":
          items.push({ kind: "tool", key, callId: b.call_id, name: b.name, summary: b.summary, result: results.get(b.call_id) ?? null });
          break;
        case "tool_result":
          if (!calls.has(b.call_id)) {
            items.push({ kind: "tool", key, callId: b.call_id, name: "", summary: "", result: { output: b.output, isError: b.is_error } });
          }
          break;
        case "image":
          items.push({ kind: "image", key, ref: b.image_ref, alt: b.alt });
          break;
      }
    });
  }
  for (const e of echoes) items.push({ kind: "user", key: `echo:${e.key}`, text: e.text, images: [], pending: true });
  return items;
}

// ---- scrolling / zoom ----

/** True when the scroll position is within `threshold` px of the bottom. */
export function isNearBottom(scrollTop: number, clientHeight: number, scrollHeight: number, threshold = 80): boolean {
  return scrollHeight - (scrollTop + clientHeight) <= threshold;
}

/** New zoom factor for a pinch (two-finger distance `d1` vs the start distance `d0`). */
export function pinchScale(startScale: number, d0: number, d1: number, min = 1, max = 6): number {
  if (!(d0 > 0) || !(d1 > 0)) return startScale;
  return Math.min(max, Math.max(min, startScale * (d1 / d0)));
}
