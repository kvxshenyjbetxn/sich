// Build-time check (no network): every glyph in src/lib/termGlyphs.json must be covered by a
// bundled font file (honouring each @font-face unicode-range). Prints missing glyphs; exit 1 if any.
import fs from "node:fs";
import path from "node:path";

import * as fontkit from "fontkit";

const root = path.resolve(import.meta.dirname, "..");
const main = fs.readFileSync(path.join(root, "src/main.tsx"), "utf8");
const cssFiles = [...main.matchAll(/import "(@fontsource\/[^"]+\.css)"/g)].map((m) => path.join(root, "node_modules", m[1]));
cssFiles.push(path.join(root, "src/styles.css"));
const glyphs = JSON.parse(fs.readFileSync(path.join(root, "src/lib/termGlyphs.json"), "utf8"));

const inRange = (cp, range) =>
  !range ||
  range.split(",").some((r) => {
    const [a, b] = r.trim().replace("U+", "").split("-");
    const lo = parseInt(a, 16), hi = b ? parseInt(b, 16) : lo;
    return cp >= lo && cp <= hi;
  });

const faces = [];
for (const css of cssFiles) {
  const file = css;
  const text = fs.readFileSync(file, "utf8");
  for (const block of text.match(/@font-face\s*{[^}]*}/g) ?? []) {
    const url = block.match(/url\(["']?([^)"']+\.woff2)["']?\)/)?.[1];
    if (!url) continue;
    const range = block.match(/unicode-range:\s*([^;]+);/)?.[1];
    const font = fontkit.openSync(path.resolve(path.dirname(file), url));
    faces.push({ name: path.basename(url), range, font });
  }
}
// The Kitty placeholder must be covered by the bundled empty-glyph font (no tofu box).
{
  const ph = fontkit.openSync(path.join(root, "src/assets/sich-placeholder.ttf"));
  const g = ph.hasGlyphForCodePoint(0x10eeee) ? ph.glyphForCodePoint(0x10eeee) : null;
  const css = fs.readFileSync(path.join(root, "src/styles.css"), "utf8");
  const ok = g && g.path.commands.length === 0 && g.advanceWidth === 600 && ph.unitsPerEm === 1000 && /Sich Placeholder[^}]*U[+]10EEEE/.test(css);
  console.log(`U+10EEEE sich-placeholder.ttf ${ok ? "empty glyph" : "BAD"}`);
  if (!ok) {
    console.log("Placeholder font must cover U+10EEEE with an empty glyph (advance 600/1000)");
    process.exit(1);
  }
}
const missing = [];
for (const g of glyphs) {
  const cp = g.codePointAt(0);
  const hit = faces.find((f) => inRange(cp, f.range) && f.font.hasGlyphForCodePoint(cp));
  console.log(`${g} U+${cp.toString(16).toUpperCase().padStart(4, "0")} ${hit ? hit.name : "MISSING"}`);
  if (!hit) missing.push(g);
}
console.log(missing.length ? `Missing: ${missing.join(" ")}` : "All glyphs covered.");
process.exit(missing.length ? 1 : 0);
