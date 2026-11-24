/** App theme (Gray / AMOLED). Values mirror crates/app/src/colors.rs on the desktop. */
import type { ITheme } from "@xterm/xterm";

export type ThemeKind = "gray" | "amoled";
export const THEME_KINDS: ThemeKind[] = ["gray", "amoled"];
export const DEFAULT_THEME: ThemeKind = "gray";
export const THEME_STORAGE = "sich.theme";

/** Background color per theme (also used for the status-bar meta tag). */
export const THEME_BG: Record<ThemeKind, string> = { gray: "#1e1f22", amoled: "#000000" };

export function isThemeKind(v: unknown): v is ThemeKind {
  return v === "gray" || v === "amoled";
}

export function loadTheme(): ThemeKind {
  try {
    const v = localStorage.getItem(THEME_STORAGE);
    if (isThemeKind(v)) return v;
  } catch {
    /* storage unavailable */
  }
  return DEFAULT_THEME;
}

export function saveTheme(k: ThemeKind) {
  try {
    localStorage.setItem(THEME_STORAGE, k);
  } catch {
    /* ignore */
  }
}

const ANSI = {
  black: "#15161e",
  red: "#f7768e",
  green: "#9ece6a",
  yellow: "#e0af68",
  blue: "#7aa2f7",
  magenta: "#bb9af7",
  cyan: "#7dcfff",
  white: "#a9b1d6",
  brightBlack: "#414868",
  brightRed: "#ff899d",
  brightGreen: "#b9f27c",
  brightYellow: "#ffc777",
  brightBlue: "#9abdf5",
  brightMagenta: "#cfb2ff",
  brightCyan: "#a4daff",
  brightWhite: "#c0caf5",
};

export function terminalTheme(k: ThemeKind): ITheme {
  const amoled = k === "amoled";
  const fg = amoled ? "#e6e6e6" : "#dfe1e5";
  return {
    background: THEME_BG[k],
    foreground: fg,
    cursor: fg,
    selectionBackground: amoled ? "#1f3a6e" : "#2e436e",
    ...ANSI,
  };
}

/** Sets `data-theme` on <html> and keeps `<meta name="theme-color">` in sync. */
export function applyThemeToDocument(k: ThemeKind) {
  if (typeof document === "undefined") return;
  document.documentElement.dataset.theme = k;
  let meta = document.querySelector<HTMLMetaElement>('meta[name="theme-color"]');
  if (!meta) {
    meta = document.createElement("meta");
    meta.name = "theme-color";
    document.head.appendChild(meta);
  }
  meta.content = THEME_BG[k];
}

// ---- live state ----
let current: ThemeKind = DEFAULT_THEME;
const listeners = new Set<() => void>();

export function getTheme(): ThemeKind {
  return current;
}

export function subscribeTheme(fn: () => void): () => void {
  listeners.add(fn);
  return () => {
    listeners.delete(fn);
  };
}

/** Change the active theme: persist, apply to the document and notify subscribers. */
export function setTheme(k: ThemeKind) {
  if (!isThemeKind(k)) return;
  current = k;
  saveTheme(k);
  applyThemeToDocument(k);
  listeners.forEach((l) => l());
}

/** Load the saved theme and apply it; call once before the first render. */
export function initTheme(): ThemeKind {
  current = loadTheme();
  applyThemeToDocument(current);
  return current;
}
