import type { Group, Project, Terminal, Workspace, Worktree } from "./types";

/** Compare paths from a Windows/Unix desktop: slashes, trailing slash, drive-letter case. */
export function normPath(p: string): string {
  let s = p.replace(/^\\\\\?\\/, "").replace(/\\/g, "/").replace(/\/+$/, "");
  if (/^[a-zA-Z]:/.test(s)) s = s.toLowerCase();
  return s;
}

export function samePath(a: string | null | undefined, b: string | null | undefined): boolean {
  return a != null && b != null && normPath(a) === normPath(b);
}

/** Desktop order: (created_unix_ms, id). */
export function sortTerminals(terms: Terminal[]): Terminal[] {
  return [...terms].sort((a, b) => a.created_unix_ms - b.created_unix_ms || a.term.id - b.term.id);
}

export interface WorktreeNode {
  worktree: Worktree;
  terminals: Terminal[];
}

export interface ProjectNode {
  project: Project;
  worktrees: WorktreeNode[];
}

export interface GroupNode {
  /** `null` = projects that belong to no group. */
  group: Group | null;
  projects: ProjectNode[];
}

export interface Tree {
  /** Terminals without a (known) worktree. */
  home: Terminal[];
  groups: GroupNode[];
}

export function buildTree(ws: Workspace): Tree {
  const terms = sortTerminals(ws.terminals);
  const claimed = new Set<Terminal>();

  const projectNodes = new Map<number, ProjectNode>();
  for (const project of ws.projects) {
    const worktrees: WorktreeNode[] = project.worktrees.map((worktree) => {
      const mine = terms.filter((t) => !claimed.has(t) && samePath(t.worktree, worktree.path));
      mine.forEach((t) => claimed.add(t));
      return { worktree, terminals: mine };
    });
    projectNodes.set(project.id, { project, worktrees });
  }

  const home = terms.filter((t) => !claimed.has(t));

  const groups: GroupNode[] = [];
  for (const group of ws.groups) {
    const projects = ws.projects.filter((p) => p.group === group.id).map((p) => projectNodes.get(p.id)!);
    groups.push({ group, projects });
  }
  const knownGroups = new Set(ws.groups.map((g) => g.id));
  const loose = ws.projects.filter((p) => p.group == null || !knownGroups.has(p.group));
  if (loose.length > 0) {
    groups.push({ group: null, projects: loose.map((p) => projectNodes.get(p.id)!) });
  }
  return { home, groups };
}

function matches(q: string, ...fields: (string | null | undefined)[]): boolean {
  return fields.some((f) => !!f && f.toLowerCase().includes(q));
}

function termMatches(q: string, t: Terminal): boolean {
  return matches(q, t.title, t.cwd, t.agent?.title, t.agent?.detail, t.agent?.message, t.agent?.kind);
}

/**
 * Keep only what matches `query` (case-insensitive). A project or worktree whose own name /
 * branch matches keeps all its terminals; otherwise only matching terminals remain.
 * Empty query returns the tree unchanged.
 */
export function filterTree(tree: Tree, query: string): Tree {
  const q = query.trim().toLowerCase();
  if (!q) return tree;
  const home = tree.home.filter((t) => termMatches(q, t));
  const groups: GroupNode[] = [];
  for (const g of tree.groups) {
    const groupHit = !!g.group && matches(q, g.group.name);
    const projects: ProjectNode[] = [];
    for (const pn of g.projects) {
      const projectHit = groupHit || matches(q, pn.project.name, pn.project.path);
      const worktrees: WorktreeNode[] = [];
      for (const wn of pn.worktrees) {
        const wtHit = projectHit || matches(q, wn.worktree.branch, wn.worktree.path);
        const terminals = wtHit ? wn.terminals : wn.terminals.filter((t) => termMatches(q, t));
        if (wtHit || terminals.length > 0) worktrees.push({ worktree: wn.worktree, terminals });
      }
      if (worktrees.length > 0) projects.push({ project: pn.project, worktrees });
    }
    if (projects.length > 0) groups.push({ group: g.group, projects });
  }
  return { home, groups };
}

export function allTerminals(tree: Tree): Terminal[] {
  const out = [...tree.home];
  for (const g of tree.groups) for (const p of g.projects) for (const w of p.worktrees) out.push(...w.terminals);
  return out;
}

/** Short branch label; detached/unknown shows the folder name. */
export function worktreeLabel(w: Worktree): string {
  if (w.branch) return w.branch;
  const parts = normPath(w.path).split("/");
  return parts[parts.length - 1] || w.path;
}
