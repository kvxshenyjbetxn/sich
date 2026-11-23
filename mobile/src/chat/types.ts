// Shape of the chat JSON emitted by the Rust core (src-tauri/src/chat.rs).

export type ChatRole = "user" | "assistant" | "tool" | "reasoning" | "system";

export type ChatBlock =
  | { type: "text"; markdown: string }
  | { type: "tool_call"; call_id: string; name: string; summary: string }
  | { type: "tool_result"; call_id: string; output: string; is_error: boolean }
  | { type: "image"; image_ref: string; alt: string };

export interface ChatMessage {
  id: string;
  /** Byte offset of the transcript line; the sort key. */
  order: number;
  role: ChatRole;
  timestamp_ms: number | null;
  blocks: ChatBlock[];
}

export type ChatStatus = { state: "ready" } | { state: "pending" } | { state: "unavailable"; reason: string };

export type ChatEvent =
  | { kind: "snapshot"; status: ChatStatus; messages: ChatMessage[]; has_more: boolean }
  | { kind: "update"; messages: ChatMessage[] }
  | { kind: "earlier"; messages: ChatMessage[]; has_more: boolean };
