import { describe, expect, it } from "vitest";
import { KittyApcFilter, type KittyEvent } from "./apc";
import { CLAUDE_COLS, CLAUDE_IMAGE_ID, CLAUDE_ROWS, GATEWAY_APCS, PLACEHOLDER_ROWS } from "./fixtures/claude-inline";
import { PLACEHOLDER, imageCells, scanLine, type CellLike, type FgColor, type Strip } from "./placeholder";
import { ImageStore, applyEvent } from "./store";

const enc = (s: string) => new TextEncoder().encode(s);
const dec = (b: Uint8Array) => new TextDecoder().decode(b);

/** Minimal terminal: SGR 38;2 / 38;5 / 39 foreground tracking and one cell per placeholder. */
class FakeCell implements CellLike {
  constructor(
    private chars: string,
    private fg: FgColor,
  ) {}
  getChars() {
    return this.chars;
  }
  getWidth() {
    return 1;
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

function cellsOf(line: string): FakeCell[] {
  const cells: FakeCell[] = [];
  let fg: FgColor = { kind: "default" };
  const re = /\x1b\[([0-9;]*)([A-Za-z])|(\u{10EEEE}[̀-ͯ]*)/gu;
  for (const m of line.matchAll(re)) {
    if (m[3]) {
      cells.push(new FakeCell(m[3], fg));
    } else if (m[2] === "m") {
      const p = m[1].split(";").map(Number);
      if (p[0] === 38 && p[1] === 2) fg = { kind: "rgb", value: (p[2] << 16) | (p[3] << 8) | p[4] };
      else if (p[0] === 38 && p[1] === 5) fg = { kind: "palette", value: p[2] };
      else if (p[0] === 39 || p[0] === 0) fg = { kind: "default" };
    }
  }
  return cells;
}

function tinyStore() {
  return new ImageStore({ makeUrl: () => "blob:test", revokeUrl: () => {} });
}

describe("real Claude Code inline image through the phone pipeline", () => {
  const stream = GATEWAY_APCS.join("") + PLACEHOLDER_ROWS.join("\r\n") + "\r\n";

  function run(size: number) {
    const f = new KittyApcFilter();
    const store = tinyStore();
    const out: number[] = [];
    const events: KittyEvent[] = [];
    const bytes = enc(stream);
    for (let i = 0; i < bytes.length; i += size) {
      const r = f.feed(bytes.subarray(i, i + size));
      out.push(...r.out);
      events.push(...r.events);
    }
    events.forEach((e) => applyEvent(store, e));
    return { store, text: dec(Uint8Array.from(out)), events };
  }

  it.each([1, 3, 64, 100000])("lifts the image and keeps every placeholder row (chunks of %i bytes)", (size) => {
    const { store, text, events } = run(size);
    expect(events.map((e) => e.type)).toEqual(["image", "place"]);
    expect(text).toBe(PLACEHOLDER_ROWS.join("\r\n") + "\r\n");
    const img = store.get(CLAUDE_IMAGE_ID)!;
    expect(img).toBeTruthy();
    expect([img.width, img.height]).toEqual([1, 1]);
    expect([img.cols, img.rows]).toEqual([CLAUDE_COLS, CLAUDE_ROWS]);
  });

  it("resolves every row of the picture to the stored image", () => {
    const { store, text } = run(100000);
    const lines = text.split("\r\n").filter(Boolean);
    expect(lines).toHaveLength(CLAUDE_ROWS);
    const strips: Strip[] = [];
    lines.forEach((line: string, y: number) => {
      const cells = cellsOf(line);
      expect(cells).toHaveLength(CLAUDE_COLS);
      scanLine({ getCell: (x) => cells[x] }, y, cells.length, strips, (r) => store.noteExtent(r.image, r.row, r.col));
    });
    expect(strips).toHaveLength(CLAUDE_ROWS);
    strips.forEach((s, y) => {
      expect(s).toEqual({ y, x: 0, len: CLAUDE_COLS, image: CLAUDE_IMAGE_ID, row: y, col: 0 });
      expect(store.get(s.image)).toBeTruthy();
    });
    const img = store.get(CLAUDE_IMAGE_ID)!;
    expect(imageCells(img.cols, img.rows, img.seenCols, img.seenRows)).toEqual({ cols: CLAUDE_COLS, rows: CLAUDE_ROWS });
    expect([img.seenCols, img.seenRows]).toEqual([CLAUDE_COLS, CLAUDE_ROWS]);
  });

  it("the same picture addressed with a 256-color foreground resolves too", () => {
    const row = `\x1b[38;5;42m${String.fromCodePoint(PLACEHOLDER)}̅̅${String.fromCodePoint(PLACEHOLDER)}̅̍\x1b[39m`;
    const cells = cellsOf(row);
    const strips: Strip[] = [];
    scanLine({ getCell: (x) => cells[x] }, 0, cells.length, strips);
    expect(strips).toEqual([{ y: 0, x: 0, len: 2, image: 42, row: 0, col: 0 }]);
  });

  it("gateway 0.1.41 repeated the keys on every chunk: still one image", () => {
    const legacy = [
      GATEWAY_APCS[0],
      GATEWAY_APCS[1].replace("_Gm=0,q=2;", `_Ga=t,f=100,t=d,q=2,i=${CLAUDE_IMAGE_ID},m=0;`),
      GATEWAY_APCS[2],
    ];
    const f = new KittyApcFilter();
    const r = f.feed(enc(legacy.join("")));
    expect(r.events.map((e) => e.type)).toEqual(["image", "place"]);
    const store = tinyStore();
    r.events.forEach((e) => applyEvent(store, e));
    expect(store.get(CLAUDE_IMAGE_ID)).toBeTruthy();
    // A different id while a transfer is open starts a new image instead.
    const f2 = new KittyApcFilter();
    const other = f2.feed(enc(GATEWAY_APCS[0] + GATEWAY_APCS[0].replace("i=15537056", "i=9").replace("m=1", "m=0")));
    expect(other.events.map((e) => (e.type === "image" ? e.id : -1))).toEqual([9]);
  });

  it("a replay that starts in the middle of an image transfer is ignored without breaking the stream", () => {
    const f = new KittyApcFilter();
    // Continuation chunk (no `a=`) of a transfer whose start was never seen, then a full image.
    const orphan = "\x1b_Gm=1,q=2;QUJDRA==\x1b\\\x1b_Gm=0,q=2;QUJDRA==\x1b\\";
    const r = f.feed(enc(orphan + GATEWAY_APCS.join("") + "ok"));
    expect(r.events.map((e) => e.type)).toEqual(["image", "place"]);
    expect(dec(r.out)).toBe("ok");
  });
});
