import { useEffect, useRef, useState } from "react";
import { ArrowLeft, Link2, Monitor, Plus, QrCode, Trash2 } from "lucide-react";
import { Format, cancel, checkPermissions, requestPermissions, scan } from "@tauri-apps/plugin-barcode-scanner";
import { api, type HostsList } from "../api";
import { useI18n } from "../ctx";
import { rejectKey } from "../lib/connLabels";
import type { ConnState } from "../lib/types";

type Mode = "list" | "add" | "link" | "scanning" | "pairing";

interface Props {
  hosts: HostsList;
  conn: ConnState;
  /** Called when there is somewhere to go back to (a paired host exists). */
  onBack: (() => void) | null;
  onPaired: () => void;
  onChanged: () => void;
}

export function HostsScreen({ hosts, conn, onBack, onPaired, onChanged }: Props) {
  const { t } = useI18n();
  const [mode, setMode] = useState<Mode>(hosts.hosts.length === 0 ? "add" : "list");
  const [link, setLink] = useState("");
  const [error, setError] = useState<string | null>(null);
  const sawProgress = useRef(false);

  // Follow the pairing progress reported by the core.
  useEffect(() => {
    if (mode !== "pairing") return;
    if (conn.state === "connecting" || conn.state === "pair_pending" || conn.state === "rejected") sawProgress.current = true;
    if (conn.state === "connected" && sawProgress.current) {
      onChanged();
      const id = window.setTimeout(onPaired, 900);
      return () => window.clearTimeout(id);
    }
  }, [conn, mode, onPaired, onChanged]);

  const startPair = async (url: string) => {
    setError(null);
    sawProgress.current = false;
    try {
      await api.pairStart(url);
      setMode("pairing");
    } catch {
      setError(t("bad_link"));
      setMode("link");
    }
  };

  const doScan = async () => {
    setError(null);
    try {
      let perm = await checkPermissions();
      if (perm !== "granted") perm = await requestPermissions();
      if (perm !== "granted") {
        setError(t("camera_denied"));
        return;
      }
      setMode("scanning");
      document.documentElement.classList.add("scanning");
      const res = await scan({ windowed: true, formats: [Format.QRCode] });
      document.documentElement.classList.remove("scanning");
      await startPair(res.content);
    } catch {
      document.documentElement.classList.remove("scanning");
      setMode((m) => (m === "scanning" ? "add" : m));
      setError((e) => e ?? t("scan_failed"));
    }
  };

  const cancelScan = async () => {
    document.documentElement.classList.remove("scanning");
    try {
      await cancel();
    } catch {
      /* not scanning */
    }
    setMode("add");
  };

  if (mode === "scanning") {
    return (
      <div className="screen scan-screen">
        <div className="scan-top">{t("scanning")}</div>
        <button className="btn scan-cancel" onClick={() => void cancelScan()}>
          {t("cancel")}
        </button>
      </div>
    );
  }

  if (mode === "pairing") {
    return (
      <div className="screen centered">
        <PairProgress conn={conn} />
        {conn.state === "rejected" && (
          <button className="btn" onClick={() => setMode("add")}>
            {t("retry")}
          </button>
        )}
      </div>
    );
  }

  const canLeave = onBack && hosts.hosts.length > 0;

  return (
    <div className="screen hosts-screen">
      <header className="topbar">
        {canLeave && (
          <button className="icon-btn" onClick={onBack} aria-label={t("back")}>
            <ArrowLeft size={18} />
          </button>
        )}
        <span className="title grow">{t("hosts_title")}</span>
        {mode === "list" && (
          <button className="icon-btn" onClick={() => setMode("add")} aria-label={t("add_pc")}>
            <Plus size={20} />
          </button>
        )}
      </header>

      {mode === "list" && (
        <div className="list">
          {hosts.hosts.map((h) => (
            <div key={h.id} className="row host-row">
              <Monitor size={18} />
              <div className="term-text grow">
                <span className="ellipsis">{h.name}</span>
                {hosts.active === h.id && <span className="muted small">{t("active_host")}</span>}
              </div>
              <button
                className="btn small-btn"
                onClick={async () => {
                  await api.hostConnect(h.id);
                  onChanged();
                  onPaired();
                }}
              >
                {t("open")}
              </button>
              <button
                className="icon-btn"
                aria-label={t("forget")}
                onClick={async () => {
                  if (window.confirm(t("forget_confirm"))) {
                    await api.hostForget(h.id);
                    onChanged();
                  }
                }}
              >
                <Trash2 size={16} />
              </button>
            </div>
          ))}
        </div>
      )}

      {(mode === "add" || mode === "link") && (
        <div className="add-box">
          {hosts.hosts.length === 0 && <p className="muted">{t("hosts_empty")}</p>}
          <button className="btn primary wide" onClick={() => void doScan()}>
            <QrCode size={18} /> {t("scan_qr")}
          </button>
          <div className="link-box">
            <label className="muted small" htmlFor="pair-link">
              <Link2 size={13} /> {t("paste_link")}
            </label>
            <input
              id="pair-link"
              value={link}
              onChange={(e) => setLink(e.target.value)}
              placeholder={t("link_placeholder")}
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
            />
            <button className="btn" disabled={!link.trim()} onClick={() => void startPair(link.trim())}>
              {t("connect")}
            </button>
          </div>
          {error && <p className="error">{error}</p>}
          {hosts.hosts.length > 0 && (
            <button className="btn" onClick={() => setMode("list")}>
              {t("cancel")}
            </button>
          )}
        </div>
      )}
    </div>
  );
}

function PairProgress({ conn }: { conn: ConnState }) {
  const { t } = useI18n();
  switch (conn.state) {
    case "pair_pending":
      return (
        <>
          <h2>{t("pairing_confirm")}</h2>
          <p className="muted center">{t("pairing_confirm_hint")}</p>
        </>
      );
    case "connected":
      return <h2>{t("pairing_done")}</h2>;
    case "rejected":
      return (
        <>
          <h2 className="error">{t("conn_idle")}</h2>
          <p className="center">{t(rejectKey(conn.reason))}</p>
        </>
      );
    default:
      return <h2>{t("pairing_connecting")}</h2>;
  }
}
