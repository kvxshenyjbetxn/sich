import { useI18n } from "../ctx";
import type { ViewMode } from "./chatApi";
import "./chat.css";

/** "Чат / Термінал" switch for the terminal header. */
export function ModeToggle({ mode, onChange }: { mode: ViewMode; onChange: (m: ViewMode) => void }) {
  const { t } = useI18n();
  return (
    <div className="mode-toggle" role="group" aria-label={t("chat_mode_toggle")}>
      <button className={mode === "chat" ? "on" : ""} onClick={() => onChange("chat")}>
        {t("chat_mode_chat")}
      </button>
      <button className={mode === "terminal" ? "on" : ""} onClick={() => onChange("terminal")}>
        {t("chat_mode_terminal")}
      </button>
    </div>
  );
}
