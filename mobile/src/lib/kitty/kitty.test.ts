import { describe, expect, it } from "vitest";
import { DIACRITICS } from "./diacritics";
import { KittyApcFilter, KittyCommandAssembler, type KittyEvent } from "./apc";
import {
  PLACEHOLDER,
  coverBox,
  scanCover,
  type CoverRun,
  diacritic,
  diacriticIndex,
  imageCells,
  parseCell,
  resolve,
  scanLine,
  stripBox,
  type CellLike,
  type FgColor,
  type Strip,
} from "./placeholder";
import { ImageStore, applyEvent, pngSize } from "./store";

const enc = (s: string) => Uint8Array.from(s, (c) => c.charCodeAt(0));
const b64 = (bytes: Uint8Array) => btoa(String.fromCharCode(...bytes));

function png(w: number, h: number, extra = 0): Uint8Array {
  const b = new Uint8Array(24 + extra);
  b.set([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0x49, 0x48, 0x44, 0x52]);
  new DataView(b.buffer).setUint32(16, w);
  new DataView(b.buffer).setUint32(20, h);
  return b;
}

const apc = (ctrl: string, payload = "") => `\x1b_G${ctrl}${payload ? ";" + payload : ""}\x1b\\`;

function runFilter(chunks: Uint8Array[]) {
  const f = new KittyApcFilter();
  const out: number[] = [];
  const events: KittyEvent[] = [];
  for (const c of chunks) {
    const r = f.feed(c);
    out.push(...r.out);
    events.push(...r.events);
  }
  return { out: Uint8Array.from(out), events };
}

describe("diacritics", () => {
  it("table is sorted and complete", () => {
    expect(DIACRITICS.length).toBe(297);
    expect(DIACRITICS.every((v, i) => i === 0 || DIACRITICS[i - 1] < v)).toBe(true);
    expect(diacritic(0)).toBe(0x305);
    expect(diacriticIndex(0x30d)).toBe(1);
    expect(diacriticIndex(0x61)).toBeUndefined();
    expect(diacritic(297)).toBeUndefined();
  });
});

describe("KittyApcFilter", () => {
  const png1 = png(4, 2, 10);
  const transfer = apc("a=t,f=100,t=d,q=2,i=7,m=0", b64(png1));
  const text = "hello \x1b[31mred\x1b[0m world\r\n";
  const stream = enc(text + transfer + "after\x1b_Xnot kitty\x1b\\tail");

  it("passes non-APC bytes through and extracts the image", () => {
    const { out, events } = runFilter([stream]);
    expect(new TextDecoder().decode(out)).toBe("hello \x1b[31mred\x1b[0m world\r\nafter\x1b_Xnot kitty\x1b\\tail");
    expect(events).toEqual([{ type: "image", id: 7, png: png1 }]);
  });

  it("gives the same result when split at every byte position", () => {
    const whole = runFilter([stream]);
    for (let i = 0; i <= stream.length; i++) {
      const r = runFilter([stream.subarray(0, i), stream.subarray(i)]);
      expect(r.out).toEqual(whole.out);
      expect(r.events).toEqual(whole.events);
    }
  });

  it("gives the same result when fed one byte at a time", () => {
    const whole = runFilter([stream]);
    const r = runFilter(Array.from(stream, (b) => Uint8Array.of(b)));
    expect(r.out).toEqual(whole.out);
    expect(r.events).toEqual(whole.events);
  });

  it("assembles m=1 chunks into one image", () => {
    const data = b64(png(10, 20, 9000));
    const parts = [data.slice(0, 4096), data.slice(4096, 8192), data.slice(8192)];
    const s =
      "x" +
      apc("a=t,f=100,t=d,q=2,i=3,m=1", parts[0]) +
      "y" +
      apc("m=1", parts[1]) +
      apc("m=0", parts[2]) +
      "z";
    const { out, events } = runFilter([enc(s)]);
    expect(new TextDecoder().decode(out)).toBe("xyz");
    expect(events.length).toBe(1);
    expect(events[0]).toMatchObject({ type: "image", id: 3 });
    expect(pngSize((events[0] as { png: Uint8Array }).png)).toEqual({ width: 10, height: 20 });
  });

  it("decodes placement and delete commands", () => {
    const s = apc("a=p,U=1,q=2,i=5,p=2,c=10,r=4") + apc("a=d,q=2,d=i,i=5") + apc("a=d,q=2,d=A");
    const { events } = runFilter([enc(s)]);
    expect(events).toEqual([
      { type: "place", id: 5, placement: 2, cols: 10, rows: 4 },
      { type: "delete", all: false, id: 5 },
      { type: "delete", all: true },
    ]);
  });

  it("ignores unsupported delete modes, non-virtual placements and non-PNG", () => {
    const s = apc("a=d,d=c") + apc("a=p,i=5") + apc("a=t,f=24,i=1,m=0", "AAAA") + apc("a=d,d=i");
    expect(runFilter([enc(s)]).events).toEqual([]);
  });

  it("drops bad base64 and oversize bodies without losing following bytes", () => {
    const big = apc("a=t,f=100,i=1,m=0", "A".repeat(1 << 21));
    const r = runFilter([enc("a" + big + "b" + apc("a=t,f=100,i=2,m=0", "***") + "c")]);
    expect(new TextDecoder().decode(r.out)).toBe("abc");
    expect(r.events).toEqual([]);
  });

  it("keeps lone ESC and ESC _ non-G sequences intact", () => {
    const s = enc("a\x1b[2Jb\x1b\x1b[Hc\x1b_");
    const r = runFilter([s.subarray(0, 2), s.subarray(2)]);
    expect(r.out).toEqual(s.subarray(0, s.length - 2)); // trailing ESC _ is held back
    const f = new KittyApcFilter();
    f.feed(enc("\x1b"));
    expect(new TextDecoder().decode(f.feed(enc("_Q")).out)).toBe("\x1b_Q");
  });

  it("reset drops a half-received sequence", () => {
    const f = new KittyApcFilter();
    f.feed(enc("\x1b_Ga=t,f=100,i=1,m=1;AAAA"));
    f.reset();
    expect(new TextDecoder().decode(f.feed(enc("ok")).out)).toBe("ok");
  });

  it("a new command interrupts an unfinished transfer", () => {
    const asm = new KittyCommandAssembler();
    expect(asm.handle("a=t,f=100,i=1,m=1;AAAA")).toBeNull();
    expect(asm.handle("a=d,d=a")).toEqual({ type: "delete", all: true });
    expect(asm.handle("m=0;AAAA")).toBeNull(); // continuation of nothing: treated as t without id
  });
});

describe("placeholder cells", () => {
  const cell = (marks: number[], fg: FgColor) =>
    parseCell(String.fromCodePoint(PLACEHOLDER, ...marks.map((m) => DIACRITICS[m])), fg)!;
  const rgb = (v: number): FgColor => ({ kind: "rgb", value: v });

  it("ignores non-placeholders", () => {
    expect(parseCell("a", { kind: "palette", value: 1 })).toBeNull();
    expect(parseCell("", rgb(1))).toBeNull();
  });

  it("decodes RGB and palette ids", () => {
    expect(resolve(cell([0, 0], rgb(0x123456)), null).image).toBe(0x123456);
    expect(resolve(cell([], { kind: "palette", value: 42 }), null).image).toBe(42);
    expect(parseCell(String.fromCodePoint(PLACEHOLDER), { kind: "default" })!.idLow).toBe(0);
  });

  it("third diacritic is the high byte of the id", () => {
    const r = resolve(cell([3, 4, 5], rgb(0x010203)), null);
    expect(r).toEqual({ image: ((5 << 24) | 0x010203) >>> 0, row: 3, col: 4 });
  });

  it("round-trips explicit row/col up to the table limit", () => {
    const r = resolve(cell([5, 296], rgb(0x654321)), null);
    expect([r.row, r.col]).toEqual([5, 296]);
  });

  it("missing marks inherit from the left cell (placeholder.rs test)", () => {
    const first = resolve(cell([1, 0], { kind: "palette", value: 9 }), null);
    expect([first.row, first.col]).toEqual([1, 0]);
    const next = resolve(cell([], { kind: "palette", value: 9 }), first);
    expect([next.row, next.col]).toEqual([1, 1]);
    const rowOnly = resolve(cell([1], { kind: "palette", value: 9 }), next);
    expect([rowOnly.row, rowOnly.col]).toEqual([1, 2]);
    const otherRow = resolve(cell([2], { kind: "palette", value: 9 }), next);
    expect([otherRow.row, otherRow.col]).toEqual([2, 0]);
    const otherImage = resolve(cell([], { kind: "palette", value: 8 }), next);
    expect([otherImage.row, otherImage.col]).toEqual([0, 0]);
  });

  it("inherits the high id byte from the left neighbour", () => {
    const first = resolve(cell([0, 0, 7], rgb(0x000a0b)), null);
    const next = resolve(cell([], rgb(0x000a0b)), first);
    expect(next.image).toBe(first.image);
    expect(next.col).toBe(1);
  });
});

class FakeCell implements CellLike {
  constructor(
    private ch: string,
    private fg: FgColor = { kind: "default" },
    private w = 1,
  ) {}
  getChars() {
    return this.ch;
  }
  getWidth() {
    return this.w;
  }
  isFgRGB() {
    return this.fg.kind === "rgb";
  }
  isFgPalette() {
    return this.fg.kind === "palette";
  }
  getFgColor() {
    return this.fg.kind === "default" ? -1 : this.fg.value;
  }
}

const ph = (row: number | null, col: number | null, id: number) => {
  const marks = [row, col].filter((m) => m !== null).map((m) => String.fromCodePoint(DIACRITICS[m!]));
  return new FakeCell(String.fromCodePoint(PLACEHOLDER) + marks.join(""), { kind: "rgb", value: id });
};

function scan(cells: FakeCell[]): Strip[] {
  const out: Strip[] = [];
  scanLine({ getCell: (x) => cells[x] }, 0, cells.length, out);
  return out;
}

describe("strip grouping", () => {
  it("groups consecutive cells, inheriting columns", () => {
    const strips = scan([new FakeCell("a"), ph(2, 0, 7), ph(null, null, 7), ph(null, null, 7), new FakeCell("b")]);
    expect(strips).toEqual([{ y: 0, x: 1, len: 3, image: 7, row: 2, col: 0 }]);
  });

  it("splits on gaps, other images and column jumps", () => {
    const strips = scan([ph(0, 0, 1), ph(0, 1, 1), new FakeCell(" "), ph(0, 2, 1), ph(0, 5, 1), ph(0, 6, 2)]);
    expect(strips.map((s) => [s.x, s.len, s.image, s.col])).toEqual([
      [0, 2, 1, 0],
      [3, 1, 1, 2],
      [4, 1, 1, 5],
      [5, 1, 2, 6],
    ]);
  });

  it("skips default-colored placeholders (id 0), zero-width cells and missing lines", () => {
    const def = new FakeCell(String.fromCodePoint(PLACEHOLDER));
    expect(scan([def])).toEqual([]);
    expect(scan([new FakeCell(String.fromCodePoint(PLACEHOLDER), { kind: "rgb", value: 3 }, 0)])).toEqual([]);
    const out: Strip[] = [];
    scanLine(undefined, 0, 5, out);
    expect(out).toEqual([]);
  });

  it("reports every resolved cell to the callback", () => {
    const seen: number[] = [];
    scanLine({ getCell: (x) => [ph(3, 4, 9), ph(null, null, 9)][x] }, 0, 2, [], (r) => seen.push(r.row * 100 + r.col));
    expect(seen).toEqual([304, 305]);
  });
});

describe("cover runs", () => {
  const cover = (cells: FakeCell[]) => {
    const out: CoverRun[] = [];
    scanCover({ getCell: (x) => cells[x] }, 3, cells.length, out);
    return out;
  };
  it("covers every placeholder cell, including default-colored and unresolvable ones", () => {
    const def = new FakeCell(String.fromCodePoint(PLACEHOLDER));
    expect(cover([new FakeCell("a"), ph(0, 0, 5), def, ph(0, 2, 9), new FakeCell("b"), def])).toEqual([
      { y: 3, x: 1, len: 3 },
      { y: 3, x: 5, len: 1 },
    ]);
  });
  it("box spans all cells of the run", () => {
    expect(coverBox({ y: 2, x: 4, len: 3 }, 10, 20)).toEqual({ left: 40, top: 40, width: 30, height: 20 });
  });
  it("handles missing lines", () => {
    const out: CoverRun[] = [];
    scanCover(undefined, 0, 4, out);
    expect(out).toEqual([]);
  });
});

describe("stripBox geometry", () => {
  it("stretches nothing when aspect matches", () => {
    const b = stripBox({ x: 2, y: 5, len: 3, row: 1, col: 4 }, 10, 4, 200, 160, 10, 20);
    expect(b).toMatchObject({ left: 20, top: 100, width: 30, height: 20, bgWidth: 100, bgHeight: 80, bgX: -40, bgY: -20 });
  });

  it("letterboxes a wide image vertically", () => {
    // box 100x80, image 200x50 -> 100x25, centered: oy = 27.5
    const b = stripBox({ x: 0, y: 0, len: 10, row: 0, col: 0 }, 10, 4, 200, 50, 10, 20);
    expect([b.bgWidth, b.bgHeight, b.bgX, b.bgY]).toEqual([100, 25, 0, 27.5]);
  });

  it("pillarboxes a tall image horizontally", () => {
    const b = stripBox({ x: 0, y: 0, len: 1, row: 0, col: 2 }, 10, 4, 50, 200, 10, 20);
    expect([b.bgWidth, b.bgHeight, b.bgX]).toEqual([20, 80, 40 - 20]);
  });

  it("imageCells prefers explicit size, falls back to seen extent, never zero", () => {
    expect(imageCells(5, 3, 9, 9)).toEqual({ cols: 5, rows: 3 });
    expect(imageCells(undefined, undefined, 4, 2)).toEqual({ cols: 4, rows: 2 });
    expect(imageCells(undefined, undefined, 0, 0)).toEqual({ cols: 1, rows: 1 });
  });
});

describe("ImageStore", () => {
  function mk(opts: { maxBytes?: number; maxImages?: number } = {}) {
    const live = new Set<string>();
    let n = 0;
    const store = new ImageStore({
      ...opts,
      makeUrl: () => {
        const u = `blob:${n++}`;
        live.add(u);
        return u;
      },
      revokeUrl: (u) => live.delete(u),
    });
    return { store, live };
  }

  it("parses PNG size and rejects junk", () => {
    expect(pngSize(png(33, 44))).toEqual({ width: 33, height: 44 });
    expect(pngSize(new Uint8Array(30))).toBeNull();
    expect(pngSize(png(0, 5))).toBeNull();
    const { store } = mk();
    expect(store.set(1, new Uint8Array(40))).toBe(false);
    expect(store.size).toBe(0);
  });

  it("replaces an image with the same id and revokes the old url", () => {
    const { store, live } = mk();
    store.set(1, png(2, 2));
    store.place(1, 6, 3);
    store.set(1, png(4, 4, 8));
    expect(store.size).toBe(1);
    expect(live.size).toBe(1);
    expect(store.get(1)).toMatchObject({ width: 4, cols: 6, rows: 3 });
    expect(store.bytes).toBe(32);
  });

  it("applies delete semantics", () => {
    const { store, live } = mk();
    applyEvent(store, { type: "image", id: 1, png: png(1, 1) });
    applyEvent(store, { type: "image", id: 2, png: png(1, 1) });
    applyEvent(store, { type: "delete", all: false, id: 1 });
    expect(store.get(1)).toBeUndefined();
    expect(store.size).toBe(1);
    applyEvent(store, { type: "delete", all: true });
    expect(store.size).toBe(0);
    expect(live.size).toBe(0);
    expect(store.bytes).toBe(0);
  });

  it("evicts least recently used by count and by bytes", () => {
    const { store, live } = mk({ maxImages: 2 });
    store.set(1, png(1, 1));
    store.set(2, png(1, 1));
    store.get(1); // 2 is now the oldest
    store.set(3, png(1, 1));
    expect(store.get(2)).toBeUndefined();
    expect(store.get(1)).toBeDefined();
    expect(live.size).toBe(2);

    const b = mk({ maxBytes: 100 });
    b.store.set(1, png(1, 1, 40)); // 64 bytes
    b.store.set(2, png(1, 1, 40));
    expect(b.store.get(1)).toBeUndefined();
    expect(b.store.get(2)).toBeDefined();
    // The newest image is kept even when it alone exceeds the budget.
    b.store.set(3, png(1, 1, 500));
    expect(b.store.size).toBe(1);
  });

  it("tracks extents and notifies listeners", () => {
    const { store } = mk();
    let calls = 0;
    const off = store.onChange(() => calls++);
    store.set(1, png(1, 1));
    expect(store.noteExtent(1, 2, 3)).toBe(true);
    expect(store.noteExtent(1, 1, 1)).toBe(false);
    expect(store.noteExtent(99, 1, 1)).toBe(false);
    expect(store.get(1)).toMatchObject({ seenRows: 3, seenCols: 4 });
    store.clear();
    off();
    store.set(2, png(1, 1));
    expect(calls).toBe(2);
  });
});
