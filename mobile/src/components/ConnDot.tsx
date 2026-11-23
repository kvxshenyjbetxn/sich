import { useI18n } from "../ctx";
import { DOT_COLOR, canRetry, connDot, connLabel } from "../lib/connLabels";
import type { ConnState } from "../lib/types";
import { api } from "../api";

/** Connection indicator; tapping it retries when not connected. */
export function ConnDot({ conn, showLabel = false }: { conn: ConnState; showLabel?: boolean }) {
  const { t } = useI18n();
  const retry = canRetry(conn);
  const label = connLabel(conn, t);
  return (
    <button
      className="conn-dot-btn"
      title={retry ? `${label} — ${t("tap_to_retry")}` : label}
      aria-label={label}
      onClick={() => retry && void api.retryNow()}
    >
      <span className={`conn-dot ${conn.state === "reconnecting" || conn.state === "connecting" ? "pulse" : ""}`} style={{ background: DOT_COLOR[connDot(conn)] }} />
      {showLabel && <span className="conn-label">{label}</span>}
    </button>
  );
}
