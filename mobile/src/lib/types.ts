// Mirrors the JSON the Rust core emits (remote-proto types; `exited` is flattened, see ui.rs).

export interface TermRef {
  daemon: string;
  id: number;
}

export type AgentKind = "Claude" | "Gemini" | "Codex" | "Omp" | "Other";
export type AgentState = "Working" | "Waiting" | "Done" | "Failed" | "Idle";

export interface Agent {
  kind: AgentKind;
  state: AgentState;
  unread: boolean;
  since_unix_ms: number;
  title: string | null;
  detail: string | null;
  model: string | null;
  prompt: string | null;
  message: string | null;
  subagents_working: number;
}

export interface Terminal {
  term: TermRef;
  title: string;
  cwd: string;
  worktree: string | null;
  cols: number;
  rows: number;
  exited: boolean;
  exit_code: number | null;
  created_unix_ms: number;
  agent: Agent | null;
}

export interface Worktree {
  path: string;
  branch: string | null;
  is_main: boolean;
  dirty: boolean;
}

export interface Project {
  id: number;
  name: string;
  path: string;
  is_git: boolean;
  group: number | null;
  worktrees: Worktree[];
  error: string | null;
}

export interface Group {
  id: number;
  name: string;
}

export interface Workspace {
  host_name: string;
  groups: Group[];
  projects: Project[];
  terminals: Terminal[];
}

export interface Host {
  id: string;
  name: string;
  endpoint_id: string;
  addrs: string[];
  relay: string | null;
  paired_unix_ms: number;
}

export type ConnState =
  | { state: "idle" }
  | { state: "connecting" }
  | { state: "connected"; host_name: string }
  | { state: "reconnecting"; attempt: number; retry_in_ms: number }
  | { state: "pair_pending" }
  | { state: "rejected"; reason: string; detail: string | null };

export function termKey(t: TermRef): string {
  return `${t.daemon}#${t.id}`;
}

export function sameTerm(a: TermRef | null | undefined, b: TermRef | null | undefined): boolean {
  return !!a && !!b && a.daemon === b.daemon && a.id === b.id;
}
