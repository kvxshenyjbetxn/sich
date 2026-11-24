import { useSyncExternalStore } from "react";
import { getTheme, subscribeTheme, type ThemeKind } from "./theme";

export function useTheme(): ThemeKind {
  return useSyncExternalStore(subscribeTheme, getTheme, getTheme);
}
