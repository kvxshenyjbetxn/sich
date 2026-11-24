import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
// @ts-expect-error @types/node is not installed; vitest runs in node
import { readFileSync } from "node:fs";
import { DICT, LANGS } from "./i18n";
import { THEME_KINDS, THEME_STORAGE, getTheme, loadTheme, saveTheme, setTheme, subscribeTheme, terminalTheme } from "./theme";

function stubStorage(initial: Record<string, string> = {}) {
  const m = new Map(Object.entries(initial));
  vi.stubGlobal("localStorage", {
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
  });
  return m;
}

beforeEach(() => {
  stubStorage();
});
afterEach(() => {
  vi.unstubAllGlobals();
});

describe("theme storage", () => {
  it("defaults to gray when empty", () => {
    expect(loadTheme()).toBe("gray");
  });
  it("garbage value falls back to gray", () => {
    stubStorage({ [THEME_STORAGE]: "purple" });
    expect(loadTheme()).toBe("gray");
  });
  it("round-trips both kinds", () => {
    for (const k of THEME_KINDS) {
      saveTheme(k);
      expect(loadTheme()).toBe(k);
    }
  });
  it("throwing storage does not crash", () => {
    vi.stubGlobal("localStorage", {
      getItem: () => {
        throw new Error("denied");
      },
      setItem: () => {
        throw new Error("denied");
      },
    });
    expect(loadTheme()).toBe("gray");
    expect(() => saveTheme("amoled")).not.toThrow();
  });
});

describe("terminalTheme", () => {
  it("gray matches the previous hardcoded theme", () => {
    const t = terminalTheme("gray");
    expect(t.background).toBe("#1e1f22");
    expect(t.foreground).toBe("#dfe1e5");
    expect(t.cursor).toBe("#dfe1e5");
    expect(t.selectionBackground).toBe("#2e436e");
    expect(t.black).toBe("#15161e");
    expect(t.red).toBe("#f7768e");
    expect(t.brightWhite).toBe("#c0caf5");
  });
  it("amoled colors", () => {
    const t = terminalTheme("amoled");
    expect(t.background).toBe("#000000");
    expect(t.foreground).toBe("#e6e6e6");
    expect(t.cursor).toBe("#e6e6e6");
    expect(t.selectionBackground).toBe("#1f3a6e");
  });
  it("ANSI table is identical between kinds", () => {
    const strip = (t: Record<string, unknown>) => {
      const { background, foreground, cursor, selectionBackground, ...rest } = t;
      void background, foreground, cursor, selectionBackground;
      return rest;
    };
    expect(strip(terminalTheme("amoled") as Record<string, unknown>)).toEqual(strip(terminalTheme("gray") as Record<string, unknown>));
  });
});

describe("live theme state", () => {
  it("setTheme persists, updates and notifies", () => {
    const fn = vi.fn();
    const off = subscribeTheme(fn);
    setTheme("amoled");
    expect(getTheme()).toBe("amoled");
    expect(loadTheme()).toBe("amoled");
    expect(fn).toHaveBeenCalledTimes(1);
    off();
    setTheme("gray");
    expect(fn).toHaveBeenCalledTimes(1);
    expect(getTheme()).toBe("gray");
  });
});

describe("theme i18n", () => {
  it("new keys have non-empty uk/ru/en", () => {
    for (const key of ["theme", "theme_gray", "theme_amoled"] as const) {
      for (const l of LANGS) expect(DICT[key][l].length).toBeGreaterThan(0);
    }
  });
});

describe("styles.css", () => {
  const css: string = readFileSync(new URL("../styles.css", import.meta.url), "utf8");
  const varsOf = (block: string) => new Set([...block.matchAll(/(--[\w-]+)\s*:/g)].map((m) => m[1]));
  it("amoled block defines every var of :root", () => {
    const root = css.match(/:root\s*\{([^}]*)\}/)![1];
    const amoled = css.match(/:root\[data-theme="amoled"\]\s*\{([^}]*)\}/)![1];
    const missing = [...varsOf(root)].filter((v) => !varsOf(amoled).has(v));
    expect(missing).toEqual([]);
  });
});
