import { Minus, Plus, X } from "lucide-react";
import { useI18n } from "../ctx";
import { LANGS, type Lang } from "../lib/i18n";
import { clampFont } from "../lib/termLogic";

const LANG_NAMES: Record<Lang, string> = { uk: "Українська", ru: "Русский", en: "English" };

interface Props {
  fontSize: number;
  onFont: (px: number) => void;
  onClose: () => void;
}

export function Settings({ fontSize, onFont, onClose }: Props) {
  const { t, lang, setLang } = useI18n();
  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <span className="title grow">{t("settings")}</span>
          <button className="icon-btn" onClick={onClose} aria-label={t("close")}>
            <X size={18} />
          </button>
        </div>
        <div className="setting">
          <span>{t("font_size")}</span>
          <div className="stepper">
            <button className="icon-btn" onClick={() => onFont(clampFont(fontSize - 0.5))} aria-label="-">
              <Minus size={16} />
            </button>
            <span className="stepper-val">{fontSize}px</span>
            <button className="icon-btn" onClick={() => onFont(clampFont(fontSize + 0.5))} aria-label="+">
              <Plus size={16} />
            </button>
          </div>
        </div>
        <div className="setting">
          <span>{t("language")}</span>
          <div className="seg">
            {LANGS.map((l) => (
              <button key={l} className={`seg-btn ${l === lang ? "on" : ""}`} onClick={() => setLang(l)}>
                {LANG_NAMES[l]}
              </button>
            ))}
          </div>
        </div>
      </div>
    </div>
  );
}
