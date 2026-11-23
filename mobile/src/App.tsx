import { useCallback, useMemo, useState } from "react";
import { Settings } from "./components/Settings";
import { I18nContext } from "./ctx";
import { useCore, useFontSize, useLang, useNav, useViewport } from "./hooks";
import { HostsScreen } from "./screens/HostsScreen";
import { TerminalScreen } from "./screens/TerminalScreen";
import { WorkspaceScreen } from "./screens/WorkspaceScreen";
import { sameTerm, type TermRef } from "./lib/types";

/** Width from which the desktop-like two-pane layout is used. */
const WIDE_PX = 900;

export function App() {
  const i18n = useLang();
  const { t } = i18n;
  const core = useCore();
  const { nav, push, replace, back } = useNav();
  const width = useViewport();
  const [font, setFont] = useFontSize();
  const [settingsOpen, setSettingsOpen] = useState(false);
  const wide = width >= WIDE_PX;

  const hostName = core.workspace?.host_name || core.hosts.hosts.find((h) => h.id === core.hosts.active)?.name || t("app_name");

  const openTerm = useCallback(
    (term: TermRef) => {
      const next = { view: "main" as const, term };
      if (wide || nav.term) replace(next);
      else push(next);
    },
    [wide, nav.term, push, replace],
  );

  // First launch (nothing paired yet): go straight to pairing.
  const mustPair = core.hostsLoaded && core.hosts.hosts.length === 0;
  const info = useMemo(
    () => (nav.term && core.workspace ? (core.workspace.terminals.find((x) => sameTerm(x.term, nav.term)) ?? null) : null),
    [nav.term, core.workspace],
  );

  if (!core.hostsLoaded) return <I18nContext.Provider value={i18n}><div className="screen centered" /></I18nContext.Provider>;

  let body;
  if (nav.view === "hosts" || mustPair) {
    body = (
      <HostsScreen
        hosts={core.hosts}
        conn={core.conn}
        onBack={mustPair ? null : back}
        onPaired={() => replace({ view: "main", term: null })}
        onChanged={() => void core.reloadHosts()}
      />
    );
  } else {
    const list = (
      <WorkspaceScreen
        workspace={core.workspace}
        conn={core.conn}
        hostName={hostName}
        selected={nav.term}
        onOpen={openTerm}
        onHosts={() => push({ view: "hosts", term: null })}
        onSettings={() => setSettingsOpen(true)}
      />
    );
    const termView = nav.term ? (
      <TerminalScreen
        term={nav.term}
        info={info}
        workspaceLoaded={core.workspace !== null}
        conn={core.conn}
        baseFont={font}
        onFont={setFont}
        onBack={wide ? null : back}
      />
    ) : (
      <div className="screen centered muted">{t("select_terminal")}</div>
    );
    body = wide ? (
      <div className="two-pane">
        <div className="pane-side">{list}</div>
        <div className="pane-main">{termView}</div>
      </div>
    ) : nav.term ? (
      termView
    ) : (
      list
    );
  }

  return (
    <I18nContext.Provider value={i18n}>
      <div className="app">
        {body}
        {settingsOpen && <Settings fontSize={font} onFont={setFont} onClose={() => setSettingsOpen(false)} />}
      </div>
    </I18nContext.Provider>
  );
}
