// Thin typed wrappers over the Rust core (src-tauri/src/commands.rs). No logic here.
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { parseFrame, type TermFrame } from "./lib/termLogic";
import type { ConnState, Host, TermRef, Workspace } from "./lib/types";

export interface HostsList {
  hosts: Host[];
  active: string | null;
}

export const api = {
  hostsList: () => invoke<HostsList>("hosts_list"),
  hostForget: (id: string) => invoke<void>("host_forget", { id }),
  pairStart: (url: string) => invoke<void>("pair_start", { url }),
  hostConnect: (id: string) => invoke<void>("host_connect", { id }),
  retryNow: () => invoke<void>("retry_now"),
  connectionState: () => invoke<ConnState>("connection_state"),
  workspaceSnapshot: () => invoke<Workspace | null>("workspace_snapshot"),

  /** Terminal data arrives as binary frames; `onFrame` gets them parsed. */
  subscribe(term: TermRef, cols: number, rows: number, fit: boolean, onFrame: (f: TermFrame) => void) {
    const ch = new Channel<ArrayBuffer>();
    ch.onmessage = (data) => {
      const f = parseFrame(data);
      if (f) onFrame(f);
    };
    return invoke<void>("subscribe", { term, cols, rows, fit, onOutput: ch });
  },
  unsubscribe: (term: TermRef) => invoke<void>("unsubscribe", { term }),
  sendInput: (term: TermRef, bytes: Uint8Array) => invoke<void>("send_input", { term, bytes: Array.from(bytes) }),
  sendText: (term: TermRef, text: string, imageIds: string[], submit: boolean) =>
    invoke<void>("send_text", { term, text, imageIds, submit }),
  resize: (term: TermRef, cols: number, rows: number) => invoke<void>("resize", { term, cols, rows }),
  setFit: (term: TermRef, fit: boolean) => invoke<void>("set_fit", { term, fit }),
  markSeen: (term: TermRef) => invoke<void>("mark_seen", { term }),

  /** Raw body = image bytes; the name travels percent-encoded in a header. Resolves to the upload id. */
  uploadImage: (fileName: string, bytes: Uint8Array) =>
    invoke<string>("upload_image", bytes, { headers: { "x-file-name": encodeURIComponent(fileName) } }),
};

export interface TermSizeEvent {
  term: TermRef;
  cols: number;
  rows: number;
  fit: boolean;
}

export interface TermExitedEvent {
  term: TermRef;
  code: number | null;
}

export const events = {
  connection: (cb: (s: ConnState) => void): Promise<UnlistenFn> => listen<ConnState>("connection", (e) => cb(e.payload)),
  workspace: (cb: (w: Workspace) => void): Promise<UnlistenFn> => listen<Workspace>("workspace", (e) => cb(e.payload)),
  termSize: (cb: (e: TermSizeEvent) => void): Promise<UnlistenFn> => listen<TermSizeEvent>("term_size", (e) => cb(e.payload)),
  termExited: (cb: (e: TermExitedEvent) => void): Promise<UnlistenFn> =>
    listen<TermExitedEvent>("term_exited", (e) => cb(e.payload)),
  hosts: (cb: () => void): Promise<UnlistenFn> => listen("hosts", () => cb()),
  gatewayError: (cb: (m: string) => void): Promise<UnlistenFn> => listen<string>("gateway_error", (e) => cb(e.payload)),
};
