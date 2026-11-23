import { CircleAlert, CircleCheck, CircleX, LoaderCircle } from "lucide-react";
import { BADGE_COLOR, type BadgeKind } from "../lib/agent";

/** Agent status icon: spinner (working), alert (waiting), check (done), x (failed), dot (seen). */
export function AgentBadge({ kind, size = 14 }: { kind: BadgeKind | null; size?: number }) {
  if (!kind) return <span className="badge-slot" style={{ width: size, height: size }} />;
  const color = BADGE_COLOR[kind];
  const common = { size, color, strokeWidth: 2.2 };
  return (
    <span className="badge-slot" style={{ width: size, height: size }}>
      {kind === "working" && <LoaderCircle {...common} className="spin" />}
      {kind === "waiting" && <CircleAlert {...common} />}
      {kind === "done" && <CircleCheck {...common} />}
      {kind === "failed" && <CircleX {...common} />}
      {kind === "seen_done" && <span className="seen-dot" style={{ background: color }} />}
    </span>
  );
}
