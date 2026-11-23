import { useEffect, useRef, useState } from "react";
import { X } from "lucide-react";
import { useI18n } from "../ctx";
import { pinchScale } from "../chat/messages";
import { initialViewerHist, onViewerPop, onViewerUnmount, type ViewerHistState } from "../lib/viewerHistory";

/**
 * Owns one history entry while mounted: Android back closes the viewer (the screen-level popstate
 * handler never sees it, via a capture listener + stopImmediatePropagation).
 */
function useViewerHistory(onClose: () => void) {
  const closeRef = useRef(onClose);
  closeRef.current = onClose;
  useEffect(() => {
    let st: ViewerHistState = initialViewerHist();
    history.pushState({ imageViewer: true }, "");
    const onPop = (e: PopStateEvent) => {
      const r = onViewerPop(st);
      st = r.state;
      e.stopImmediatePropagation();
      if (r.action === "close") closeRef.current();
    };
    window.addEventListener("popstate", onPop, true);
    return () => {
      const u = onViewerUnmount(st);
      st = u.state;
      if (u.needBack) {
        // Keep swallowing until our own history.back() has been delivered, then detach.
        const swallow = (e: PopStateEvent) => {
          window.removeEventListener("popstate", swallow, true);
          e.stopImmediatePropagation();
        };
        window.removeEventListener("popstate", onPop, true);
        window.addEventListener("popstate", swallow, true);
        history.back();
      } else {
        window.removeEventListener("popstate", onPop, true);
      }
    };
  }, []);
}

/** Fullscreen image: pinch to zoom (the overlay scrolls when zoomed), tap the backdrop or X to close. */
export function ImageViewer({ src, alt, onClose }: { src: string; alt: string; onClose: () => void }) {
  const { t } = useI18n();
  const [scale, setScale] = useState(1);
  const start = useRef({ d: 0, s: 1 });
  useViewerHistory(onClose);

  const dist = (e: React.TouchEvent) =>
    Math.hypot(e.touches[0].clientX - e.touches[1].clientX, e.touches[0].clientY - e.touches[1].clientY);

  return (
    <div
      className="cm-viewer"
      onTouchStart={(e) => {
        if (e.touches.length === 2) start.current = { d: dist(e), s: scale };
      }}
      onTouchMove={(e) => {
        if (e.touches.length === 2 && start.current.d > 0) setScale(pinchScale(start.current.s, start.current.d, dist(e)));
      }}
    >
      <button className="cm-viewer-x" aria-label={t("chat_image_close")} onClick={onClose}>
        <X size={22} />
      </button>
      <div className="cm-viewer-scroll" onClick={(e) => e.target === e.currentTarget && onClose()}>
        <img src={src} alt={alt} style={{ width: `${scale * 100}%`, maxWidth: scale > 1 ? "none" : "100%" }} onDoubleClick={() => setScale((s) => (s > 1 ? 1 : 2.5))} />
      </div>
    </div>
  );
}
