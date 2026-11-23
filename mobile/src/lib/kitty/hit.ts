/** Tap detection and hit-testing of drawn Kitty strips (the overlay itself is pointer-events: none). */

export interface StripRect {
  image: number;
  left: number;
  top: number;
  width: number;
  height: number;
}

export interface PointerSample {
  x: number;
  y: number;
  t: number;
}

export const TAP_MAX_MOVE = 10;
export const TAP_MAX_MS = 500;

/** A short press with little movement. */
export function isTap(down: PointerSample, up: PointerSample, maxMove = TAP_MAX_MOVE, maxMs = TAP_MAX_MS): boolean {
  const ms = up.t - down.t;
  if (ms < 0 || ms >= maxMs) return false;
  return Math.hypot(up.x - down.x, up.y - down.y) < maxMove;
}

/** Image id of the topmost strip containing (x, y) (rect coordinates), or null. */
export function hitStrip(rects: readonly StripRect[], x: number, y: number): number | null {
  for (let i = rects.length - 1; i >= 0; i--) {
    const r = rects[i];
    if (x >= r.left && x < r.left + r.width && y >= r.top && y < r.top + r.height) return r.image;
  }
  return null;
}
