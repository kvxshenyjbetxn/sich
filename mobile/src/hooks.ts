import { useCallback, useEffect, useRef, useState } from "react";
import { api, events, type HostsList } from "./api";
import { loadLang, saveLang, translate, type Key, type Lang, type TFn } from "./lib/i18n";
import { loadFontSize, saveFontSize } from "./lib/termLogic";
import type { ConnState, TermRef, Workspace } from "./lib/types";

/** Connection state, workspace snapshot and paired hosts, kept in sync with the Rust core. */
export function useCore() {
  const [conn, setConn] = useState<ConnState>({ state: "idle" });
  const [workspace, setWorkspace] = useState<Workspace | null>(null);
  const [hosts, setHosts] = useState<HostsList>({ hosts: [], active: null });
  const [hostsLoaded, setHostsLoaded] = useState(false);
  const [lastError, setLastError] = useState<string | null>(null);

  const reloadHosts = useCallback(async () => {
    try {
      setHosts(await api.hostsList());
    } finally {
      setHostsLoaded(true);
    }
  }, []);

  useEffect(() => {
    let alive = true;
    const unlisten: Array<() => void> = [];
    const add = (p: Promise<() => void>) => p.then((u) => (alive ? unlisten.push(u) : u()));
    add(events.connection((s) => setConn(s)));
    add(events.workspace((w) => setWorkspace(w)));
    add(events.hosts(() => void reloadHosts()));
    add(events.gatewayError((m) => setLastError(m)));
    // Pick up whatever happened before the listeners were attached.
    void api.connectionState().then((s) => alive && setConn(s));
    void api.workspaceSnapshot().then((w) => alive && w && setWorkspace(w));
    void reloadHosts();
    return () => {
      alive = false;
      unlisten.forEach((u) => u());
    };
  }, [reloadHosts]);

  // A disconnected host keeps showing its last workspace, so lists do not flash empty on reconnect.
  return { conn, workspace, hosts, hostsLoaded, reloadHosts, lastError, clearError: () => setLastError(null) };
}

export function useLang() {
  const [lang, setLangState] = useState<Lang>(loadLang);
  const setLang = useCallback((l: Lang) => {
    saveLang(l);
    setLangState(l);
    document.documentElement.lang = l;
  }, []);
  useEffect(() => {
    document.documentElement.lang = lang;
  }, [lang]);
  const t: TFn = useCallback((key: Key, params) => translate(lang, key, params), [lang]);
  return { lang, setLang, t };
}

export function useFontSize() {
  const [size, setSize] = useState<number>(loadFontSize);
  const set = useCallback((px: number) => {
    saveFontSize(px);
    setSize(loadFontSize());
  }, []);
  return [size, set] as const;
}

export function useViewport() {
  const [w, setW] = useState(window.innerWidth);
  useEffect(() => {
    const onResize = () => {
      setW(window.innerWidth);
      const vv = window.visualViewport;
      document.documentElement.style.setProperty("--app-h", `${vv ? vv.height : window.innerHeight}px`);
    };
    onResize();
    window.addEventListener("resize", onResize);
    window.visualViewport?.addEventListener("resize", onResize);
    return () => {
      window.removeEventListener("resize", onResize);
      window.visualViewport?.removeEventListener("resize", onResize);
    };
  }, []);
  return w;
}

export type View = "main" | "hosts";
export interface Nav {
  view: View;
  term: TermRef | null;
}

/**
 * Navigation on top of the History API: the Android back button / gesture pops the entry and the
 * app goes back from terminal to workspace (and from hosts to workspace) instead of closing.
 */
export function useNav() {
  const [nav, setNav] = useState<Nav>({ view: "main", term: null });
  const navRef = useRef(nav);
  navRef.current = nav;

  useEffect(() => {
    history.replaceState({ view: "main", term: null } satisfies Nav, "");
    const onPop = (e: PopStateEvent) => {
      const s = e.state as Nav | null;
      setNav(s && typeof s === "object" && "view" in s ? s : { view: "main", term: null });
    };
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);

  const push = useCallback((next: Nav) => {
    history.pushState(next, "");
    setNav(next);
  }, []);
  /** Replace the current entry (e.g. switching terminals in two-pane mode). */
  const replace = useCallback((next: Nav) => {
    history.replaceState(next, "");
    setNav(next);
  }, []);
  const back = useCallback(() => history.back(), []);

  return { nav, push, replace, back };
}
