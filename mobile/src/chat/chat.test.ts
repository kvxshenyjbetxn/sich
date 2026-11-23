import { describe, expect, it } from "vitest";
import { FetchQueue } from "./fetchQueue";
import { escapeHtml, renderMarkdown, safeHref } from "./markdown";
import {
  INITIAL_CHAT,
  buildItems,
  earliestOrder,
  expireEchoes,
  isNearBottom,
  lastUserOrder,
  mergeMessages,
  pinchScale,
  pruneEchoes,
  reduceChat,
  resultPreview,
  toggleSet,
  toolIcon,
  toolSummaryLine,
  type Echo,
} from "./messages";
import { chatSupported } from "./chatApi";
import type { ChatBlock, ChatMessage, ChatRole } from "./types";

function msg(id: string, order: number, role: ChatRole = "assistant", blocks: ChatBlock[] = [{ type: "text", markdown: id }]): ChatMessage {
  return { id, order, role, timestamp_ms: null, blocks };
}

describe("mergeMessages", () => {
  it("sorts by order", () => {
    const out = mergeMessages([], [msg("b", 20), msg("a", 10), msg("c", 30)]);
    expect(out.map((m) => m.id)).toEqual(["a", "b", "c"]);
  });

  it("replaces by id (keeps one entry, takes the new content and order)", () => {
    const list = [msg("a", 10), msg("b", 20)];
    const out = mergeMessages(list, [msg("a", 25, "assistant", [{ type: "text", markdown: "new" }])]);
    expect(out.map((m) => m.id)).toEqual(["b", "a"]);
    expect(out[1].blocks[0]).toEqual({ type: "text", markdown: "new" });
  });

  it("dedupes ids inside one batch and against the list", () => {
    const out = mergeMessages([msg("a", 1)], [msg("a", 1), msg("a", 1), msg("b", 2)]);
    expect(out.map((m) => m.id)).toEqual(["a", "b"]);
  });

  it("does not mutate its inputs and returns the same list for an empty batch", () => {
    const list = [msg("a", 1)];
    expect(mergeMessages(list, [])).toBe(list);
    mergeMessages(list, [msg("b", 2)]);
    expect(list).toHaveLength(1);
  });
});

describe("reduceChat", () => {
  it("snapshot replaces everything, update merges, earlier prepends and clears loading", () => {
    let s = reduceChat(INITIAL_CHAT, { kind: "snapshot", status: { state: "ready" }, messages: [msg("c", 30), msg("d", 40)], has_more: true });
    expect(s.status).toEqual({ state: "ready" });
    expect(s.hasMore).toBe(true);
    s = reduceChat(s, { kind: "update", messages: [msg("e", 50)] });
    expect(s.messages.map((m) => m.id)).toEqual(["c", "d", "e"]);
    s = { ...s, loadingEarlier: true };
    s = reduceChat(s, { kind: "earlier", messages: [msg("a", 10), msg("b", 20)], has_more: false });
    expect(s.messages.map((m) => m.id)).toEqual(["a", "b", "c", "d", "e"]);
    expect(s.hasMore).toBe(false);
    expect(s.loadingEarlier).toBe(false);
    expect(earliestOrder(s.messages)).toBe(10);
  });

  it("a new snapshot (e.g. after reconnect) replaces stale messages", () => {
    let s = reduceChat(INITIAL_CHAT, { kind: "snapshot", status: { state: "ready" }, messages: [msg("a", 1)], has_more: false });
    s = reduceChat(s, { kind: "snapshot", status: { state: "pending" }, messages: [], has_more: false });
    expect(s.messages).toEqual([]);
    expect(s.status).toEqual({ state: "pending" });
    expect(earliestOrder(s.messages)).toBeNull();
  });
});

const userMsg = (id: string, order: number, text: string) => msg(id, order, "user", [{ type: "text", markdown: text }]);
const echo = (key: number, text: string, baseOrder: number, sentAt = 0): Echo => ({ key, text, baseOrder, sentAt });

describe("optimistic echo", () => {
  it("is removed once a newer user message with the same text arrives", () => {
    const e = [echo(1, "hello  world", 5)];
    expect(pruneEchoes(e, [userMsg("u1", 10, "hello world")])).toEqual([]);
  });

  it("stays when only older / different / assistant messages exist", () => {
    const e = [echo(1, "hello", 100)];
    expect(pruneEchoes(e, [userMsg("old", 50, "hello")])).toBe(e);
    expect(pruneEchoes(e, [userMsg("u", 200, "other")])).toBe(e);
    expect(pruneEchoes(e, [msg("a", 200, "assistant", [{ type: "text", markdown: "hello" }])])).toBe(e);
  });

  it("one message confirms only one of two identical echoes", () => {
    const e = [echo(1, "ok", 0), echo(2, "ok", 0)];
    const left = pruneEchoes(e, [userMsg("u1", 10, "ok")]);
    expect(left.map((x) => x.key)).toEqual([2]);
    expect(pruneEchoes(e, [userMsg("u1", 10, "ok"), userMsg("u2", 20, "ok")])).toEqual([]);
  });

  it("lastUserOrder ignores non-user messages; expired echoes are dropped", () => {
    expect(lastUserOrder([userMsg("u", 7, "x"), msg("a", 99)])).toBe(7);
    expect(lastUserOrder([])).toBe(-1);
    const e = [echo(1, "x", 0, 1000), echo(2, "y", 0, 100_000)];
    expect(expireEchoes(e, 100_500).map((x) => x.key)).toEqual([2]);
  });

  it("buildItems appends pending echoes after the transcript", () => {
    const items = buildItems([userMsg("u1", 1, "a")], [echo(3, "later", 1)]);
    expect(items.map((i) => i.kind)).toEqual(["user", "user"]);
    expect(items[1]).toMatchObject({ key: "echo:3", text: "later", pending: true });
  });
});

describe("buildItems / tools", () => {
  it("pairs tool calls with results across messages, errors included", () => {
    const items = buildItems([
      msg("a1", 1, "assistant", [
        { type: "text", markdown: "running" },
        { type: "tool_call", call_id: "c1", name: "Bash", summary: "ls -la" },
        { type: "tool_call", call_id: "c2", name: "Read", summary: "/x" },
      ]),
      msg("t1", 2, "tool", [
        { type: "tool_result", call_id: "c1", output: "file", is_error: false },
        { type: "tool_result", call_id: "c2", output: "boom", is_error: true },
      ]),
    ]);
    expect(items.map((i) => i.kind)).toEqual(["assistant", "tool", "tool"]);
    const t = items.filter((i) => i.kind === "tool");
    expect(t[0]).toMatchObject({ name: "Bash", result: { output: "file", isError: false } });
    expect(t[1]).toMatchObject({ name: "Read", result: { output: "boom", isError: true } });
  });

  it("a call without a result is pending; an orphan result becomes its own row", () => {
    const items = buildItems([
      msg("a", 1, "assistant", [{ type: "tool_call", call_id: "x", name: "Bash", summary: "sleep" }]),
      msg("t", 2, "tool", [{ type: "tool_result", call_id: "zzz", output: "late", is_error: false }]),
    ]);
    expect(items).toHaveLength(2);
    expect(items[0]).toMatchObject({ kind: "tool", result: null });
    expect(items[1]).toMatchObject({ kind: "tool", callId: "zzz", result: { output: "late" } });
  });

  it("maps roles to item kinds, user images, and skips empty text", () => {
    const items = buildItems([
      msg("u", 1, "user", [
        { type: "text", markdown: "look" },
        { type: "image", image_ref: "r1", alt: "pic" },
      ]),
      msg("r", 2, "reasoning", [{ type: "text", markdown: "hmm" }]),
      msg("s", 3, "system", [{ type: "text", markdown: "sys" }]),
      msg("e", 4, "assistant", [{ type: "text", markdown: "   " }]),
      msg("i", 5, "assistant", [{ type: "image", image_ref: "r2", alt: "" }]),
    ]);
    expect(items.map((i) => i.kind)).toEqual(["user", "reasoning", "system", "image"]);
    expect(items[0]).toMatchObject({ images: [{ ref: "r1", alt: "pic" }] });
  });

  it("summary is one line and truncated; result preview takes the first non-empty line", () => {
    expect(toolSummaryLine("a\n  b\tc")).toBe("a b c");
    expect(toolSummaryLine("x".repeat(500), 10)).toBe("xxxxxxxxx…");
    expect(resultPreview("\n\n  first line\nsecond")).toBe("first line");
    expect(resultPreview("")).toBe("");
  });

  it("picks icons by tool name", () => {
    expect(toolIcon("Bash")).toBe("terminal");
    expect(toolIcon("Edit")).toBe("edit");
    expect(toolIcon("Read")).toBe("file");
    expect(toolIcon("Grep")).toBe("search");
    expect(toolIcon("WebFetch")).toBe("globe");
    expect(toolIcon("TodoWrite")).toBe("edit"); // "write" wins; still a sensible icon
    expect(toolIcon("mystery")).toBe("wrench");
  });

  it("toggleSet adds and removes without mutating", () => {
    const a = new Set<string>();
    const b = toggleSet(a, "k");
    expect(a.size).toBe(0);
    expect(b.has("k")).toBe(true);
    expect(toggleSet(b, "k").has("k")).toBe(false);
  });
});

describe("scroll / zoom helpers", () => {
  it("isNearBottom", () => {
    expect(isNearBottom(920, 400, 1320)).toBe(true);
    expect(isNearBottom(0, 400, 1320)).toBe(false);
    expect(isNearBottom(0, 400, 400)).toBe(true);
  });
  it("pinchScale clamps and ignores bad distances", () => {
    expect(pinchScale(1, 100, 200)).toBe(2);
    expect(pinchScale(1, 100, 50)).toBe(1);
    expect(pinchScale(2, 100, 10_000)).toBe(6);
    expect(pinchScale(2, 0, 100)).toBe(2);
  });
  it("chatSupported needs Claude/Codex and the chat capability", () => {
    expect(chatSupported("Claude", ["chat"])).toBe(true);
    expect(chatSupported("Codex", ["chat"])).toBe(true);
    expect(chatSupported("Gemini", ["chat"])).toBe(false);
    expect(chatSupported("Claude", [])).toBe(false);
    expect(chatSupported(undefined, ["chat"])).toBe(false);
  });
});

describe("FetchQueue", () => {
  function deferred<T>() {
    let resolve!: (v: T) => void;
    let reject!: (e: unknown) => void;
    const promise = new Promise<T>((res, rej) => {
      resolve = res;
      reject = rej;
    });
    return { promise, resolve, reject };
  }
  const tick = () => new Promise((r) => setTimeout(r, 0));

  it("never runs more than 2 at a time and drains in order", async () => {
    const q = new FetchQueue<string>(2);
    const ds = [deferred<string>(), deferred<string>(), deferred<string>(), deferred<string>()];
    let running = 0;
    let peak = 0;
    const started: number[] = [];
    const ps = ds.map((d, i) =>
      q.load(`k${i}`, () => {
        running++;
        peak = Math.max(peak, running);
        started.push(i);
        return d.promise.finally(() => running--);
      }),
    );
    await tick();
    expect(started).toEqual([0, 1]);
    ds[0].resolve("a");
    await tick();
    expect(started).toEqual([0, 1, 2]);
    ds[1].resolve("b");
    ds[2].resolve("c");
    await tick();
    expect(started).toEqual([0, 1, 2, 3]);
    ds[3].resolve("d");
    expect(await Promise.all(ps)).toEqual(["a", "b", "c", "d"]);
    expect(peak).toBe(2);
    expect(q.active).toBe(0);
  });

  it("dedupes in-flight requests and caches results", async () => {
    const q = new FetchQueue<string>(2);
    let calls = 0;
    const fn = async () => {
      calls++;
      return "x";
    };
    const [a, b] = await Promise.all([q.load("k", fn), q.load("k", fn)]);
    expect([a, b]).toEqual(["x", "x"]);
    await q.load("k", fn);
    expect(calls).toBe(1);
    expect(q.cached("k")).toBe("x");
  });

  it("a failure frees the slot and is not cached (retry possible)", async () => {
    const q = new FetchQueue<string>(1);
    await expect(q.load("k", () => Promise.reject(new Error("no")))).rejects.toThrow("no");
    expect(q.active).toBe(0);
    expect(q.cached("k")).toBeUndefined();
    expect(await q.load("k", async () => "ok")).toBe("ok");
  });

  it("evicts the oldest cache entry past the limit", async () => {
    const q = new FetchQueue<number>(2, 2);
    await q.load("a", async () => 1);
    await q.load("b", async () => 2);
    await q.load("c", async () => 3);
    expect(q.cached("a")).toBeUndefined();
    expect(q.cached("c")).toBe(3);
  });
});

describe("markdown sanitizing", () => {
  it("renders basic markdown: code blocks, lists, tables", () => {
    const html = renderMarkdown("# T\n\n- a\n- b\n\n```ts\nconst x = 1;\n```\n\n| h |\n|---|\n| c |");
    expect(html).toContain("<h1");
    expect(html).toContain("<li>a</li>");
    expect(html).toContain("<pre><code");
    expect(html).toContain("<table>");
  });

  it("escapes raw HTML blocks and inline tags", () => {
    const html = renderMarkdown('<script>alert(1)</script>\n\nhi <img src=x onerror=alert(1)> there <b onclick="x()">b</b>');
    expect(html).not.toMatch(/<script/i);
    expect(html).not.toMatch(/<img/i);
    expect(html).not.toMatch(/<b /i);
    expect(html).toContain("&lt;script&gt;");
    expect(html).toContain("&lt;img");
  });

  it("drops javascript:/data:/vbscript: link targets but keeps the text", () => {
    for (const bad of ["javascript:alert(1)", "JaVaScRiPt:alert(1)", "data:text/html;base64,AAAA", "vbscript:x", "java\tscript:alert(1)"]) {
      const html = renderMarkdown(`[click](${bad.replace(/\t/g, "%09")}) and [x](<${bad}>)`);
      expect(html, bad).not.toMatch(/href=/i);
      expect(html, bad).toContain("click");
    }
  });

  it("keeps http(s) and mailto links with safe rel/target", () => {
    const html = renderMarkdown("[a](https://example.com/p?q=1&r=2) [m](mailto:a@b.c) <https://auto.link>");
    expect(html).toContain('href="https://example.com/p?q=1&amp;r=2"');
    expect(html).toContain('rel="noopener noreferrer"');
    expect(html).toContain('href="mailto:a@b.c"');
    expect(html).toContain('href="https://auto.link"');
  });

  it("never emits <img>: remote images become alt text", () => {
    const html = renderMarkdown("![cat](https://x/y.png)");
    expect(html).not.toMatch(/<img/i);
    expect(html).toContain("cat");
  });

  it("safeHref / escapeHtml units", () => {
    expect(safeHref("https://a.b")).toBe("https://a.b");
    expect(safeHref("  https://a.b ")).toBe("https://a.b");
    expect(safeHref("/relative")).toBeNull();
    expect(safeHref("")).toBeNull();
    expect(safeHref(null)).toBeNull();
    expect(escapeHtml(`<a href="x">&'`)).toBe("&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
  });
});
