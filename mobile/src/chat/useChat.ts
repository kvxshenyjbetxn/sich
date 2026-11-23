import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { CHAT_PAGE, chatApi } from "./chatApi";
import { INITIAL_CHAT, earliestOrder, expireEchoes, lastUserOrder, pruneEchoes, reduceChat, type ChatState, type Echo } from "./messages";
import type { TermRef } from "../lib/types";

/** Chat subscription of one terminal plus optimistic echoes of text sent from the phone. */
export function useChat(term: TermRef) {
  const [state, setState] = useState<ChatState>(INITIAL_CHAT);
  const [echoes, setEchoes] = useState<Echo[]>([]);
  const nextKey = useRef(1);
  const stateRef = useRef(state);
  stateRef.current = state;

  useEffect(() => {
    let alive = true;
    setState(INITIAL_CHAT);
    setEchoes([]);
    void chatApi
      .subscribe(term, CHAT_PAGE, (ev) => {
        if (!alive) return;
        setState((s) => reduceChat(s, ev));
      })
      .catch((e) => console.error("chat subscribe failed", e));
    return () => {
      alive = false;
      void chatApi.unsubscribe(term);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [term.daemon, term.id]);

  // A user message that matches an echo replaces it.
  useEffect(() => {
    setEchoes((cur) => pruneEchoes(cur, state.messages));
  }, [state.messages]);

  // Echoes that never matched (e.g. the agent ignored them) do not stay forever.
  useEffect(() => {
    if (echoes.length === 0) return;
    const id = window.setInterval(() => setEchoes((cur) => expireEchoes(cur, Date.now())), 5000);
    return () => window.clearInterval(id);
  }, [echoes.length]);

  const addEcho = useCallback((text: string) => {
    if (!text.trim()) return;
    const baseOrder = lastUserOrder(stateRef.current.messages);
    setEchoes((cur) => [...cur, { key: nextKey.current++, text, baseOrder, sentAt: Date.now() }]);
  }, []);

  const loadEarlier = useCallback(() => {
    const s = stateRef.current;
    const before = earliestOrder(s.messages);
    if (before === null || s.loadingEarlier || !s.hasMore) return;
    setState((c) => ({ ...c, loadingEarlier: true }));
    void chatApi.loadEarlier(term, before, CHAT_PAGE).catch(() => setState((c) => ({ ...c, loadingEarlier: false })));
  }, [term]);

  return useMemo(() => ({ state, echoes, addEcho, loadEarlier }), [state, echoes, addEcho, loadEarlier]);
}
