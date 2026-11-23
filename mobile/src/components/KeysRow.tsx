import { ArrowDown, ArrowLeft, ArrowRight, ArrowUp, CornerDownLeft } from "lucide-react";
import type { ReactNode } from "react";
import { useI18n } from "../ctx";
import { keySequence, type KeyName } from "../lib/keys";

interface Props {
  ctrl: boolean;
  disabled: boolean;
  onToggleCtrl: () => void;
  /** Sends the sequence of a special key (sticky Ctrl is applied by the caller). */
  onKey: (seq: string) => void;
}

export function KeysRow({ ctrl, disabled, onToggleCtrl, onKey }: Props) {
  const { t } = useI18n();
  // Keep focus (and the soft keyboard) where it is when a key is tapped.
  const keep = (e: React.SyntheticEvent) => e.preventDefault();
  const key = (name: KeyName, label: ReactNode, extra = "") => (
    <button key={name} className={`key ${extra}`} disabled={disabled} onMouseDown={keep} onClick={() => onKey(keySequence(name))}>
      {label}
    </button>
  );
  return (
    <div className="keys-row">
      {key("esc", t("key_esc"))}
      {key("tab", t("key_tab"))}
      {key("shift_tab", "⇧Tab")}
      <button className={`key ${ctrl ? "on" : ""}`} onMouseDown={keep} onClick={onToggleCtrl} aria-pressed={ctrl}>
        {t("key_ctrl")}
      </button>
      {key("up", <ArrowUp size={16} />)}
      {key("down", <ArrowDown size={16} />)}
      {key("left", <ArrowLeft size={16} />)}
      {key("right", <ArrowRight size={16} />)}
      {key("ctrl_c", "^C")}
      {key("enter", <CornerDownLeft size={16} />)}
      {key("slash", "/")}
    </div>
  );
}
