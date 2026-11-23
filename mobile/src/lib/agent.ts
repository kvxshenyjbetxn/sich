import type { Agent, AgentKind } from "./types";
import type { Key } from "./i18n";

/** Badge kinds in roll-up priority order (highest first), like the desktop sidebar. */
export type BadgeKind = "waiting" | "working" | "failed" | "done" | "seen_done";
const PRIORITY: BadgeKind[] = ["waiting", "working", "failed", "done", "seen_done"];

export function badgeFor(agent: Agent | null | undefined): BadgeKind | null {
  if (!agent) return null;
  let main: BadgeKind | null;
  switch (agent.state) {
    case "Working":
      main = "working";
      break;
    case "Waiting":
      main = "waiting";
      break;
    case "Done":
      main = agent.unread ? "done" : "seen_done";
      break;
    case "Failed":
      main = agent.unread ? "failed" : null;
      break;
    default:
      main = null;
  }
  // A still-working subagent shows through a finished main agent.
  if (agent.subagents_working > 0 && (main === null || PRIORITY.indexOf("working") < PRIORITY.indexOf(main))) {
    return "working";
  }
  return main;
}

export function rollup(agents: (Agent | null | undefined)[]): BadgeKind | null {
  let best: BadgeKind | null = null;
  for (const a of agents) {
    const b = badgeFor(a);
    if (b && (best === null || PRIORITY.indexOf(b) < PRIORITY.indexOf(best))) best = b;
  }
  return best;
}

export const BADGE_COLOR: Record<BadgeKind, string> = {
  waiting: "var(--agent-waiting)",
  working: "var(--accent)",
  failed: "var(--agent-failed)",
  done: "var(--agent-done)",
  seen_done: "var(--agent-done)",
};

export const KIND_COLOR: Record<AgentKind, string> = {
  Claude: "#D97757",
  Gemini: "#4796E3",
  Omp: "#AA73F5",
  Codex: "var(--muted)",
  Other: "var(--muted)",
};

export function stateKey(agent: Agent): Key {
  switch (agent.state) {
    case "Working":
      return "agent_working";
    case "Waiting":
      return "agent_waiting";
    case "Done":
      return "agent_done";
    case "Failed":
      return "agent_failed";
    default:
      return "agent_idle";
  }
}

/** One short muted line: tool/wait detail, else the last message, else the title. */
export function agentLine(agent: Agent | null | undefined): string {
  if (!agent) return "";
  const pick = [agent.detail, agent.message, agent.title].find((s) => !!s && s.trim().length > 0);
  return (pick ?? "").replace(/\s+/g, " ").trim();
}
