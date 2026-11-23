/**
 * Draws Kitty placeholder images over an xterm terminal. After every render the visible
 * viewport is scanned through the public buffer API, placeholder cells are grouped into strips
 * and each strip becomes an absolutely positioned element (inside `.xterm-screen`) carrying its
 * slice of the image as a CSS background. The overlay is fully rebuilt on render / scroll /
 * resize / font changes, so it follows the buffer without private APIs.
 */
import type { Terminal } from "@xterm/xterm";
import { imageCells, scanLine, stripBox, type LineLike, type Strip } from "./placeholder";
import type { ImageStore } from "./store";

export function attachImageOverlay(term: Terminal, store: ImageStore, background: string): () => void {
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
    if (store.size === 0) {
      el.replaceChildren();
      return;
    }
    const screen = el.parentElement!;
    const cellW = screen.clientWidth / term.cols;
    const cellH = screen.clientHeight / term.rows;
    if (!(cellW > 0 && cellH > 0)) return;

    const buf = term.buffer.active;
    const strips: Strip[] = [];
    for (let y = 0; y < term.rows; y++) {
      scanLine(buf.getLine(buf.viewportY + y) as LineLike | undefined, y, term.cols, strips, (r) => {
        store.noteExtent(r.image, r.row, r.col);
      });
    }

    const frag = document.createDocumentFragment();
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
    }
    el.replaceChildren(frag);
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
