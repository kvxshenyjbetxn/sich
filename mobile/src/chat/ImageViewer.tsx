import { useRef, useState } from "react";
import { X } from "lucide-react";
import { useI18n } from "../ctx";
import { pinchScale } from "./messages";

/** Fullscreen image: pinch to zoom (the overlay scrolls when zoomed), tap the backdrop or X to close. */
export function ImageViewer({ src, alt, onClose }: { src: string; alt: string; onClose: () => void }) {
  const { t } = useI18n();
  const [scale, setScale] = useState(1);
  const start = useRef({ d: 0, s: 1 });

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
