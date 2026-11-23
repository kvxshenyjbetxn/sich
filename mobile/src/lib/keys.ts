/** Special keys of the keys row and the sticky-Ctrl mapping. All pure. */

export type KeyName = "esc" | "tab" | "shift_tab" | "up" | "down" | "left" | "right" | "ctrl_c" | "enter" | "slash";

const SEQ: Record<KeyName, string> = {
  esc: "\x1b",
  tab: "\t",
  shift_tab: "\x1b[Z",
  up: "\x1b[A",
  down: "\x1b[B",
  right: "\x1b[C",
  left: "\x1b[D",
  ctrl_c: "\x03",
  enter: "\r",
  slash: "/",
};

export function keySequence(name: KeyName): string {
  return SEQ[name];
}

/** Control code for one character, or null when it has none. */
export function ctrlChar(ch: string): string | null {
  if (ch.length !== 1) return null;
  const c = ch.charCodeAt(0);
  if (c >= 0x61 && c <= 0x7a) return String.fromCharCode(c - 0x60); // a-z
  if (c >= 0x41 && c <= 0x5a) return String.fromCharCode(c - 0x40); // A-Z
  switch (ch) {
    case " ":
    case "@":
      return "\x00";
    case "[":
      return "\x1b";
    case "\\":
      return "\x1c";
    case "]":
      return "\x1d";
    case "^":
      return "\x1e";
    case "_":
      return "\x1f";
    case "?":
      return "\x7f";
    default:
      return null;
  }
}

/**
 * Apply sticky Ctrl to input `data`. Returns the string to send and whether Ctrl was used up.
 * Single characters become control codes, plain arrows become xterm Ctrl+arrow (`ESC[1;5X`).
 * Anything else (paste, other escape sequences) passes through and keeps Ctrl armed.
 */
export function applyCtrl(data: string, ctrl: boolean): { out: string; consumed: boolean } {
  if (!ctrl) return { out: data, consumed: false };
  const mapped = ctrlChar(data);
  if (mapped !== null) return { out: mapped, consumed: true };
  const arrow = /^\x1b\[([ABCD])$/.exec(data);
  if (arrow) return { out: `\x1b[1;5${arrow[1]}`, consumed: true };
  return { out: data, consumed: false };
}

const encoder = new TextEncoder();

export function toBytes(s: string): Uint8Array {
  return encoder.encode(s);
}
