export type CtxRow = { name: string; tokens: number; color: string; kind: 'used' | 'free' | 'buffer' | 'deferred' }
/** `detail`: MCP server, memory file path or skill plugin name. `type`: memory kind; `source`: engine source word. */
export type CtxItem = { name: string; detail?: string; type?: string; source?: string; tokens: number; loaded?: boolean }
export type CtxSnapshot = {
  model: string
  total: number
  max: number
  percent: number
  rows: CtxRow[]
  mcp: CtxItem[]
  memory: CtxItem[]
  skills: { count: number; tokens: number; items: CtxItem[] }
  /** Custom agents: `name` is the agent type. Optional: older exports lack it. */
  agents?: CtxItem[]
  detail: 'summary' | 'full'
  at: number
}

/** Live token usage of one subagent run, keyed by agentId. */
export type AgentRun = {
  id: string
  type: string
  description: string
  status: 'running' | 'done' | 'failed'
  steps: number
  /** Last step's input + cache read + cache creation: the agent's current context size. */
  lastCtx: number
  output: number
  cacheRead: number
  cacheWrite: number
  input: number
  /** Cumulative input + cacheRead + cacheWrite + output. */
  total: number
  startedAt: number
  model: string
}

/** The JSON written to `<sich config>/claude-ctx/<sessionId>.mod.json`. */
export type CtxExport = {
  version: 1
  sessionId: string
  cwd: string
  root: string
  updatedAt: number
  ended: boolean
  snapshot: CtxSnapshot | null
  /** Newest startedAt first. */
  agentRuns: AgentRun[]
  usedSkills: Record<string, number>
  /** `SICH_TERM_ID` of the Sich terminal running this session. */
  termId?: string
  /** `SICH_SOCKET` of the Sich daemon that owns that terminal. */
  socket?: string
}

declare module 'claude-code' {
  interface PluginState {
    'sich-context': { snapshot: Shaped<CtxSnapshot | null>; usedSkills: Record<string, number>; agentRuns: Record<string, AgentRun> }
  }
}
