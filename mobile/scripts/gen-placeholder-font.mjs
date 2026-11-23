// Generates src/assets/sich-placeholder.ttf: a font whose only glyphs are EMPTY (no contours) for the
// Kitty placeholder U+10EEEE (advance = mono cell, 600/1000 em) and the row/column diacritics
// (advance 0), so they render blank instead of tofu. Metrics match JetBrains Mono.
// Run: node scripts/gen-placeholder-font.mjs  (output is committed).
import fs from "node:fs";
import path from "node:path";
import opentype from "opentype.js";

const root = path.resolve(import.meta.dirname, "..");
const src = fs.readFileSync(path.join(root, "src/lib/kitty/diacritics.ts"), "utf8");
const marks = [...src.matchAll(/0x([0-9A-Fa-f]{4,6})/g)].map((m) => parseInt(m[1], 16));

const empty = () => new opentype.Path();
const glyphs = [new opentype.Glyph({ name: ".notdef", unicode: undefined, advanceWidth: 600, path: empty() })];
glyphs.push(new opentype.Glyph({ name: "placeholder", unicode: 0x10eeee, advanceWidth: 600, path: empty() }));
for (const cp of marks) {
  glyphs.push(new opentype.Glyph({ name: `m${cp.toString(16)}`, unicode: cp, advanceWidth: 0, path: empty() }));
}
const font = new opentype.Font({
  familyName: "Sich Placeholder",
  styleName: "Regular",
  unitsPerEm: 1000,
  ascender: 1020,
  descender: -300,
  glyphs,
});
const out = path.join(root, "src/assets/sich-placeholder.ttf");
fs.mkdirSync(path.dirname(out), { recursive: true });
fs.writeFileSync(out, Buffer.from(font.toArrayBuffer()));
console.log(`wrote ${out} (${marks.length} marks)`);
