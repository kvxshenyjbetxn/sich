/**
 * Pure state logic for the fullscreen image viewer's history entry. The viewer pushes one entry
 * when it opens; Android back pops it (-> close, screen navigation must not see the event);
 * closing from the UI drops the entry with history.back() and that pop must be swallowed too.
 */
export interface ViewerHistState {
  /** The user popped the viewer's entry (Android back). */
  popped: boolean;
  /** We called history.back() ourselves; the next pop belongs to us. */
  swallowNext: boolean;
}

export type PopAction = "close" | "swallow";

export const initialViewerHist = (): ViewerHistState => ({ popped: false, swallowNext: false });

/** A popstate arrived while the viewer owns an entry. */
export function onViewerPop(s: ViewerHistState): { state: ViewerHistState; action: PopAction } {
  if (s.swallowNext) return { state: { ...s, swallowNext: false }, action: "swallow" };
  return { state: { ...s, popped: true }, action: "close" };
}

/** The viewer is going away: do we still need to drop our history entry? */
export function onViewerUnmount(s: ViewerHistState): { state: ViewerHistState; needBack: boolean } {
  if (s.popped || s.swallowNext) return { state: s, needBack: false };
  return { state: { ...s, swallowNext: true }, needBack: true };
}
