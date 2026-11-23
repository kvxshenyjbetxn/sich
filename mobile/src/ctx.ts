import { createContext, useContext } from "react";
import type { Lang, TFn } from "./lib/i18n";

export interface I18n {
  lang: Lang;
  setLang: (l: Lang) => void;
  t: TFn;
}

export const I18nContext = createContext<I18n>({
  lang: "uk",
  setLang: () => {},
  t: (k) => String(k),
});

export const useI18n = () => useContext(I18nContext);
