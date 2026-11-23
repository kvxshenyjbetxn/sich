import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register } from 'claude-code'

import type { AgentRun, CtxExport, CtxRow, CtxSnapshot } from '../types'

// Export-only mod for Sich: measures the context breakdown and subagent runs of this session and
// writes them to `<sich config>/claude-ctx/<sessionId>.mod.json`. No pane, command or UI.
// It does nothing at all outside a Sich terminal (SICH_TERM_ID unset). Everything is fail-soft.

// Same snapshot format as the `context-panel` mod (version 1), plus `termId` and `socket`.
const snapshot = atom({ plugin: 'sich-context', key: 'snapshot' } as const, null, { shape: 'snap-5' })
// Skills invoked this session: name -> estimated tokens of the text they loaded.
const usedSkills = atom({ plugin: 'sich-context', key: 'usedSkills' } as const, {} as Record<string, number>)
// Subagent runs this session: agentId -> live token usage.
const agentRuns = atom({ plugin: 'sich-context', key: 'agentRuns' } as const, {} as Record<string, AgentRun>)

/** At most one `session.usage()` measurement per this many milliseconds. */
const MEASURE_INTERVAL_MS = 1500
/** At most one export per this many milliseconds from the subagent step stream. */
const STEP_EXPORT_INTERVAL_MS = 2000

type SichEnv = { termId: string; socket?: string } | null
let sichEnv: SichEnv | undefined

/** The Sich terminal this session runs in, or null outside Sich (cached after the first lookup). */
async function inSich($: EngineInterface): Promise<SichEnv> {
  if (sichEnv !== undefined) return sichEnv
  try {
    const termId = await $.env.get('SICH_TERM_ID')
    if (!termId) {
      sichEnv = null
    } else {
      const socket = await $.env.get('SICH_SOCKET')
      sichEnv = socket ? { termId, socket } : { termId }
    }
  } catch {
    // Not cached: try again on the next event.
    return null
  }
  return sichEnv
}

async function quiet<T>(work: () => Promise<T>): Promise<T | undefined> {
  try {
    return await work()
  } catch {
    return undefined
  }
}

function mcpTool(name: string, server: string): string {
  const prefix = `mcp__${server}__`
  return name.startsWith(prefix) ? name.slice(prefix.length) : name
}

function baseName(path: string): string {
  const parts = path.split(/[\\/]/)
  return parts[parts.length - 1] || path
}

function byTokens(a: { tokens: number }, b: { tokens: number }): number {
  return b.tokens - a.tokens
}

/** Session ids become file names: keep only [A-Za-z0-9_-]. */
function safeId(id: string): string {
  return id.replace(/[^A-Za-z0-9_-]/g, '')
}

/** `<x>/claude-mod/<name>` -> `<x>/claude-ctx` (either separator); null when the root has another shape. */
function exportDir(pluginRoot: string): string | null {
  const parts = pluginRoot.split(/[\\/]+/).filter(Boolean)
  const i = parts.length - 2
  if (i < 1 || parts[i] !== 'claude-mod') return null
  const sep = pluginRoot.includes('\\') ? '\\' : '/'
  const lead = /^[\\/]/.test(pluginRoot) ? sep : ''
  return `${lead}${parts.slice(0, i).join(sep)}${sep}claude-ctx`
}

async function lookupAgent($: EngineInterface, id: string): Promise<{ type: string; description: string } | null> {
  try {
    const a = (await $.agent.list()).find(x => x.id === id)
    return a ? { type: a.type, description: a.description } : null
  } catch {
    return null
  }
}

/** Apply `fn` to the run of `id`, creating it first; `resolve` re-reads type and description. */
async function trackRun(
  $: EngineInterface,
  id: string,
  fn: (r: AgentRun) => AgentRun,
  resolve = false,
): Promise<void> {
  const cur = (await read($, agentRuns))[id]
  const info = !cur || resolve ? await lookupAgent($, id) : null
  let at = 0
  if (!cur) {
    try {
      at = await $.clock.now()
    } catch {
      at = 0
    }
  }
  await update($, agentRuns, m => {
    const base: AgentRun = m[id] ?? {
      id,
      type: 'agent',
      description: '',
      status: 'running',
      steps: 0,
      lastCtx: 0,
      output: 0,
      cacheRead: 0,
      cacheWrite: 0,
      input: 0,
      total: 0,
      startedAt: at,
      model: '',
    }
    return { ...m, [id]: fn(info ? { ...base, ...info } : base) }
  })
}

let exporting: Promise<void> | null = null
let exportDirty = false
let lastStepExport = 0

/**
 * Write the snapshot file; never throws. `endedId`: mark that (ending) session's file ended, e.g.
 * after /clear, where the process goes on under a new id.
 */
async function exportSnapshot($: EngineInterface, endedId?: string): Promise<void> {
  if (endedId) {
    await quiet(async () => exporting)
    return writeExport($, endedId)
  }
  if (exporting) {
    exportDirty = true
    return exporting
  }
  exporting = (async () => {
    do {
      exportDirty = false
      await writeExport($)
    } while (exportDirty)
  })().finally(() => {
    exporting = null
  })
  return exporting
}

async function writeExport($: EngineInterface, endedId?: string): Promise<void> {
  try {
    const env = await inSich($)
    if (!env) return
    const dir = exportDir($.plugin.root)
    if (!dir) return
    const sessionId = endedId ?? (await $.session.id())
    const name = safeId(sessionId)
    if (!name) return
    const data: CtxExport = {
      version: 1,
      sessionId,
      cwd: await $.session.cwd(),
      root: await $.session.root(),
      updatedAt: await $.clock.now(),
      ended: Boolean(endedId),
      snapshot: await read($, snapshot),
      agentRuns: Object.values(await read($, agentRuns)).sort((a, b) => b.startedAt - a.startedAt),
      usedSkills: await read($, usedSkills),
      termId: env.termId,
      ...(env.socket ? { socket: env.socket } : {}),
    }
    const sep = dir.includes('\\') ? '\\' : '/'
    // The engine offers no rename, so the file is written in place; readers must tolerate a torn read.
    await $.fs.write(`${dir}${sep}${name}.mod.json`, JSON.stringify(data))
  } catch {
    // Export is best effort: never disturb Claude.
  }
}

let measuring = false
let measureAgain = false
let lastMeasure = 0
let measureTimer: ReturnType<typeof setTimeout> | null = null

async function measure($: EngineInterface): Promise<void> {
  try {
    const usage = await $.session.usage({ breakdown: 'summary' })
    const b = usage.context.breakdown
    if (!b) return
    const rows: CtxRow[] = b.categories.map(c => ({ name: c.name, tokens: c.tokens, color: c.color, kind: c.kind }))
    const snap: CtxSnapshot = {
      model: b.model,
      total: b.totalTokens,
      max: b.maxTokens,
      percent: b.percentage,
      rows,
      mcp: b.mcpTools
        .map(t => ({
          name: mcpTool(t.name, t.serverName),
          detail: t.serverName,
          tokens: t.tokens,
          loaded: t.isLoaded,
        }))
        // Loaded tools (really in the context) first, then the deferred ones.
        .sort((a, b) => Number(b.loaded) - Number(a.loaded) || byTokens(a, b)),
      memory: b.memoryFiles.map(f => ({ name: baseName(f.path), detail: f.path, type: f.type, tokens: f.tokens })),
      skills: {
        count: b.skills?.totalSkills ?? 0,
        tokens: b.skills?.tokens ?? 0,
        items: (b.skills?.skillFrontmatter ?? [])
          .map(k => ({ name: k.name, detail: k.pluginName, source: k.source, tokens: k.tokens }))
          .sort(byTokens),
      },
      // Custom agents (built-in ones are left out by the engine).
      agents: (b.agents ?? []).map(a => ({ name: a.agentType, source: a.source, tokens: a.tokens })).sort(byTokens),
      detail: 'summary',
      at: await $.clock.now(),
    }
    await update($, snapshot, () => snap)
    await exportSnapshot($)
  } catch {
    // Measuring is best effort.
  }
}

/** Throttled, coalesced measurement: runs now, or once after the interval; bursts collapse into one. */
function requestMeasure($: EngineInterface): void {
  if (measuring) {
    measureAgain = true
    return
  }
  if (measureTimer) return
  const wait = MEASURE_INTERVAL_MS - (Date.now() - lastMeasure)
  const run = () => {
    measureTimer = null
    measuring = true
    lastMeasure = Date.now()
    void measure($).finally(() => {
      measuring = false
      if (measureAgain) {
        measureAgain = false
        requestMeasure($)
      }
    })
  }
  if (wait <= 0) {
    run()
    return
  }
  try {
    measureTimer = setTimeout(run, wait)
  } catch {
    // No timers: the next event measures instead.
  }
}

// ---- Image manifest: `<sich config>/claude-ctx/<sessionId>.img.json`, plus the base64 text files of
// pasted / MCP images in `<sich config>/claude-ctx/img/<sessionId>/<id>.b64`. Read by Sich (Images tab).
const IMG_MANIFEST_MAX = 300
const IMAGE_EXT = /\.(png|jpe?g|gif|bmp|webp)$/i
type ImgEntry = {
  id: string
  name: string
  source: 'read' | 'paste' | 'mcp'
  path: string | null
  data: string | null
  mime: string | null
  at: number
}
const imgManifest: { id: string; images: ImgEntry[] } = { id: '', images: [] }
let imgChain: Promise<unknown> = Promise.resolve()
let imgSeq = 0

/** Unique within the session and safe as a file name. */
const newImgId = () => `img-${Date.now()}-${imgSeq++}`

/** `build` makes the entry (it may write the .b64 first; null = nothing to record), then the manifest is rewritten. */
type ImgBuild = (dir: string, sep: string, session: string) => Promise<ImgEntry | null>

/** Serialized, never throws. A repeated file path moves to the end with the new time instead of duplicating. */
function recordImage($: EngineInterface, build: ImgBuild): Promise<void> {
  const run = async () => {
    try {
      const env = await inSich($)
      if (!env) return
      const dir = exportDir($.plugin.root)
      if (!dir) return
      const sessionId = await $.session.id()
      const name = safeId(sessionId)
      if (!name) return
      const sep = dir.includes('\\') ? '\\' : '/'
      const file = `${dir}${sep}${name}.img.json`
      if (imgManifest.id !== sessionId) {
        imgManifest.id = sessionId
        imgManifest.images = []
        try {
          const old = JSON.parse(await $.fs.read(file))
          if (Array.isArray(old?.images)) imgManifest.images = old.images
        } catch {
          // no earlier manifest
        }
      }
      const entry = await build(dir, sep, name)
      if (!entry) return
      const kept = entry.path ? imgManifest.images.filter(x => x.path !== entry.path) : imgManifest.images
      imgManifest.images = [...kept, entry].slice(-IMG_MANIFEST_MAX)
      await $.fs.write(
        file,
        JSON.stringify({
          version: 1,
          sessionId,
          cwd: await $.session.cwd(),
          termId: env.termId,
          socket: env.socket ?? null,
          updatedAt: Date.now(),
          images: imgManifest.images,
        }),
      )
    } catch {
      // The manifest is best effort.
    }
  }
  const p = imgChain.then(run, run)
  imgChain = p
  return p
}

/** An image that exists as a file on disk. */
function recordImageFile($: EngineInterface, source: 'read' | 'paste', path: string): Promise<void> {
  return recordImage($, async () => ({
    id: newImgId(),
    name: baseName(path),
    source,
    path,
    data: null,
    mime: null,
    at: Date.now(),
  }))
}

/** An image held as bytes: stored as base64 text in `img/<session>/<id>.b64`. */
function recordImageData(
  $: EngineInterface,
  source: 'paste' | 'mcp',
  name: string,
  b64: string,
  mime: string | null,
): Promise<void> {
  return recordImage($, async (dir, sep, session) => {
    const id = newImgId()
    const data = `${dir}${sep}img${sep}${session}${sep}${id}.b64`
    await $.fs.write(data, b64)
    return { id, name, source, path: null, data, mime, at: Date.now() }
  })
}

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    if (await inSich($)) requestMeasure($)
    return next(e)
  })

  on('session.end', async ($, e, next) => {
    const active = await inSich($)
    if (active) await exportSnapshot($, e.sessionId)
    const r = await next(e)
    // /clear and resume keep the process alive under a new id: publish it right away.
    if (active && (e.reason === 'clear' || e.reason === 'resume')) requestMeasure($)
    return r
  })

  // After every turn (and after compaction) the engine reports the new fill.
  on('session.measure', async ($, e, next) => {
    if (e.changed.includes('context') && (await inSich($))) requestMeasure($)
    return next(e)
  })

  // A skill's full text joins the conversation when it runs: note its rough size (~4 chars a token).
  on('skill.prompt', async ($, e, next) => {
    const r = await next(e)
    if (await inSich($)) {
      const est = Math.ceil(r.text.length / 4)
      await quiet(() => update($, usedSkills, m => ({ ...m, [e.skill]: (m[e.skill] ?? 0) + est })))
      await exportSnapshot($)
    }
    return r
  })

  // Subagent runs: the main loop (no agentId) is ignored.
  on('turn.start', async ($, e, next) => {
    if (e.agentId && (await inSich($))) {
      const id = e.agentId
      await quiet(() => trackRun($, id, r => ({ ...r, status: 'running' })))
      await exportSnapshot($)
    }
    return next(e)
  })

  // Pure observer: the stream and its result pass through untouched; the result is only counted.
  on('turn.step', async function* ($, e, next) {
    const r = yield* next(e)
    const id = e.agentId
    const u = r.usage
    if (id && u && (await inSich($))) {
      const ctx = u.input_tokens + u.cache_read_input_tokens + u.cache_creation_input_tokens
      await quiet(() =>
        trackRun($, id, run => ({
          ...run,
          steps: run.steps + 1,
          lastCtx: ctx,
          input: run.input + u.input_tokens,
          output: run.output + u.output_tokens,
          cacheRead: run.cacheRead + u.cache_read_input_tokens,
          cacheWrite: run.cacheWrite + u.cache_creation_input_tokens,
          total: run.total + ctx + u.output_tokens,
          model: u.model,
        })),
      )
      const now = (await quiet(() => $.clock.now())) ?? 0
      if (now - lastStepExport >= STEP_EXPORT_INTERVAL_MS) {
        lastStepExport = now
        await exportSnapshot($)
      }
    }
    return r
  })

  on('turn.complete', async ($, e, next) => {
    const r = await next(e)
    const id = e.agentId
    if (id && (await inSich($))) {
      const u = e.usage
      await quiet(() =>
        trackRun(
          $,
          id,
          run => {
            const done: AgentRun = { ...run, status: e.reason === 'error' ? 'failed' : 'done' }
            // Fallback only: steps already counted their usage.
            if (run.steps === 0 && u) {
              const ctx = u.input_tokens + u.cache_read_input_tokens + u.cache_creation_input_tokens
              return {
                ...done,
                steps: 1,
                lastCtx: ctx,
                input: u.input_tokens,
                output: u.output_tokens,
                cacheRead: u.cache_read_input_tokens,
                cacheWrite: u.cache_creation_input_tokens,
                total: ctx + u.output_tokens,
                model: u.model,
              }
            }
            return done
          },
          true,
        ),
      )
      await exportSnapshot($)
    }
    return r
  })

  // ---- Image manifest (see recordImage). Fail-soft, and only active inside Sich.

  // Images pasted into the prompt: read the bytes while the submission is in flight.
  on('prompt.submit', async ($, e, next) => {
    try {
      if (await inSich($)) {
        let found = 0
        for (const a of e.attachments ?? []) {
          if (a.type !== 'image' || !a.id) continue
          try {
            let b64 = ''
            let mime: string | null = a.mediaType ?? null
            let offset: number | undefined
            do {
              const m: any = await $.prompt.media({ id: a.id, offset })
              if (m.unreadable || m.deny || typeof m.base64 !== 'string') break
              b64 += m.base64
              mime = m.mediaType ?? mime
              offset = m.next
            } while (offset !== undefined)
            if (b64) {
              found++
              void recordImageData($, 'paste', a.filename ?? 'pasted image', b64, mime)
            }
          } catch {
            // never block the prompt over a manifest entry
          }
        }
        if (found === 0) {
          // Fallback: image paths typed or pasted as text.
          for (const m of (e.text ?? '').matchAll(/[A-Za-z]:[\\/][^\]\n"<>|?*]+?\.(?:png|jpe?g|gif|bmp|webp)\b/gi)) {
            const p = m[0]
            try {
              const st = await $.fs.stat(p)
              if (st.kind === 'file') void recordImageFile($, 'paste', p)
            } catch {
              // not a file
            }
          }
        }
      }
    } catch {
      // never block the prompt
    }
    return next(e)
  }).catch(($, e, next) => next(e))

  // Images other tools return (MCP screenshots and the like): image content blocks.
  on('tool.call', async ($, e, next) => {
    const ran: any = await next(e)
    if (e.tool === 'Read' || ran.deny !== undefined || ran.isError) return ran
    if (!(await inSich($))) return ran
    const blocks: any[] = Array.isArray(ran.result?.content) ? ran.result.content : Array.isArray(ran.result) ? ran.result : []
    let k = 0
    for (const b of blocks) {
      if (b?.type !== 'image') continue
      const data = typeof b.data === 'string' ? b.data : typeof b.source?.data === 'string' ? b.source.data : undefined
      if (!data) continue
      const mime = b.mimeType ?? b.source?.media_type ?? null
      const short = e.tool.replace(/^mcp__/, '').replace(/__/g, ' · ')
      const name = `${short}${k++ ? ` #${k}` : ''}`
      void recordImageData($, 'mcp', name, data, typeof mime === 'string' ? mime : null)
    }
    return ran
  }).catch(($, e, next) => next(e))

  // Image files Claude reads.
  on('tool.call', { tool: 'Read' }, async ($, e, next) => {
    const ran = await next(e)
    if (IMAGE_EXT.test(e.file_path) && ran.deny === undefined && !ran.isError && (await inSich($))) {
      void recordImageFile($, 'read', e.file_path)
    }
    return ran
  }).catch(($, e, next) => next(e))
}
