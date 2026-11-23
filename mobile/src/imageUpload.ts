// Browser-side image preparation (canvas). Size math and naming are in lib/image.ts (tested).
import { JPEG_QUALITY, chooseOutput, fitWithin, hasAlpha, outputName } from "./lib/image";

export interface Prepared {
  blob: Blob;
  name: string;
}

async function decode(file: File): Promise<{ source: CanvasImageSource; width: number; height: number; close?: () => void }> {
  if (typeof createImageBitmap === "function") {
    try {
      const bmp = await createImageBitmap(file);
      return { source: bmp, width: bmp.width, height: bmp.height, close: () => bmp.close() };
    } catch {
      /* fall through to <img> */
    }
  }
  const url = URL.createObjectURL(file);
  try {
    const img = new Image();
    img.src = url;
    await img.decode();
    return { source: img, width: img.naturalWidth, height: img.naturalHeight };
  } finally {
    URL.revokeObjectURL(url);
  }
}

function toBlob(canvas: HTMLCanvasElement, mime: string, quality?: number): Promise<Blob> {
  return new Promise((resolve, reject) =>
    canvas.toBlob((b) => (b ? resolve(b) : reject(new Error("encode failed"))), mime, quality),
  );
}

/** Downscale to <= 2048 px on the longest side; JPEG q0.85 unless the image is a PNG with alpha. */
export async function prepareImage(file: File): Promise<Prepared> {
  const img = await decode(file);
  try {
    const { width, height } = fitWithin(img.width, img.height);
    if (width === 0 || height === 0) throw new Error("empty image");
    const canvas = document.createElement("canvas");
    canvas.width = width;
    canvas.height = height;
    const ctx = canvas.getContext("2d", { willReadFrequently: true });
    if (!ctx) throw new Error("no canvas");
    ctx.drawImage(img.source, 0, 0, width, height);
    const alpha = file.type === "image/png" ? hasAlpha(ctx.getImageData(0, 0, width, height).data) : false;
    const mime = chooseOutput(file.type, alpha);
    if (mime === "image/jpeg") {
      // JPEG has no alpha: flatten onto white so transparent areas do not turn black.
      ctx.globalCompositeOperation = "destination-over";
      ctx.fillStyle = "#ffffff";
      ctx.fillRect(0, 0, width, height);
    }
    const blob = await toBlob(canvas, mime, mime === "image/jpeg" ? JPEG_QUALITY : undefined);
    return { blob, name: outputName(file.name, mime) };
  } finally {
    img.close?.();
  }
}
