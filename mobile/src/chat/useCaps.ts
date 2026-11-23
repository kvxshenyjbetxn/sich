import { useEffect, useState } from "react";
import { chatApi } from "./chatApi";
import type { ConnState } from "../lib/types";

/** Capabilities the gateway advertised; refreshed whenever the connection (re)establishes. */
export function useGatewayCaps(conn: ConnState): string[] {
  const [caps, setCaps] = useState<string[]>([]);
  const connected = conn.state === "connected";
  useEffect(() => {
    if (!connected) return;
    let alive = true;
    chatApi.capabilities().then(
      (c) => alive && setCaps(c),
      () => alive && setCaps([]),
    );
    return () => {
      alive = false;
    };
  }, [connected]);
  return caps;
}
