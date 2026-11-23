import { useMemo, useState } from "react";
import { ChevronDown, ChevronRight, FolderGit2, Folder, GitBranch, House, Search, Settings as Gear, Monitor, SquareTerminal } from "lucide-react";
import { useI18n } from "../ctx";
import { AgentBadge } from "../components/AgentBadge";
import { ConnDot } from "../components/ConnDot";
import { KIND_COLOR, agentLine, badgeFor, rollup } from "../lib/agent";
import { api } from "../api";
import { canRetry, connLabel } from "../lib/connLabels";
import { allTerminals, buildTree, filterTree, worktreeLabel, type ProjectNode } from "../lib/workspaceTree";
import { sameTerm, type ConnState, type Terminal, type TermRef, type Workspace } from "../lib/types";

const COLLAPSED_KEY = "sich.collapsed";

function loadCollapsed(): Set<number> {
  try {
    const raw = JSON.parse(localStorage.getItem(COLLAPSED_KEY) ?? "[]");
    return new Set(Array.isArray(raw) ? raw.filter((x) => typeof x === "number") : []);
  } catch {
    return new Set();
  }
}

function saveCollapsed(s: Set<number>) {
  try {
    localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...s]));
  } catch {
    /* ignore */
  }
}

interface Props {
  workspace: Workspace | null;
  conn: ConnState;
  hostName: string;
  selected: TermRef | null;
  onOpen: (t: TermRef) => void;
  onHosts: () => void;
  onSettings: () => void;
}

export function WorkspaceScreen({ workspace, conn, hostName, selected, onOpen, onHosts, onSettings }: Props) {
  const { t } = useI18n();
  const [query, setQuery] = useState("");
  const [collapsed, setCollapsed] = useState<Set<number>>(loadCollapsed);

  const tree = useMemo(() => (workspace ? buildTree(workspace) : null), [workspace]);
  const shown = useMemo(() => (tree ? filterTree(tree, query) : null), [tree, query]);
  const filtering = query.trim().length > 0;

  const toggle = (id: number) => {
    const next = new Set(collapsed);
    if (!next.delete(id)) next.add(id);
    setCollapsed(next);
    saveCollapsed(next);
  };

  const row = (term: Terminal) => (
    <TermRow key={`${term.term.daemon}#${term.term.id}`} term={term} active={sameTerm(selected, term.term)} onOpen={onOpen} />
  );

  const projectView = (pn: ProjectNode) => {
    const open = filtering || !collapsed.has(pn.project.id);
    const terms = pn.worktrees.flatMap((w) => w.terminals);
    const badge = rollup(terms.map((x) => x.agent));
    return (
      <div key={pn.project.id} className="project">
        <button className="row project-row" onClick={() => toggle(pn.project.id)}>
          {open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
          {pn.project.is_git ? <FolderGit2 size={15} /> : <Folder size={15} />}
          <span className="grow ellipsis">{pn.project.name}</span>
          {!open && badge && <AgentBadge kind={badge} />}
          {!open && terms.length > 0 && <span className="count">{terms.length}</span>}
        </button>
        {pn.project.error && <div className="muted small pad-left">{pn.project.error}</div>}
        {open &&
          pn.worktrees.map((wn) => (
            <div key={wn.worktree.path} className="worktree">
              <div className="row worktree-row">
                <GitBranch size={13} />
                <span className="grow ellipsis">{worktreeLabel(wn.worktree)}</span>
                {wn.worktree.is_main && <span className="tag">{t("main_worktree")}</span>}
                {wn.worktree.dirty && <span className="dirty-dot" title={t("uncommitted")} />}
              </div>
              {wn.terminals.map(row)}
            </div>
          ))}
      </div>
    );
  };

  const empty = shown && allTerminals(shown).length === 0 && shown.groups.length === 0;

  return (
    <div className="screen workspace-screen">
      <header className="topbar">
        <button className="host-btn" onClick={onHosts} title={t("hosts_title")}>
          <Monitor size={16} />
          <span className="ellipsis">{hostName}</span>
        </button>
        <ConnDot conn={conn} />
        <button className="icon-btn" onClick={onSettings} aria-label={t("settings")}>
          <Gear size={18} />
        </button>
      </header>
      {conn.state !== "connected" && (
        <button className="banner" onClick={() => canRetry(conn) && void api.retryNow()}>
          {connLabel(conn, t)}
        </button>
      )}
      <div className="filter">
        <Search size={14} />
        <input value={query} onChange={(e) => setQuery(e.target.value)} placeholder={t("filter_placeholder")} autoCapitalize="off" autoCorrect="off" spellCheck={false} />
      </div>
      <div className="list">
        {!shown && <div className="empty">{t("waiting_workspace")}</div>}
        {shown && shown.home.length > 0 && (
          <div className="section">
            <div className="row section-row">
              <House size={14} />
              <span className="grow">{t("home")}</span>
            </div>
            {shown.home.map(row)}
          </div>
        )}
        {shown?.groups.map((g) => (
          <div key={g.group?.id ?? "none"} className="section">
            {g.group && <div className="group-title">{g.group.name}</div>}
            {g.projects.map(projectView)}
          </div>
        ))}
        {empty && shown.home.length === 0 && <div className="empty">{t("no_terminals")}</div>}
      </div>
    </div>
  );
}

function TermRow({ term, active, onOpen }: { term: Terminal; active: boolean; onOpen: (t: TermRef) => void }) {
  const { t } = useI18n();
  const kind = term.agent?.kind;
  const badge = badgeFor(term.agent);
  const line = term.exited
    ? term.exit_code != null
      ? t("exited_code", { code: term.exit_code })
      : t("exited")
    : agentLine(term.agent);
  return (
    <button className={`row term-row ${active ? "active" : ""} ${term.exited ? "dim" : ""}`} onClick={() => onOpen(term.term)}>
      <SquareTerminal size={16} style={{ color: kind ? KIND_COLOR[kind] : "var(--muted)" }} />
      <span className="term-text">
        <span className="term-title">
          <span className="ellipsis">{term.title || term.cwd}</span>
          {term.agent?.unread && <span className="unread-dot" title={t("unread")} />}
        </span>
        {line && <span className="muted small ellipsis">{line}</span>}
      </span>
      <AgentBadge kind={badge} />
    </button>
  );
}
