/** Image store: id -> PNG blob URL + geometry, with LRU eviction. */

export interface StoredImage {
  id: number;
  url: string;
  /** Natural size in pixels (from the PNG header). */
  width: number;
  height: number;
  bytes: number;
  /** Size in cells from the virtual placement (`c`/`r`), when known. */
  cols?: number;
  rows?: number;
  /** Largest row/column (+1) seen in placeholder cells, used when cols/rows are missing. */
  seenCols: number;
  seenRows: number;
}

export interface StoreOptions {
  maxBytes?: number;
  maxImages?: number;
  makeUrl?: (png: Uint8Array) => string;
  revokeUrl?: (url: string) => void;
}

const PNG_MAGIC = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];

/** Reads width/height from the IHDR chunk; null for anything that is not a PNG. */
export function pngSize(png: Uint8Array): { width: number; height: number } | null {
  if (png.length < 24) return null;
  for (let i = 0; i < 8; i++) if (png[i] !== PNG_MAGIC[i]) return null;
  const u32 = (o: number) => ((png[o] << 24) | (png[o + 1] << 16) | (png[o + 2] << 8) | png[o + 3]) >>> 0;
  const width = u32(16);
  const height = u32(20);
  if (width === 0 || height === 0) return null;
  return { width, height };
}

export class ImageStore {
  private map = new Map<number, StoredImage>(); // insertion order = LRU order (oldest first)
  private total = 0;
  private listeners = new Set<() => void>();
  private readonly maxBytes: number;
  private readonly maxImages: number;
  private readonly makeUrl: (png: Uint8Array) => string;
  private readonly revokeUrl: (url: string) => void;

  constructor(opts: StoreOptions = {}) {
    this.maxBytes = opts.maxBytes ?? 64 * 1024 * 1024;
    this.maxImages = opts.maxImages ?? 64;
    this.makeUrl = opts.makeUrl ?? ((png) => URL.createObjectURL(new Blob([png as BlobPart], { type: "image/png" })));
    this.revokeUrl = opts.revokeUrl ?? ((u) => URL.revokeObjectURL(u));
  }

  get size(): number {
    return this.map.size;
  }

  get bytes(): number {
    return this.total;
  }

  onChange(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private emit() {
    this.listeners.forEach((f) => f());
  }

  /** Stores (or replaces) the image. Returns false when the data is not a valid PNG. */
  set(id: number, png: Uint8Array): boolean {
    const dim = pngSize(png);
    if (!dim) return false;
    const old = this.map.get(id);
    if (old) this.drop(old);
    this.map.set(id, {
      id,
      url: this.makeUrl(png),
      width: dim.width,
      height: dim.height,
      bytes: png.length,
      cols: old?.cols,
      rows: old?.rows,
      seenCols: 0,
      seenRows: 0,
    });
    this.total += png.length;
    this.evict(id);
    this.emit();
    return true;
  }

  /** Records the cell size from a virtual placement. */
  place(id: number, cols?: number, rows?: number) {
    const img = this.map.get(id);
    if (!img) return;
    if (cols) img.cols = cols;
    if (rows) img.rows = rows;
    this.emit();
  }

  /** Looks an image up and marks it most recently used. */
  get(id: number): StoredImage | undefined {
    const img = this.map.get(id);
    if (img) {
      this.map.delete(id);
      this.map.set(id, img);
    }
    return img;
  }

  /** Raises the inferred extent from a placeholder cell; true if it grew. */
  noteExtent(id: number, row: number, col: number): boolean {
    const img = this.map.get(id);
    if (!img) return false;
    let grew = false;
    if (row + 1 > img.seenRows) {
      img.seenRows = row + 1;
      grew = true;
    }
    if (col + 1 > img.seenCols) {
      img.seenCols = col + 1;
      grew = true;
    }
    return grew;
  }

  delete(id: number) {
    const img = this.map.get(id);
    if (!img) return;
    this.drop(img);
    this.emit();
  }

  clear() {
    if (this.map.size === 0) return;
    for (const img of [...this.map.values()]) this.drop(img);
    this.emit();
  }

  private drop(img: StoredImage) {
    this.map.delete(img.id);
    this.total -= img.bytes;
    this.revokeUrl(img.url);
  }

  private evict(keep: number) {
    for (const [id, img] of [...this.map]) {
      if (this.map.size <= this.maxImages && this.total <= this.maxBytes) break;
      if (id === keep) continue;
      this.drop(img);
    }
  }
}

/** Applies a decoded Kitty event to the store. */
export function applyEvent(store: ImageStore, ev: import("./apc").KittyEvent) {
  switch (ev.type) {
    case "image":
      store.set(ev.id, ev.png);
      break;
    case "place":
      store.place(ev.id, ev.cols, ev.rows);
      break;
    case "delete":
      if (ev.all) store.clear();
      else store.delete(ev.id);
      break;
  }
}
