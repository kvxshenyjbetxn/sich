// Typed wrappers over the chat commands of the Rust core (src-tauri/src/commands.rs).
import { Channel, invoke } from "@tauri-apps/api/core";
import { FetchQueue } from "./fetchQueue";
import type { ChatEvent } from "./types";
import { termKey, type TermRef } from "../lib/types";

export const CHAT_PAGE = 60;

export const chatApi = {
  capabilities: () => invoke<string[]>("gateway_capabilities"),
  subscribe(term: TermRef, limit: number, onEvent: (e: ChatEvent) => void) {
    const ch = new Channel<ChatEvent>();
    ch.onmessage = onEvent;
    return invoke<void>("chat_subscribe", { term, limit, onEvent: ch });
  },
  unsubscribe: (term: TermRef) => invoke<void>("chat_unsubscribe", { term }),
  loadEarlier: (term: TermRef, beforeOrder: number, limit: number) =>
    invoke<void>("chat_load_earlier", { term, beforeOrder, limit }),
  image: (term: TermRef, imageRef: string) => invoke<string>("chat_image", { term, imageRef }),
};

/** At most 2 image requests at a time; results are cached as data URLs. */
export const imageQueue = new FetchQueue<string>(2, 60);

export function fetchChatImage(term: TermRef, imageRef: string): Promise<string> {
  return imageQueue.load(`${termKey(term)}|${imageRef}`, () => chatApi.image(term, imageRef));
}

const MODE_PREFIX = "sich.chatMode.";

export type ViewMode = "chat" | "terminal";

export function loadMode(term: TermRef): ViewMode {
  try {
    return localStorage.getItem(MODE_PREFIX + termKey(term)) === "chat" ? "chat" : "terminal";
  } catch {
    return "terminal";
  }
}

export function saveMode(term: TermRef, mode: ViewMode) {
  try {
    localStorage.setItem(MODE_PREFIX + termKey(term), mode);
  } catch {
    /* storage unavailable */
  }
}

/** Chat is offered for Claude and Codex terminals only. */
export function chatSupported(kind: string | null | undefined, caps: string[]): boolean {
  return (kind === "Claude" || kind === "Codex") && caps.includes("chat");
}
