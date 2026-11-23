/**
 * Streaming filter that lifts Kitty graphics APC sequences (`ESC _ G ... ESC \`) out of the PTY
 * byte stream. Everything else passes through byte-exact and in order, even when a sequence is
 * split across chunks. Commands are decoded into `KittyEvent`s (see the gateway contract in
 * docs/phone-mvp.md): PNG transfers (chunked with `m=1..0`), virtual placements (`a=p,U=1`) and
 * deletes (`d=i/I/a/A`).
 */

export type KittyEvent =
  | { type: "image"; id: number; png: Uint8Array }
  | { type: "place"; id: number; placement: number; cols?: number; rows?: number }
  | { type: "delete"; all: true }
  | { type: "delete"; all: false; id: number };

const ESC = 0x1b;
const UNDERSCORE = 0x5f;
const BACKSLASH = 0x5c;
const G = 0x47;

/** A single sequence larger than this is dropped (the gateway sends chunks of at most 4096). */
export const MAX_APC_BODY = 1 << 20;
/** Total base64 text of one multi-chunk transfer. */
export const MAX_TRANSFER = 24 << 20;

export function parseControl(ctrl: string): Map<string, string> {
  const m = new Map<string, string>();
  for (const part of ctrl.split(",")) {
    const eq = part.indexOf("=");
    if (eq > 0) m.set(part.slice(0, eq), part.slice(eq + 1));
  }
  return m;
}

function num(v: string | undefined): number | undefined {
  if (v === undefined || !/^\d+$/.test(v)) return undefined;
  const n = Number(v);
  return Number.isSafeInteger(n) ? n : undefined;
}

export function decodeBase64(text: string): Uint8Array | null {
  try {
    const bin = atob(text.replace(/\s+/g, ""));
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  } catch {
    return null;
  }
}

/** Turns Kitty command bodies (without the leading `G`) into events, assembling chunks. */
export class KittyCommandAssembler {
  private pending: { id: number; parts: string[]; size: number } | null = null;

  reset() {
    this.pending = null;
  }

  handle(body: string): KittyEvent | null {
    const semi = body.indexOf(";");
    const ctrl = semi < 0 ? body : body.slice(0, semi);
    const payload = semi < 0 ? "" : body.slice(semi + 1);
    const c = parseControl(ctrl);
    const action = c.get("a");
    const more = c.get("m") === "1";

    if (this.pending && action === undefined) {
      // Continuation chunk of the transfer in progress.
      const p = this.pending;
      p.size += payload.length;
      if (p.size > MAX_TRANSFER) {
        this.pending = null;
        return null;
      }
      p.parts.push(payload);
      return more ? null : this.finish();
    }
    this.pending = null;

    switch (action ?? "t") {
      case "t":
      case "T": {
        const id = num(c.get("i"));
        const fmt = c.get("f");
        if (!id || (fmt !== undefined && fmt !== "100")) return null;
        this.pending = { id, parts: [payload], size: payload.length };
        return more ? null : this.finish();
      }
      case "p": {
        if (c.get("U") !== "1") return null;
        const id = num(c.get("i"));
        if (!id) return null;
        const ev: KittyEvent = { type: "place", id, placement: num(c.get("p")) ?? 0 };
        const cols = num(c.get("c"));
        const rows = num(c.get("r"));
        if (cols) ev.cols = cols;
        if (rows) ev.rows = rows;
        return ev;
      }
      case "d": {
        const d = c.get("d") ?? "a";
        if (d === "a" || d === "A") return { type: "delete", all: true };
        if (d === "i" || d === "I") {
          const id = num(c.get("i"));
          return id ? { type: "delete", all: false, id } : null;
        }
        return null;
      }
      default:
        return null;
    }
  }

  private finish(): KittyEvent | null {
    const p = this.pending;
    this.pending = null;
    if (!p) return null;
    const png = decodeBase64(p.parts.join(""));
    return png && png.length > 0 ? { type: "image", id: p.id, png } : null;
  }
}

const enum S {
  Normal,
  Esc, // saw ESC
  EscUnderscore, // saw ESC _
  Body, // inside ESC _ G ...
  BodyEsc, // inside body, saw ESC
}

export interface FilterResult {
  out: Uint8Array;
  events: KittyEvent[];
}

export class KittyApcFilter {
  private state: S = S.Normal;
  private body: number[] = [];
  private overflow = false;
  private readonly asm = new KittyCommandAssembler();

  reset() {
    this.state = S.Normal;
    this.body = [];
    this.overflow = false;
    this.asm.reset();
  }

  feed(input: Uint8Array): FilterResult {
    // Up to two held-back bytes from a previous chunk may be flushed in front.
    const out = new Uint8Array(input.length + 2);
    let n = 0;
    const events: KittyEvent[] = [];
    for (let i = 0; i < input.length; i++) {
      const b = input[i];
      switch (this.state) {
        case S.Normal:
          if (b === ESC) this.state = S.Esc;
          else out[n++] = b;
          break;
        case S.Esc:
          if (b === UNDERSCORE) {
            this.state = S.EscUnderscore;
          } else {
            out[n++] = ESC;
            this.state = S.Normal;
            i--; // reprocess b
          }
          break;
        case S.EscUnderscore:
          if (b === G) {
            this.state = S.Body;
            this.body = [];
            this.overflow = false;
          } else {
            out[n++] = ESC;
            out[n++] = UNDERSCORE;
            this.state = S.Normal;
            i--;
          }
          break;
        case S.Body:
          if (b === ESC) this.state = S.BodyEsc;
          else this.push(b);
          break;
        case S.BodyEsc:
          if (b === BACKSLASH) {
            this.state = S.Normal;
            if (!this.overflow) {
              let text = "";
              for (let k = 0; k < this.body.length; k += 8192) {
                text += String.fromCharCode(...this.body.slice(k, k + 8192));
              }
              const ev = this.asm.handle(text);
              if (ev) events.push(ev);
            }
            this.body = [];
          } else if (b === ESC) {
            this.push(ESC);
          } else {
            this.push(ESC);
            this.push(b);
            this.state = S.Body;
          }
          break;
      }
    }
    return { out: out.subarray(0, n), events };
  }

  private push(b: number) {
    if (this.overflow) return;
    if (this.body.length >= MAX_APC_BODY) {
      this.overflow = true;
      this.body = [];
      return;
    }
    this.body.push(b);
  }
}
