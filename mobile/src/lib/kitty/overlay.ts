/**
 * Draws Kitty placeholder images over an xterm terminal. After every render the visible
 * viewport is scanned through the public buffer API, placeholder cells are grouped into strips
 * and each strip becomes an absolutely positioned element (inside `.xterm-screen`) carrying its
 * slice of the image as a CSS background. The overlay is fully rebuilt on render / scroll /
 * resize / font changes, so it follows the buffer without private APIs.
 */
import type { Terminal } from "@xterm/xterm";
import { coverBox, imageCells, scanCover, scanLine, stripBox, type CoverRun, type LineLike, type Strip } from "./placeholder";
import type { ImageStore } from "./store";
import type { StripRect } from "./hit";

export function attachImageOverlay(term: Terminal, store: ImageStore, background: string, onLayout?: (rects: StripRect[], screen: HTMLElement) => void): () => void {
  let layer: HTMLDivElement | null = null;
  let raf = 0;
  let disposed = false;

  const ensureLayer = (): HTMLDivElement | null => {
    const screen = term.element?.querySelector<HTMLElement>(".xterm-screen");
    if (!screen) return null;
    if (layer && layer.parentElement === screen) return layer;
    layer?.remove();
    layer = document.createElement("div");
    layer.className = "kitty-layer";
    screen.appendChild(layer);
    return layer;
  };

  const draw = () => {
    raf = 0;
    if (disposed) return;
    const el = ensureLayer();
    if (!el) return;
    const screen = el.parentElement!;
    const cellW = screen.clientWidth / term.cols;
    const cellH = screen.clientHeight / term.rows;
    if (!(cellW > 0 && cellH > 0)) return;

    const buf = term.buffer.active;
    const strips: Strip[] = [];
    const covers: CoverRun[] = [];
    for (let y = 0; y < term.rows; y++) {
      const line = buf.getLine(buf.viewportY + y) as LineLike | undefined;
      scanCover(line, y, term.cols, covers);
      scanLine(line, y, term.cols, strips, (r) => {
        store.noteExtent(r.image, r.row, r.col);
      });
    }

    const rects: StripRect[] = [];
    const frag = document.createDocumentFragment();
    // Background boxes first: every placeholder cell is hidden, even without a loaded image.
    for (const c of covers) {
      const b = coverBox(c, cellW, cellH);
      const d = document.createElement("div");
      d.className = "kitty-strip";
      const st = d.style;
      st.left = `${b.left}px`;
      st.top = `${b.top}px`;
      st.width = `${b.width}px`;
      st.height = `${b.height}px`;
      st.backgroundColor = background;
      frag.appendChild(d);
    }
    for (const s of strips) {
      const img = store.get(s.image);
      if (!img) continue;
      const { cols, rows } = imageCells(img.cols, img.rows, img.seenCols, img.seenRows);
      const b = stripBox(s, cols, rows, img.width, img.height, cellW, cellH);
      const d = document.createElement("div");
      d.className = "kitty-strip";
      const st = d.style;
      st.left = `${b.left}px`;
      st.top = `${b.top}px`;
      st.width = `${b.width}px`;
      st.height = `${b.height}px`;
      st.backgroundColor = background;
      st.backgroundImage = `url("${img.url}")`;
      st.backgroundSize = `${b.bgWidth}px ${b.bgHeight}px`;
      st.backgroundPosition = `${b.bgX}px ${b.bgY}px`;
      frag.appendChild(d);
      rects.push({ image: s.image, left: b.left, top: b.top, width: b.width, height: b.height });
    }
    el.replaceChildren(frag);
    onLayout?.(rects, screen);
  };

  const schedule = () => {
    if (!raf && !disposed) raf = requestAnimationFrame(draw);
  };

  const subs = [term.onRender(schedule), term.onScroll(schedule), term.onResize(schedule)];
  const off = store.onChange(schedule);
  schedule();

  return () => {
    disposed = true;
    if (raf) cancelAnimationFrame(raf);
    subs.forEach((s) => s.dispose());
    off();
    layer?.remove();
  };
}
