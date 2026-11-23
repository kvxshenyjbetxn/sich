import { describe, expect, it } from "vitest";
import { DICT, LANGS, detectLang, interpolate, translate } from "./i18n";
import { applyCtrl, ctrlChar, keySequence, toBytes } from "./keys";
import { chooseOutput, fitWithin, hasAlpha, outputName } from "./image";
import { canRetry, connDot, connLabel, rejectKey } from "./connLabels";
import { agentLine, badgeFor, rollup } from "./agent";
import { allTerminals, buildTree, filterTree, normPath, samePath, sortTerminals, worktreeLabel } from "./workspaceTree";
import { WriteQueue, clampFont, fontForWidth, parseFrame, pinchFont } from "./termLogic";
import type { Agent, ConnState, Project, Terminal, Workspace } from "./types";

// ---- i18n ----
describe("i18n", () => {
  it("every key has a non-empty uk/ru/en string", () => {
    for (const [key, entry] of Object.entries(DICT)) {
      for (const lang of LANGS) {
        expect((entry as Record<string, string>)[lang], `${key}.${lang}`).toBeTruthy();
        expect((entry as Record<string, string>)[lang].trim().length, `${key}.${lang}`).toBeGreaterThan(0);
      }
    }
  });

  it("placeholders match across languages", () => {
    const ph = (s: string) => (s.match(/\{\w+\}/g) ?? []).sort().join(",");
    for (const [key, entry] of Object.entries(DICT)) {
      const e = entry as Record<string, string>;
      expect(ph(e.ru), key).toBe(ph(e.uk));
      expect(ph(e.en), key).toBe(ph(e.uk));
    }
  });

  it("detects system language, Ukrainian by default", () => {
    expect(detectLang("ru-RU")).toBe("ru");
    expect(detectLang("en_US")).toBe("en");
    expect(detectLang("uk-UA")).toBe("uk");
    expect(detectLang("de-DE")).toBe("uk");
    expect(detectLang("")).toBe("uk");
    expect(detectLang(undefined)).toBe("uk");
  });

  it("interpolates and leaves unknown placeholders", () => {
    expect(interpolate("a {x} b {y}", { x: 1 })).toBe("a 1 b {y}");
    expect(translate("en", "conn_reconnecting", { attempt: 3 })).toContain("3");
  });
});

// ---- keys ----
describe("keys", () => {
  it("special key sequences", () => {
    expect(keySequence("esc")).toBe("\x1b");
    expect(keySequence("shift_tab")).toBe("\x1b[Z");
    expect(keySequence("up")).toBe("\x1b[A");
    expect(keySequence("left")).toBe("\x1b[D");
    expect(keySequence("ctrl_c")).toBe("\x03");
    expect(keySequence("enter")).toBe("\r");
  });

  it("ctrlChar maps letters and punctuation", () => {
    expect(ctrlChar("c")).toBe("\x03");
    expect(ctrlChar("C")).toBe("\x03");
    expect(ctrlChar("a")).toBe("\x01");
    expect(ctrlChar("z")).toBe("\x1a");
    expect(ctrlChar("[")).toBe("\x1b");
    expect(ctrlChar(" ")).toBe("\x00");
    expect(ctrlChar("1")).toBeNull();
    expect(ctrlChar("ab")).toBeNull();
    expect(ctrlChar("")).toBeNull();
  });

  it("sticky ctrl applies to the next key and is consumed", () => {
    expect(applyCtrl("d", true)).toEqual({ out: "\x04", consumed: true });
    expect(applyCtrl("d", false)).toEqual({ out: "d", consumed: false });
  });

  it("ctrl + arrow gives xterm modifier sequence", () => {
    expect(applyCtrl("\x1b[A", true)).toEqual({ out: "\x1b[1;5A", consumed: true });
    expect(applyCtrl("\x1b[D", true)).toEqual({ out: "\x1b[1;5D", consumed: true });
  });

  it("ctrl stays armed for unmappable input (paste, other escapes)", () => {
    expect(applyCtrl("hello", true)).toEqual({ out: "hello", consumed: false });
    expect(applyCtrl("\x1b[Z", true)).toEqual({ out: "\x1b[Z", consumed: false });
    expect(applyCtrl("5", true)).toEqual({ out: "5", consumed: false });
  });

  it("toBytes is UTF-8", () => {
    expect(Array.from(toBytes("є"))).toEqual([0xd1, 0x94]);
  });
});

// ---- image ----
describe("image", () => {
  it("does not upscale", () => {
    expect(fitWithin(800, 600)).toEqual({ width: 800, height: 600 });
    expect(fitWithin(2048, 100)).toEqual({ width: 2048, height: 100 });
  });
  it("downscales the longest side to 2048 keeping aspect", () => {
    expect(fitWithin(4096, 2048)).toEqual({ width: 2048, height: 1024 });
    expect(fitWithin(3000, 6000)).toEqual({ width: 1024, height: 2048 });
  });
  it("never produces a zero side", () => {
    expect(fitWithin(100000, 1)).toEqual({ width: 2048, height: 1 });
  });
  it("handles bad input", () => {
    expect(fitWithin(0, 10)).toEqual({ width: 0, height: 0 });
    expect(fitWithin(NaN, 10)).toEqual({ width: 0, height: 0 });
  });
  it("detects alpha", () => {
    expect(hasAlpha(new Uint8ClampedArray([1, 2, 3, 255, 4, 5, 6, 255]))).toBe(false);
    expect(hasAlpha(new Uint8ClampedArray([1, 2, 3, 255, 4, 5, 6, 200]))).toBe(true);
    expect(hasAlpha(new Uint8ClampedArray([]))).toBe(false);
  });
  it("PNG with alpha stays PNG, rest is JPEG", () => {
    expect(chooseOutput("image/png", true)).toBe("image/png");
    expect(chooseOutput("image/png", false)).toBe("image/jpeg");
    expect(chooseOutput("image/webp", true)).toBe("image/jpeg");
    expect(chooseOutput("image/heic", false)).toBe("image/jpeg");
  });
  it("output name follows the type", () => {
    expect(outputName("IMG_1.HEIC", "image/jpeg")).toBe("IMG_1.jpg");
    expect(outputName("shot.png", "image/png")).toBe("shot.png");
    expect(outputName("noext", "image/jpeg")).toBe("noext.jpg");
    expect(outputName("", "image/png")).toBe("image.png");
    expect(outputName("a.b.c.webp", "image/jpeg")).toBe("a.b.c.jpg");
  });
});

// ---- connection labels ----
describe("connLabels", () => {
  const t = (key: Parameters<typeof translate>[1], p?: Record<string, string | number>) => translate("en", key, p);
  const states: ConnState[] = [
    { state: "idle" },
    { state: "connecting" },
    { state: "connected", host_name: "PC" },
    { state: "reconnecting", attempt: 2, retry_in_ms: 1500 },
    { state: "reconnecting", attempt: 1, retry_in_ms: 0 },
    { state: "pair_pending" },
    { state: "rejected", reason: "revoked", detail: null },
  ];

  it("dot colors", () => {
    expect(states.map(connDot)).toEqual(["red", "amber", "green", "amber", "amber", "amber", "red"]);
  });
  it("every state has a non-empty label", () => {
    for (const s of states) expect(connLabel(s, t).length).toBeGreaterThan(0);
  });
  it("reconnect label shows seconds rounded up", () => {
    expect(connLabel({ state: "reconnecting", attempt: 2, retry_in_ms: 1500 }, t)).toBe("Reconnecting in 2s (attempt 2)");
  });
  it("known reject reasons have their own text, unknown fall back", () => {
    for (const r of ["unpaired", "pair_secret_invalid", "pair_denied", "pair_timeout", "revoked", "incompatible", "pair_failed", "bad_address"]) {
      expect(rejectKey(r)).not.toBe("reject_unknown");
    }
    expect(rejectKey("zzz")).toBe("reject_unknown");
  });
  it("retry is offered unless connected or pairing", () => {
    expect(canRetry({ state: "connected", host_name: "x" })).toBe(false);
    expect(canRetry({ state: "pair_pending" })).toBe(false);
    expect(canRetry({ state: "reconnecting", attempt: 1, retry_in_ms: 10 })).toBe(true);
    expect(canRetry({ state: "rejected", reason: "x", detail: null })).toBe(true);
  });
});

// ---- agent badges ----
const agent = (over: Partial<Agent> = {}): Agent => ({
  kind: "Claude",
  state: "Idle",
  unread: false,
  since_unix_ms: 0,
  title: null,
  detail: null,
  model: null,
  prompt: null,
  message: null,
  subagents_working: 0,
  ...over,
});

describe("agent badges", () => {
  it("maps states like the desktop", () => {
    expect(badgeFor(null)).toBeNull();
    expect(badgeFor(agent({ state: "Working" }))).toBe("working");
    expect(badgeFor(agent({ state: "Waiting" }))).toBe("waiting");
    expect(badgeFor(agent({ state: "Done", unread: true }))).toBe("done");
    expect(badgeFor(agent({ state: "Done", unread: false }))).toBe("seen_done");
    expect(badgeFor(agent({ state: "Failed", unread: true }))).toBe("failed");
    expect(badgeFor(agent({ state: "Failed", unread: false }))).toBeNull();
    expect(badgeFor(agent({ state: "Idle" }))).toBeNull();
  });
  it("a working subagent shows through a finished or idle main agent but not over waiting", () => {
    expect(badgeFor(agent({ state: "Done", unread: true, subagents_working: 1 }))).toBe("working");
    expect(badgeFor(agent({ state: "Idle", subagents_working: 2 }))).toBe("working");
    expect(badgeFor(agent({ state: "Waiting", subagents_working: 1 }))).toBe("waiting");
  });
  it("rollup priority: waiting > working > failed > done > seen_done", () => {
    const w = agent({ state: "Waiting" });
    const k = agent({ state: "Working" });
    const f = agent({ state: "Failed", unread: true });
    const d = agent({ state: "Done", unread: true });
    expect(rollup([d, f, k, w])).toBe("waiting");
    expect(rollup([d, f, k])).toBe("working");
    expect(rollup([d, f])).toBe("failed");
    expect(rollup([d])).toBe("done");
    expect(rollup([null, agent()])).toBeNull();
    expect(rollup([])).toBeNull();
  });
  it("agentLine prefers detail, then message, then title, collapsing whitespace", () => {
    expect(agentLine(agent({ detail: "Bash", message: "m", title: "t" }))).toBe("Bash");
    expect(agentLine(agent({ detail: "  ", message: "a\n b", title: "t" }))).toBe("a b");
    expect(agentLine(agent({ title: "t" }))).toBe("t");
    expect(agentLine(null)).toBe("");
  });
});

// ---- workspace tree ----
const term = (id: number, over: Partial<Terminal> = {}): Terminal => ({
  term: { daemon: "d", id },
  title: `t${id}`,
  cwd: "C:\\x",
  worktree: null,
  cols: 80,
  rows: 24,
  exited: false,
  exit_code: null,
  created_unix_ms: id,
  agent: null,
  ...over,
});

const proj = (id: number, over: Partial<Project> = {}): Project => ({
  id,
  name: `proj${id}`,
  path: `C:\\dev\\proj${id}`,
  is_git: true,
  group: null,
  worktrees: [{ path: `C:\\dev\\proj${id}`, branch: "main", is_main: true, dirty: false }],
  error: null,
  ...over,
});

describe("workspace tree", () => {
  it("normalizes Windows and Unix paths", () => {
    expect(normPath("C:\\Dev\\App\\")).toBe("c:/dev/app");
    expect(normPath("\\\\?\\C:\\Dev")).toBe("c:/dev");
    expect(normPath("/home/Me/app/")).toBe("/home/Me/app");
    expect(samePath("C:\\dev\\app", "c:/dev/app/")).toBe(true);
    expect(samePath("/a/B", "/a/b")).toBe(false);
    expect(samePath(null, "/a")).toBe(false);
  });

  it("orders terminals by (created, id)", () => {
    const sorted = sortTerminals([term(3, { created_unix_ms: 5 }), term(2, { created_unix_ms: 5 }), term(9, { created_unix_ms: 1 })]);
    expect(sorted.map((t) => t.term.id)).toEqual([9, 2, 3]);
  });

  it("puts terminals without a worktree (or with an unknown one) in Home", () => {
    const ws: Workspace = {
      host_name: "PC",
      groups: [],
      projects: [proj(1)],
      terminals: [term(1), term(2, { worktree: "C:\\dev\\proj1" }), term(3, { worktree: "C:\\gone" })],
    };
    const tree = buildTree(ws);
    expect(tree.home.map((t) => t.term.id)).toEqual([1, 3]);
    expect(tree.groups).toHaveLength(1);
    expect(tree.groups[0].group).toBeNull();
    expect(tree.groups[0].projects[0].worktrees[0].terminals.map((t) => t.term.id)).toEqual([2]);
  });

  it("matches worktree paths across slash styles", () => {
    const ws: Workspace = {
      host_name: "PC",
      groups: [],
      projects: [proj(1)],
      terminals: [term(1, { worktree: "c:/dev/proj1/" })],
    };
    expect(buildTree(ws).groups[0].projects[0].worktrees[0].terminals).toHaveLength(1);
  });

  it("groups projects, keeping loose ones last; unknown group ids count as loose", () => {
    const ws: Workspace = {
      host_name: "PC",
      groups: [{ id: 10, name: "Work" }, { id: 11, name: "Empty" }],
      projects: [proj(1, { group: 10 }), proj(2), proj(3, { group: 99 })],
      terminals: [],
    };
    const tree = buildTree(ws);
    expect(tree.groups.map((g) => g.group?.name ?? null)).toEqual(["Work", "Empty", null]);
    expect(tree.groups[0].projects.map((p) => p.project.id)).toEqual([1]);
    expect(tree.groups[2].projects.map((p) => p.project.id)).toEqual([2, 3]);
  });

  it("a terminal is shown once even if worktrees overlap", () => {
    const p = proj(1, {
      worktrees: [
        { path: "C:\\a", branch: "a", is_main: true, dirty: false },
        { path: "c:/a", branch: "dup", is_main: false, dirty: false },
      ],
    });
    const tree = buildTree({ host_name: "", groups: [], projects: [p], terminals: [term(1, { worktree: "C:\\a" })] });
    expect(allTerminals(tree)).toHaveLength(1);
  });

  it("empty workspace", () => {
    const tree = buildTree({ host_name: "", groups: [], projects: [], terminals: [] });
    expect(tree).toEqual({ home: [], groups: [] });
  });

  const ws: Workspace = {
    host_name: "PC",
    groups: [],
    projects: [
      proj(1, { name: "alpha", worktrees: [{ path: "C:\\a", branch: "main", is_main: true, dirty: false }, { path: "C:\\a-x", branch: "feature-x", is_main: false, dirty: true }] }),
      proj(2, { name: "beta", worktrees: [{ path: "C:\\b", branch: "main", is_main: true, dirty: false }] }),
    ],
    terminals: [
      term(1, { title: "fix bug", worktree: "C:\\a" }),
      term(2, { title: "docs", worktree: "C:\\a-x" }),
      term(3, { title: "docs home" }),
      term(4, { title: "other", worktree: "C:\\b" }),
    ],
  };

  it("filter: empty query keeps everything", () => {
    const tree = buildTree(ws);
    expect(filterTree(tree, "  ")).toBe(tree);
  });

  it("filter by terminal title keeps the path to it", () => {
    const f = filterTree(buildTree(ws), "docs");
    expect(f.home.map((t) => t.term.id)).toEqual([3]);
    expect(f.groups[0].projects).toHaveLength(1);
    expect(f.groups[0].projects[0].worktrees).toHaveLength(1);
    expect(f.groups[0].projects[0].worktrees[0].terminals.map((t) => t.term.id)).toEqual([2]);
  });

  it("filter by project name keeps all its terminals", () => {
    const f = filterTree(buildTree(ws), "ALPHA");
    expect(allTerminals(f).filter((t) => t.worktree).map((t) => t.term.id)).toEqual([1, 2]);
    expect(f.groups[0].projects.map((p) => p.project.name)).toEqual(["alpha"]);
  });

  it("filter by branch keeps that worktree only", () => {
    const f = filterTree(buildTree(ws), "feature");
    expect(f.groups[0].projects[0].worktrees.map((w) => w.worktree.branch)).toEqual(["feature-x"]);
  });

  it("filter with no match is empty", () => {
    const f = filterTree(buildTree(ws), "zzzz");
    expect(f.home).toEqual([]);
    expect(f.groups).toEqual([]);
  });

  it("worktree label: branch, else folder name", () => {
    expect(worktreeLabel({ path: "C:\\dev\\app-x\\", branch: null, is_main: false, dirty: false })).toBe("app-x");
    expect(worktreeLabel({ path: "/a/b", branch: "feat", is_main: false, dirty: false })).toBe("feat");
  });
});

// ---- terminal logic ----
describe("termLogic", () => {
  it("parses reset and output frames", () => {
    expect(parseFrame(new Uint8Array([0, 0x01, 0x2c, 0x00, 0x32]))).toEqual({ kind: "reset", cols: 300, rows: 50 });
    const out = parseFrame(new Uint8Array([1, 104, 105]).buffer);
    expect(out && out.kind === "output" && Array.from(out.bytes)).toEqual([104, 105]);
    const empty = parseFrame([1]);
    expect(empty && empty.kind === "output" && empty.bytes.length).toBe(0);
  });
  it("rejects malformed frames", () => {
    expect(parseFrame(new Uint8Array([]))).toBeNull();
    expect(parseFrame(new Uint8Array([0, 1, 2]))).toBeNull();
    expect(parseFrame(new Uint8Array([7, 1, 2]))).toBeNull();
  });

  it("WriteQueue buffers until attached, then flushes in order", () => {
    const q = new WriteQueue<number>();
    q.push(1);
    q.push(2);
    expect(q.pending).toBe(2);
    const got: number[] = [];
    q.attach((x) => got.push(x));
    q.push(3);
    expect(got).toEqual([1, 2, 3]);
    expect(q.pending).toBe(0);
    q.detach();
    q.push(4);
    expect(q.pending).toBe(1);
    q.attach((x) => got.push(x));
    expect(got).toEqual([1, 2, 3, 4]);
  });
  it("WriteQueue drops the oldest beyond its cap", () => {
    const q = new WriteQueue<number>(3);
    [1, 2, 3, 4, 5].forEach((x) => q.push(x));
    const got: number[] = [];
    q.attach((x) => got.push(x));
    expect(got).toEqual([3, 4, 5]);
  });

  it("fontForWidth scales down to fit but not below min or above base", () => {
    // 100 cols * 0.6 * 13 = 780 px needed
    expect(fontForWidth(780, 100, 0.6, 13)).toBe(13);
    expect(fontForWidth(2000, 100, 0.6, 13)).toBe(13);
    expect(fontForWidth(390, 100, 0.6, 13)).toBe(7); // would be 6.5, clamped to min
    expect(fontForWidth(585, 100, 0.6, 13)).toBe(9.7);
    expect(fontForWidth(100, 200, 0.6, 13)).toBe(7);
    expect(fontForWidth(0, 100, 0.6, 13)).toBe(13);
    expect(fontForWidth(300, 0, 0.6, 13)).toBe(13);
  });
  it("clampFont and pinch", () => {
    expect(clampFont(3)).toBe(7);
    expect(clampFont(99)).toBe(24);
    expect(clampFont(NaN)).toBe(13);
    expect(clampFont(12.26)).toBe(12.5);
    expect(pinchFont(13, 100, 200)).toBe(24);
    expect(pinchFont(13, 100, 50)).toBe(7);
    expect(pinchFont(13, 100, 100)).toBe(13);
    expect(pinchFont(13, 0, 100)).toBe(13);
  });
});
