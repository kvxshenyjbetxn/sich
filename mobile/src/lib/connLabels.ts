import type { ConnState } from "./types";
import type { Key, TFn } from "./i18n";

export type Dot = "green" | "amber" | "red";

export function connDot(s: ConnState): Dot {
  switch (s.state) {
    case "connected":
      return "green";
    case "connecting":
    case "reconnecting":
    case "pair_pending":
      return "amber";
    default:
      return "red";
  }
}

export const DOT_COLOR: Record<Dot, string> = {
  green: "var(--agent-done)",
  amber: "var(--agent-waiting)",
  red: "var(--agent-failed)",
};

const REJECT_KEYS: Record<string, Key> = {
  unpaired: "reject_unpaired",
  pair_secret_invalid: "reject_pair_secret_invalid",
  pair_denied: "reject_pair_denied",
  pair_timeout: "reject_pair_timeout",
  revoked: "reject_revoked",
  incompatible: "reject_incompatible",
  pair_failed: "reject_pair_failed",
  bad_address: "reject_bad_address",
};

export function rejectKey(reason: string): Key {
  return REJECT_KEYS[reason] ?? "reject_unknown";
}

export function connLabel(s: ConnState, t: TFn): string {
  switch (s.state) {
    case "idle":
      return t("conn_idle");
    case "connecting":
      return t("conn_connecting");
    case "connected":
      return t("conn_connected");
    case "reconnecting":
      return s.retry_in_ms > 0
        ? t("conn_reconnecting_in", { sec: Math.ceil(s.retry_in_ms / 1000), attempt: s.attempt })
        : t("conn_reconnecting", { attempt: s.attempt });
    case "pair_pending":
      return t("conn_pair_pending");
    case "rejected":
      return t(rejectKey(s.reason));
  }
}

/** Whether tapping the connection indicator should trigger a retry. */
export function canRetry(s: ConnState): boolean {
  return s.state !== "connected" && s.state !== "pair_pending";
}
