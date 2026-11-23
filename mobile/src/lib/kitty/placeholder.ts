/**
 * Kitty unicode placeholders (port of crates/term/src/placeholder.rs): U+10EEEE cells whose
 * foreground color carries the image id and combining diacritics the row/column in the image.
 * Pure logic only; the DOM glue lives in overlay.ts.
 */
import { DIACRITICS } from "./diacritics";

export const PLACEHOLDER = 0x10eeee;

const INDEX = new Map<number, number>(DIACRITICS.map((c, i) => [c, i]));

export function diacriticIndex(cp: number): number | undefined {
  return INDEX.get(cp);
}

export function diacritic(index: number): number | undefined {
  return DIACRITICS[index];
}

export type FgColor = { kind: "rgb"; value: number } | { kind: "palette"; value: number } | { kind: "default" };

export function colorId(c: FgColor): number | undefined {
  if (c.kind === "rgb") return c.value & 0xffffff;
  if (c.kind === "palette") return c.value & 0xff;
  return undefined;
}

export interface RawCell {
  idLow: number;
  row?: number;
  col?: number;
  idHigh?: number;
}

export interface Resolved {
  image: number;
  row: number;
  col: number;
}

/** Decodes the text of one cell (placeholder + marks); null when it is not a placeholder. */
export function parseCell(chars: string, fg: FgColor): RawCell | null {
  const first = chars.codePointAt(0);
  if (first !== PLACEHOLDER) return null;
  const marks: number[] = [];
  for (const ch of chars.slice(2)) {
    // slice(2): U+10EEEE is a surrogate pair
    const idx = diacriticIndex(ch.codePointAt(0)!);
    if (idx === undefined) break;
    marks.push(idx);
  }
  return { idLow: colorId(fg) ?? 0, row: marks[0], col: marks[1], idHigh: marks[2] };
}

/** Applies the protocol's inheritance rules using the cell to the left (same image). */
export function resolve(raw: RawCell, prev: Resolved | null): Resolved {
  const high = raw.idHigh ?? (prev && (prev.image & 0xffffff) === raw.idLow ? prev.image >>> 24 : 0);
  const image = ((high << 24) | raw.idLow) >>> 0;
  const same = prev && prev.image === image ? prev : null;
  let row: number;
  let col: number;
  if (raw.row !== undefined && raw.col !== undefined) {
    row = raw.row;
    col = raw.col;
  } else if (raw.row !== undefined) {
    row = raw.row;
    col = same && same.row === raw.row ? same.col + 1 : 0;
  } else if (same) {
    row = same.row;
    col = same.col + 1;
  } else {
    row = 0;
    col = 0;
  }
  return { image, row, col };
}

/** A run of consecutive placeholder cells of one image row on one screen row. */
export interface Strip {
  /** Screen row (0 = top of the viewport) and first column. */
  y: number;
  x: number;
  len: number;
  image: number;
  /** Image row and first image column. */
  row: number;
  col: number;
}

/** Minimal slice of xterm's IBufferCell / IBufferLine used for scanning. */
export interface CellLike {
  getChars(): string;
  getWidth(): number;
  isFgRGB(): boolean;
  isFgPalette(): boolean;
  getFgColor(): number;
}
export interface LineLike {
  getCell(x: number, reuse?: never): CellLike | undefined;
}

export function fgOf(cell: CellLike): FgColor {
  if (cell.isFgRGB()) return { kind: "rgb", value: cell.getFgColor() };
  if (cell.isFgPalette()) return { kind: "palette", value: cell.getFgColor() };
  return { kind: "default" };
}

/** Appends the strips of one screen row to `out`, calling `onCell` for every placeholder. */
export function scanLine(
  line: LineLike | undefined,
  y: number,
  cols: number,
  out: Strip[],
  onCell?: (r: Resolved) => void,
) {
  if (!line) return;
  let prev: Resolved | null = null;
  let cur: Strip | null = null;
  for (let x = 0; x < cols; x++) {
    const cell = line.getCell(x);
    let res: Resolved | null = null;
    if (cell && cell.getWidth() > 0) {
      const raw = parseCell(cell.getChars(), fgOf(cell));
      if (raw && raw.idLow !== 0) res = resolve(raw, prev);
    }
    prev = res;
    if (!res) {
      cur = null;
      continue;
    }
    onCell?.(res);
    if (cur && cur.image === res.image && cur.row === res.row && cur.col + cur.len === res.col && cur.x + cur.len === x) {
      cur.len++;
    } else {
      cur = { y, x, len: 1, image: res.image, row: res.row, col: res.col };
      out.push(cur);
    }
  }
}

export interface Box {
  left: number;
  top: number;
  width: number;
  height: number;
  bgWidth: number;
  bgHeight: number;
  bgX: number;
  bgY: number;
}

/**
 * Pixel geometry of a strip. The image is fitted (letterboxed, centered) into
 * `imgCols x imgRows` cells; the strip shows its slice of that box.
 */
export function stripBox(
  strip: Pick<Strip, "x" | "y" | "len" | "row" | "col">,
  imgCols: number,
  imgRows: number,
  natW: number,
  natH: number,
  cellW: number,
  cellH: number,
): Box {
  const boxW = imgCols * cellW;
  const boxH = imgRows * cellH;
  const scale = Math.min(boxW / natW, boxH / natH);
  const dw = natW * scale;
  const dh = natH * scale;
  const ox = (boxW - dw) / 2;
  const oy = (boxH - dh) / 2;
  return {
    left: strip.x * cellW,
    top: strip.y * cellH,
    width: strip.len * cellW,
    height: cellH,
    bgWidth: dw,
    bgHeight: dh,
    bgX: ox - strip.col * cellW,
    bgY: oy - strip.row * cellH,
  };
}

/**
 * Size of an image in cells: explicit `c`/`r`, else the extent seen in placeholders, else a
 * guess from the aspect ratio and cell size (only when nothing else is known).
 */
export function imageCells(
  cols: number | undefined,
  rows: number | undefined,
  seenCols: number,
  seenRows: number,
): { cols: number; rows: number } {
  return { cols: Math.max(1, cols ?? seenCols), rows: Math.max(1, rows ?? seenRows) };
}
