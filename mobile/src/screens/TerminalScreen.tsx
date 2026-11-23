import { useCallback, useEffect, useRef, useState } from "react";
import { ArrowLeft, Monitor, Smartphone } from "lucide-react";
import { api, events } from "../api";
import { useI18n } from "../ctx";
import { AgentBadge } from "../components/AgentBadge";
import { Composer } from "../components/Composer";
import { ConnDot } from "../components/ConnDot";
import { KeysRow } from "../components/KeysRow";
import { TerminalView } from "../components/TerminalView";
import { KIND_COLOR, badgeFor, stateKey } from "../lib/agent";
import { applyCtrl, toBytes } from "../lib/keys";
import { loadFit, saveFit } from "../lib/termLogic";
import { sameTerm, type ConnState, type TermRef, type Terminal } from "../lib/types";

interface Props {
  term: TermRef;
  info: Terminal | null;
  workspaceLoaded: boolean;
  conn: ConnState;
  baseFont: number;
  onFont: (px: number) => void;
  /** null in two-pane mode (no back button). */
  onBack: (() => void) | null;
}

export function TerminalScreen({ term, info, workspaceLoaded, conn, baseFont, onFont, onBack }: Props) {
  const { t } = useI18n();
  const [fit, setFit] = useState<boolean>(loadFit);
  const [pty, setPty] = useState({ cols: info?.cols ?? 80, rows: info?.rows ?? 24 });
  const [ctrl, setCtrl] = useState(false);
  const ctrlRef = useRef(false);
  ctrlRef.current = ctrl;

  // Each terminal starts from its own desktop size.
  useEffect(() => {
    if (info) setPty({ cols: info.cols, rows: info.rows });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [term.daemon, term.id]);

  // PTY size / fit changes decided by the gateway (e.g. the desktop resized the terminal).
  useEffect(() => {
    let alive = true;
    let un: (() => void) | undefined;
    void events
      .termSize((e) => {
        if (!sameTerm(e.term, term)) return;
        setPty({ cols: e.cols, rows: e.rows });
        setFit(e.fit);
      })
      .then((u) => (alive ? (un = u) : u()));
    return () => {
      alive = false;
      un?.();
    };
  }, [term.daemon, term.id]);

  // Opening a terminal (and unread state arriving while it is open) marks it seen.
  const unread = info?.agent?.unread ?? false;
  useEffect(() => {
    if (conn.state === "connected") void api.markSeen(term);
  }, [term.daemon, term.id, unread, conn.state]);

  const exited = info?.exited ?? false;
  const offline = conn.state !== "connected";

  const sendRaw = useCallback(
    (data: string) => {
      const { out, consumed } = applyCtrl(data, ctrlRef.current);
      if (consumed) setCtrl(false);
      void api.sendInput(term, toBytes(out));
    },
    [term],
  );

  const toggleFit = () => {
    const next = !fit;
    saveFit(next);
    setFit(next);
    void api.setFit(term, next);
  };

  if (workspaceLoaded && !info) {
    return (
      <div className="screen centered">
        <p className="muted">{t("term_gone")}</p>
        {onBack && (
          <button className="btn" onClick={onBack}>
            {t("back")}
          </button>
        )}
      </div>
    );
  }

  const agent = info?.agent ?? null;
  const badge = badgeFor(agent);

  return (
    <div className="screen terminal-screen">
      <header className="topbar">
        {onBack && (
          <button className="icon-btn" onClick={onBack} aria-label={t("back")}>
            <ArrowLeft size={18} />
          </button>
        )}
        <div className="term-head grow">
          <span className="ellipsis term-head-title" style={{ color: agent ? KIND_COLOR[agent.kind] : undefined }}>
            {info?.title || info?.cwd || "…"}
          </span>
          {agent && (
            <span className="term-head-state">
              <AgentBadge kind={badge} size={13} />
              <span className="muted small">{t(stateKey(agent))}</span>
            </span>
          )}
        </div>
        <button className="icon-btn" onClick={toggleFit} aria-label={t("fit_toggle")} title={fit ? t("fit_phone") : t("fit_desktop")}>
          {fit ? <Smartphone size={18} /> : <Monitor size={18} />}
        </button>
        <ConnDot conn={conn} />
      </header>

      {offline && <div className="banner">{t("offline_hint")}</div>}
      {exited && <div className="banner">{info?.exit_code != null ? t("exited_code", { code: info.exit_code }) : t("exited")}</div>}

      <TerminalView
        key={`${term.daemon}#${term.id}`}
        term={term}
        fit={fit}
        baseFont={baseFont}
        ptyCols={pty.cols}
        ptyRows={pty.rows}
        disabled={exited}
        onData={sendRaw}
        onPinchFont={onFont}
        onReset={(cols, rows) => setPty({ cols, rows })}
      />

      <KeysRow ctrl={ctrl} disabled={exited} onToggleCtrl={() => setCtrl((c) => !c)} onKey={sendRaw} />
      <Composer disabled={exited} onSend={(text, ids) => void api.sendText(term, text, ids, true)} />
    </div>
  );
}
