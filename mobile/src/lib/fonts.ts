// Terminal font stack. Android system fonts lack many symbol glyphs that agent CLIs print,
// so symbol fallbacks are bundled (see main.tsx) and listed after the main monospace font.
// Emoji deliberately stay with the system emoji font.
/** Empty glyphs for U+10EEEE and its diacritics; FIRST in the stack so they never fall to a tofu box. */
export const PLACEHOLDER_FAMILY = "Sich Placeholder";
export const MONO_FAMILIES = ["Cascadia Mono", "JetBrains Mono"];
export const FALLBACK_FAMILIES = ["Noto Sans Symbols 2", "Noto Sans Symbols", "Noto Sans Math", "DejaVu Mono"];
export const SYSTEM_FAMILIES = ["Menlo", "monospace"];

const q = (f: string) => (f.includes(" ") ? `"${f}"` : f);

export const FONT_FAMILY = [PLACEHOLDER_FAMILY, ...MONO_FAMILIES, ...FALLBACK_FAMILIES, ...SYSTEM_FAMILIES].map(q).join(", ");

/** Bundled families that must be loaded before xterm measures cells. */
export const BUNDLED_FAMILIES = [PLACEHOLDER_FAMILY, "JetBrains Mono", ...FALLBACK_FAMILIES];

/** Load every bundled family (400, plus 700 for the primary) so measuring uses the right fonts. */
export async function loadTermFonts(px: number): Promise<void> {
  const loads: Promise<unknown>[] = [document.fonts.load(`700 ${px}px "JetBrains Mono"`)];
  for (const f of BUNDLED_FAMILIES) {
    loads.push(document.fonts.load(`${px}px "${f}"`, f === PLACEHOLDER_FAMILY ? "􎻮" : "⏵⏺⎿✻█"));
  }
  await Promise.allSettled(loads);
}
