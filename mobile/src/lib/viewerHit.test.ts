import { describe, expect, it } from "vitest";
import { hitStrip, isTap } from "./kitty/hit";
import { initialViewerHist, onViewerPop, onViewerUnmount } from "./viewerHistory";

describe("isTap", () => {
  it("accepts short still presses", () => {
    expect(isTap({ x: 10, y: 10, t: 0 }, { x: 12, y: 11, t: 100 })).toBe(true);
  });
  it("rejects moves, long presses and negative time", () => {
    expect(isTap({ x: 0, y: 0, t: 0 }, { x: 20, y: 0, t: 100 })).toBe(false);
    expect(isTap({ x: 0, y: 0, t: 0 }, { x: 1, y: 0, t: 500 })).toBe(false);
    expect(isTap({ x: 0, y: 0, t: 10 }, { x: 0, y: 0, t: 5 })).toBe(false);
  });
});

describe("hitStrip", () => {
  const rects = [
    { image: 1, left: 0, top: 0, width: 100, height: 50 },
    { image: 2, left: 50, top: 0, width: 100, height: 50 },
  ];
  it("returns topmost hit and null outside", () => {
    expect(hitStrip(rects, 10, 10)).toBe(1);
    expect(hitStrip(rects, 60, 10)).toBe(2);
    expect(hitStrip(rects, 100, 50)).toBeNull();
    expect(hitStrip([], 1, 1)).toBeNull();
  });
});

describe("viewer history", () => {
  it("back press closes without needing history.back", () => {
    const r = onViewerPop(initialViewerHist());
    expect(r.action).toBe("close");
    expect(onViewerUnmount(r.state).needBack).toBe(false);
  });
  it("UI close drops the entry once and swallows that pop", () => {
    const u = onViewerUnmount(initialViewerHist());
    expect(u.needBack).toBe(true);
    const p = onViewerPop(u.state);
    expect(p.action).toBe("swallow");
    expect(p.state.swallowNext).toBe(false);
    expect(onViewerUnmount(u.state).needBack).toBe(false);
  });
});
