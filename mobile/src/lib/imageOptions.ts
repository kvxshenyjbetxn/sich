// Options for @xterm/addon-image. The addon draws on its own overlay canvas, so it works with the
// default DOM renderer (no canvas/webgl addon needed). The desktop gateway turns Kitty graphics
// into iTerm2 inline images (OSC 1337), so IIP must be on; Sixel is enabled as well.
export interface ImageOptions {
  enableSizeReports: boolean;
  pixelLimit: number;
  storageLimit: number;
  showPlaceholder: boolean;
  sixelSupport: boolean;
  sixelScrolling: boolean;
  sixelSizeLimit: number;
  iipSupport: boolean;
  iipSizeLimit: number;
}

export function imageAddonOptions(): ImageOptions {
  return {
    enableSizeReports: true,
    pixelLimit: 16_777_216, // 16M pixels per image
    storageLimit: 64, // MB
    showPlaceholder: true,
    sixelSupport: true,
    sixelScrolling: true,
    sixelSizeLimit: 25_000_000,
    iipSupport: true,
    iipSizeLimit: 20_000_000,
  };
}
