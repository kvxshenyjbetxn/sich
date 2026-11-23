/** Pure helpers behind the terminal screen. */

export type TermFrame = { kind: "reset"; cols: number; rows: number } | { kind: "output"; bytes: Uint8Array };

/**
 * Binary channel frames from the Rust core (see src-tauri/src/ui.rs):
 * `[0, cols_be16, rows_be16]` = Reset, `[1, ...bytes]` = Output.
 */
export function parseFrame(buf: ArrayBuffer | Uint8Array | number[]): TermFrame | null {
  const b = buf instanceof Uint8Array ? buf : Array.isArray(buf) ? Uint8Array.from(buf) : new Uint8Array(buf);
  if (b.length === 0) return null;
  if (b[0] === 0) {
    if (b.length < 5) return null;
    return { kind: "reset", cols: (b[1] << 8) | b[2], rows: (b[3] << 8) | b[4] };
  }
  if (b[0] === 1) return { kind: "output", bytes: b.subarray(1) };
  return null;
}

/**
 * Holds items until a consumer is attached, then flushes in order. Used so terminal output that
 * arrives before xterm is initialized is never lost.
 */
export class WriteQueue<T> {
  private queue: T[] = [];
  private sink: ((item: T) => void) | null = null;

  constructor(private readonly maxItems = 20000) {}

  push(item: T) {
    if (this.sink) {
      this.sink(item);
      return;
    }
    this.queue.push(item);
    if (this.queue.length > this.maxItems) this.queue.splice(0, this.queue.length - this.maxItems);
  }

  attach(sink: (item: T) => void) {
    this.sink = sink;
    const pending = this.queue;
    this.queue = [];
    for (const it of pending) sink(it);
  }

  detach() {
    this.sink = null;
  }

  get pending(): number {
    return this.queue.length;
  }
}

export const MIN_FONT = 7;
export const MAX_FONT = 24;
export const DEFAULT_FONT = 13;

export function clampFont(px: number): number {
  if (!Number.isFinite(px)) return DEFAULT_FONT;
  return Math.min(MAX_FONT, Math.max(MIN_FONT, Math.round(px * 2) / 2));
}

/**
 * Font size so that `cols` columns fit in `widthPx`, never larger than `base`, never below `min`.
 * `charRatio` = glyph advance / font size of the monospace font (~0.6).
 */
export function fontForWidth(widthPx: number, cols: number, charRatio: number, base: number, min = MIN_FONT): number {
  if (!(widthPx > 0) || !(cols > 0) || !(charRatio > 0)) return base;
  const fit = Math.floor((widthPx / (cols * charRatio)) * 10) / 10;
  return Math.max(min, Math.min(base, fit));
}

const FONT_KEY = "sich.fontSize";
const FIT_KEY = "sich.fit";

export function loadFontSize(): number {
  try {
    const v = localStorage.getItem(FONT_KEY);
    if (v !== null) return clampFont(parseFloat(v));
  } catch {
    /* storage unavailable */
  }
  return DEFAULT_FONT;
}

export function saveFontSize(px: number) {
  try {
    localStorage.setItem(FONT_KEY, String(clampFont(px)));
  } catch {
    /* ignore */
  }
}

export function loadFit(): boolean {
  try {
    return localStorage.getItem(FIT_KEY) !== "0";
  } catch {
    return true;
  }
}

export function saveFit(fit: boolean) {
  try {
    localStorage.setItem(FIT_KEY, fit ? "1" : "0");
  } catch {
    /* ignore */
  }
}

/** Pinch zoom: scale the starting size by the distance ratio. */
export function pinchFont(startFont: number, startDist: number, dist: number): number {
  if (!(startDist > 0) || !(dist > 0)) return clampFont(startFont);
  return clampFont(startFont * (dist / startDist));
}
