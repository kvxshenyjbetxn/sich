import { useEffect, useRef, useState } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { ImageAddon } from "@xterm/addon-image";
import "@xterm/xterm/css/xterm.css";
import { imageAddonOptions } from "../lib/imageOptions";
import { api } from "../api";
import { WriteQueue, fontForWidth, pinchFont, type TermFrame } from "../lib/termLogic";
import type { TermRef } from "../lib/types";
import { KittyApcFilter } from "../lib/kitty/apc";
import { ImageStore, applyEvent } from "../lib/kitty/store";
import { attachImageOverlay } from "../lib/kitty/overlay";
import { hitStrip, isTap, type PointerSample, type StripRect } from "../lib/kitty/hit";
import { ImageViewer } from "./ImageViewer";

import { FONT_FAMILY, loadTermFonts } from "../lib/fonts";
export { FONT_FAMILY };

// Colors from crates/app/src/colors.rs (Gray scheme).
const THEME = {
  background: "#1e1f22",
  foreground: "#dfe1e5",
  cursor: "#dfe1e5",
  selectionBackground: "#2e436e",
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

/** Glyph advance / font size of the terminal font, measured once with a canvas. */
let cachedRatio: number | null = null;
function charRatio(): number {
  if (cachedRatio) return cachedRatio;
  try {
    const ctx = document.createElement("canvas").getContext("2d");
    if (ctx) {
      ctx.font = `100px ${FONT_FAMILY}`;
      const w = ctx.measureText("M").width;
      if (w > 20 && w < 100) cachedRatio = w / 100;
    }
  } catch {
    /* fall back below */
  }
  return cachedRatio ?? 0.6;
}

interface Props {
  term: TermRef;
  /** fit-to-phone (true) or render at the PTY size (false). */
  fit: boolean;
  baseFont: number;
  ptyCols: number;
  ptyRows: number;
  disabled: boolean;
  onData: (data: string) => void;
  onPinchFont: (px: number) => void;
  /** The gateway cleared the terminal and reported its size. */
  onReset: (cols: number, rows: number) => void;
  /** Chat view is shown: keep the subscription but do not lay out / resize the PTY. */
  paused?: boolean;
}

export function TerminalView({ term, fit, baseFont, ptyCols, ptyRows, disabled, onData, onPinchFont, onReset, paused = false }: Props) {
  const hostRef = useRef<HTMLDivElement>(null);
  const innerRef = useRef<HTMLDivElement>(null);
  const xRef = useRef<XTerm | null>(null);
  const fitAddon = useRef<FitAddon | null>(null);
  const lastSent = useRef({ cols: 0, rows: 0 });
  const [viewer, setViewer] = useState<string | null>(null);

  // Latest props for callbacks created once per terminal.
  const live = useRef({ fit, baseFont, ptyCols, ptyRows, onData, onPinchFont, onReset, disabled, term, paused });
  live.current = { fit, baseFont, ptyCols, ptyRows, onData, onPinchFont, onReset, disabled, term, paused };

  /** Apply size/font for the current mode. */
  const layout = useRef(() => {});
  layout.current = () => {
    const x = xRef.current;
    const f = fitAddon.current;
    const host = hostRef.current;
    if (!x || !f || !host || live.current.paused) return;
    const { fit, baseFont, ptyCols, ptyRows, term } = live.current;
    if (fit) {
      if (x.options.fontSize !== baseFont) x.options.fontSize = baseFont;
      const dims = f.proposeDimensions();
      if (dims && dims.cols > 1 && dims.rows > 1) {
        if (dims.cols !== x.cols || dims.rows !== x.rows) x.resize(dims.cols, dims.rows);
        if (dims.cols !== lastSent.current.cols || dims.rows !== lastSent.current.rows) {
          lastSent.current = { cols: dims.cols, rows: dims.rows };
          void api.resize(term, dims.cols, dims.rows);
        }
      }
    } else {
      const font = fontForWidth(host.clientWidth, ptyCols, charRatio(), baseFont);
      if (x.options.fontSize !== font) x.options.fontSize = font;
      if (x.cols !== ptyCols || x.rows !== ptyRows) x.resize(Math.max(1, ptyCols), Math.max(1, ptyRows));
    }
  };

  // One xterm + one subscription per terminal.
  useEffect(() => {
    let disposed = false;
    const queue = new WriteQueue<TermFrame>();
    const apc = new KittyApcFilter();
    const images = new ImageStore();
    let x: XTerm | null = null;
    let ro: ResizeObserver | null = null;
    let timer: number | undefined;
    const cleanups: Array<() => void> = [];
    const termRef = live.current.term;

    const start = async () => {
      // Make sure the bundled font is ready before xterm measures cells.
      try {
        await loadTermFonts(live.current.baseFont);
      } catch {
        /* measure with whatever is available */
      }
      const inner = innerRef.current;
      if (disposed || !inner) return;

      x = new XTerm({
        fontFamily: FONT_FAMILY,
        fontSize: live.current.baseFont,
        theme: THEME,
        cursorBlink: false,
        scrollback: 5000,
        allowProposedApi: true, // required by @xterm/addon-image
        disableStdin: false,
      });
      const f = new FitAddon();
      x.loadAddon(f);
      x.open(inner);
      try {
        x.loadAddon(new ImageAddon(imageAddonOptions()));
      } catch (e) {
        console.error("image addon failed to load", e);
      }
      xRef.current = x;
      fitAddon.current = f;
      let strips: StripRect[] = [];
      let screenEl: HTMLElement | null = null;
      cleanups.push(
        attachImageOverlay(x, images, THEME.background, (r, scr) => {
          strips = r;
          screenEl = scr;
        }),
      );

      x.onData((d) => {
        if (!live.current.disabled) live.current.onData(d);
      });

      // Everything that arrives (even before this point) goes through the queue.
      queue.attach((frame) => {
        const xt = xRef.current;
        if (!xt) return;
        if (frame.kind === "reset") {
          xt.reset();
          apc.reset();
          images.clear();
          live.current.onReset(frame.cols, frame.rows);
          layout.current();
        } else {
          // Kitty APC graphics are lifted out here; OSC 1337 / Sixel still reach addon-image.
          const { out, events } = apc.feed(frame.bytes);
          for (const ev of events) applyEvent(images, ev);
          if (out.length > 0) xt.write(out);
        }
      });

      layout.current();
      const cols = live.current.fit ? x.cols : live.current.ptyCols;
      const rows = live.current.fit ? x.rows : live.current.ptyRows;
      lastSent.current = { cols, rows };
      void api.subscribe(termRef, cols, rows, live.current.fit, (frame) => queue.push(frame));

      ro = new ResizeObserver(() => {
        window.clearTimeout(timer);
        timer = window.setTimeout(() => layout.current(), 80);
      });
      ro.observe(hostRef.current!);

      // Pinch to zoom the font.
      const host = hostRef.current!;
      let startDist = 0;
      let startFont = 0;
      const dist = (e: TouchEvent) => Math.hypot(e.touches[0].clientX - e.touches[1].clientX, e.touches[0].clientY - e.touches[1].clientY);
      const ts = (e: TouchEvent) => {
        if (e.touches.length === 2) {
          startDist = dist(e);
          startFont = live.current.baseFont;
        }
      };
      const tm = (e: TouchEvent) => {
        if (e.touches.length === 2 && startDist > 0) {
          e.preventDefault();
          live.current.onPinchFont(pinchFont(startFont, startDist, dist(e)));
        }
      };
      host.addEventListener("touchstart", ts, { passive: true });
      host.addEventListener("touchmove", tm, { passive: false });

      // Tap on a drawn inline image opens the fullscreen viewer (the overlay ignores pointers).
      let down: PointerSample | null = null;
      const pd = (e: PointerEvent) => {
        down = e.isPrimary ? { x: e.clientX, y: e.clientY, t: e.timeStamp } : null;
      };
      const pu = (e: PointerEvent) => {
        const d = down;
        down = null;
        if (!d || !screenEl || strips.length === 0) return;
        if (!isTap(d, { x: e.clientX, y: e.clientY, t: e.timeStamp })) return;
        if (window.getSelection()?.toString()) return;
        const box = screenEl.getBoundingClientRect();
        const id = hitStrip(strips, e.clientX - box.left, e.clientY - box.top);
        const img = id === null ? undefined : images.get(id);
        if (!img) return;
        e.preventDefault();
        e.stopPropagation();
        (document.activeElement as HTMLElement | null)?.blur?.();
        setViewer(img.url);
      };
      // The click that follows would focus xterm's textarea (keyboard): swallow it over an image.
      const ck = (e: MouseEvent) => {
        if (!screenEl) return;
        const box = screenEl.getBoundingClientRect();
        if (hitStrip(strips, e.clientX - box.left, e.clientY - box.top) !== null) {
          e.preventDefault();
          e.stopPropagation();
        }
      };
      const pc = () => {
        down = null;
      };
      host.addEventListener("pointerdown", pd, true);
      host.addEventListener("pointerup", pu, true);
      host.addEventListener("pointercancel", pc, true);
      host.addEventListener("click", ck, true);
      cleanups.push(() => {
        host.removeEventListener("pointerdown", pd, true);
        host.removeEventListener("pointerup", pu, true);
        host.removeEventListener("pointercancel", pc, true);
        host.removeEventListener("click", ck, true);
      });
      cleanups.push(() => {
        host.removeEventListener("touchstart", ts);
        host.removeEventListener("touchmove", tm);
      });
    };
    void start();

    return () => {
      disposed = true;
      window.clearTimeout(timer);
      ro?.disconnect();
      cleanups.forEach((c) => c());
      queue.detach();
      images.clear();
      void api.unsubscribe(termRef);
      x?.dispose();
      xRef.current = null;
      fitAddon.current = null;
    };
    // The terminal identity is the only thing that restarts the view.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [term.daemon, term.id]);

  // Switching back to fit-to-phone: the gateway ignored resizes while not fitting, so resend.
  useEffect(() => {
    if (fit) lastSent.current = { cols: 0, rows: 0 };
  }, [fit]);

  // Mode / size / font changes.
  useEffect(() => {
    layout.current();
  }, [fit, baseFont, ptyCols, ptyRows, paused]);

  return (
    <div ref={hostRef} className={`term-host ${fit ? "fit" : "free"}`}>
      <div ref={innerRef} className="term-inner" />
      {viewer && <ImageViewer src={viewer} alt="" onClose={() => setViewer(null)} />}
    </div>
  );
}
