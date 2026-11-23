import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { api } from "../api";
import { useI18n } from "../ctx";
import { AgentBadge } from "../components/AgentBadge";
import { Composer } from "../components/Composer";
import { badgeFor, stateKey } from "../lib/agent";
import type { Agent, TermRef } from "../lib/types";
import { ChatItemView } from "./ChatItems";
import { ImageViewer } from "../components/ImageViewer";
import { buildItems, isNearBottom, toggleSet } from "./messages";
import { useChat } from "./useChat";
import "./chat.css";

interface Props {
  term: TermRef;
  agent: Agent | null;
  disabled: boolean;
}

const ESC = new Uint8Array([0x1b]);

/** The chat view of one terminal: status line, message list, composer (+ Stop while working). */
export function ChatPanel({ term, agent, disabled }: Props) {
  const { t } = useI18n();
  const { state, echoes, addEcho, loadEarlier } = useChat(term);
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
  const [viewer, setViewer] = useState<{ src: string; alt: string } | null>(null);
  const [unseen, setUnseen] = useState(false);

  const items = useMemo(() => buildItems(state.messages, echoes), [state.messages, echoes]);
  const onToggle = useCallback((key: string) => setExpanded((s) => toggleSet(s, key)), []);
  const onOpenImage = useCallback((src: string, alt: string) => setViewer({ src, alt }), []);

  const scroller = useRef<HTMLDivElement>(null);
  const inner = useRef<HTMLDivElement>(null);
  const atBottom = useRef(true);
  const prev = useRef<{ first: string | null; last: string | null; height: number }>({ first: null, last: null, height: 0 });

  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const first = items[0]?.key ?? null;
    const last = items[items.length - 1]?.key ?? null;
    const p = prev.current;
    if (p.first !== null && first !== p.first && last === p.last) {
      // Earlier page prepended: keep the visible content where it was.
      el.scrollTop += el.scrollHeight - p.height;
    } else if (atBottom.current) {
      el.scrollTop = el.scrollHeight;
    } else if (p.last !== null && last !== p.last) {
      setUnseen(true);
    }
    prev.current = { first, last, height: el.scrollHeight };
  }, [items]);

  // Images / expanded rows change the height after render: stay at the bottom if we were there.
  useEffect(() => {
    const el = scroller.current;
    const content = inner.current;
    if (!el || !content || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(() => {
      if (atBottom.current) el.scrollTop = el.scrollHeight;
      prev.current.height = el.scrollHeight;
    });
    ro.observe(content);
    return () => ro.disconnect();
  }, []);

  const onScroll = () => {
    const el = scroller.current;
    if (!el) return;
    atBottom.current = isNearBottom(el.scrollTop, el.clientHeight, el.scrollHeight);
    if (atBottom.current) setUnseen(false);
  };

  const toBottom = () => {
    const el = scroller.current;
    if (el) el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
    setUnseen(false);
  };

  const status = state.status;
  const working = agent?.state === "Working";
  const badge = badgeFor(agent);

  let placeholder: string | null = null;
  if (status === null) placeholder = t("chat_loading");
  else if (status.state === "pending") placeholder = t("chat_pending");
  else if (status.state === "unavailable") placeholder = t("chat_unavailable");
  else if (items.length === 0) placeholder = t("chat_empty");

  return (
    <div className="chat-panel">
      {agent && (
        <div className="cm-status">
          <AgentBadge kind={badge} size={13} />
          <span className="muted small">{t(stateKey(agent))}</span>
        </div>
      )}
      <div className="cm-scroll" ref={scroller} onScroll={onScroll}>
        <div className="cm-inner" ref={inner}>
          {state.hasMore && (
            <button className="cm-earlier" disabled={state.loadingEarlier} onClick={loadEarlier}>
              {state.loadingEarlier ? t("chat_loading_earlier") : t("chat_load_earlier")}
            </button>
          )}
          {placeholder && items.length === 0 && (
            <div className="cm-placeholder muted">
              {placeholder}
              {status?.state === "unavailable" && status.reason && <div className="small">{status.reason}</div>}
            </div>
          )}
          {items.map((it) => (
            <ChatItemView key={it.key} item={it} term={term} expanded={expanded.has(it.key)} onToggle={onToggle} onOpenImage={onOpenImage} />
          ))}
        </div>
      </div>
      {unseen && (
        <button className="cm-pill" onClick={toBottom}>
          {t("chat_new_messages")}
        </button>
      )}
      <Composer
        disabled={disabled}
        onSend={(text, ids) => {
          addEcho(text);
          void api.sendText(term, text, ids, true);
        }}
        onStop={working && !disabled ? () => void api.sendInput(term, ESC) : undefined}
      />
      {viewer && <ImageViewer src={viewer.src} alt={viewer.alt} onClose={() => setViewer(null)} />}
    </div>
  );
}
