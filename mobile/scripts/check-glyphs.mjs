// Verifies the bundled terminal fonts cover the glyph ranges the TUIs print.
// Parses the cmap of the .woff files shipped by @fontsource (no deps). Run: node scripts/check-glyphs.mjs
import { readFileSync } from "node:fs";
import { inflateSync } from "node:zlib";

const dir = new URL("../node_modules/@fontsource/", import.meta.url);
const FONTS = [
  "jetbrains-mono/files/jetbrains-mono-latin-400-normal.woff",
  "dejavu-mono/files/dejavu-mono-latin-400-normal.woff",
];

function cmapOf(buf) {
  const n = buf.readUInt16BE(12);
  for (let i = 0; i < n; i++) {
    const o = 44 + i * 20;
    if (buf.toString("latin1", o, o + 4) !== "cmap") continue;
    const off = buf.readUInt32BE(o + 4), cl = buf.readUInt32BE(o + 8), ol = buf.readUInt32BE(o + 12);
    return cl < ol ? inflateSync(buf.subarray(off, off + cl)) : buf.subarray(off, off + ol);
  }
  throw new Error("no cmap");
}

function coverage(cmap) {
  const set = new Set();
  const n = cmap.readUInt16BE(2);
  for (let i = 0; i < n; i++) {
    const t = cmap.readUInt32BE(4 + i * 8 + 4);
    const fmt = cmap.readUInt16BE(t);
    if (fmt === 4) {
      const segX2 = cmap.readUInt16BE(t + 6);
      const endO = t + 14, startO = endO + segX2 + 2, deltaO = startO + segX2, rangeO = deltaO + segX2;
      for (let s = 0; s < segX2 / 2; s++) {
        const end = cmap.readUInt16BE(endO + s * 2), start = cmap.readUInt16BE(startO + s * 2);
        const ro = cmap.readUInt16BE(rangeO + s * 2), delta = cmap.readInt16BE(deltaO + s * 2);
        for (let c = start; c <= end && c !== 0xffff; c++) {
          let g;
          if (ro === 0) g = (c + delta) & 0xffff;
          else {
            g = cmap.readUInt16BE(rangeO + s * 2 + ro + (c - start) * 2);
            if (g) g = (g + delta) & 0xffff;
          }
          if (g) set.add(c);
        }
      }
    } else if (fmt === 12) {
      const groups = cmap.readUInt32BE(t + 12);
      for (let g = 0; g < groups; g++) {
        const o = t + 16 + g * 12;
        const a = cmap.readUInt32BE(o), b = cmap.readUInt32BE(o + 4);
        for (let c = a; c <= b; c++) set.add(c);
      }
    }
  }
  return set;
}

const cov = new Set();
for (const f of FONTS) {
  for (const c of coverage(cmapOf(readFileSync(new URL(f, dir))))) cov.add(c);
}

const REQUIRED = [
  ["Block elements", 0x2580, 0x259f],
  ["Box drawing", 0x2500, 0x257f],
];
let bad = 0;
for (const [name, a, b] of REQUIRED) {
  const missing = [];
  for (let c = a; c <= b; c++) if (!cov.has(c)) missing.push(c.toString(16));
  if (missing.length) {
    bad++;
    console.error(`${name}: missing ${missing.join(" ")}`);
  } else console.log(`${name}: ok`);
}
process.exit(bad ? 1 : 0);
