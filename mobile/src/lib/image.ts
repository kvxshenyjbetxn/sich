/** Size math and naming for image uploads (the canvas work itself lives in imageUpload.ts). */

export const MAX_SIDE = 2048;
export const JPEG_QUALITY = 0.85;

export interface Size {
  width: number;
  height: number;
}

/** Scale so the longest side is at most `max`; never upscales; keeps aspect ratio. */
export function fitWithin(width: number, height: number, max = MAX_SIDE): Size {
  if (!(width > 0) || !(height > 0)) return { width: 0, height: 0 };
  const longest = Math.max(width, height);
  if (longest <= max) return { width: Math.round(width), height: Math.round(height) };
  const k = max / longest;
  return { width: Math.max(1, Math.round(width * k)), height: Math.max(1, Math.round(height * k)) };
}

/** RGBA pixels: true when any pixel is not fully opaque. */
export function hasAlpha(rgba: Uint8ClampedArray | Uint8Array): boolean {
  for (let i = 3; i < rgba.length; i += 4) {
    if (rgba[i] < 255) return true;
  }
  return false;
}

export type OutMime = "image/jpeg" | "image/png";

/** PNG with transparency stays PNG; everything else becomes JPEG. */
export function chooseOutput(inputMime: string, alpha: boolean): OutMime {
  return inputMime === "image/png" && alpha ? "image/png" : "image/jpeg";
}

/** Replace the extension to match the output type; empty names get a default. */
export function outputName(name: string, mime: OutMime): string {
  const ext = mime === "image/png" ? "png" : "jpg";
  const base = name.replace(/\.[^./\\]*$/, "").trim() || "image";
  return `${base}.${ext}`;
}
