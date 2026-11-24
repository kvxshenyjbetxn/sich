import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/700.css";
// Block elements (U+2580-259F) are missing from the other bundled fonts.
import "@fontsource/dejavu-mono/400.css";
import "@fontsource/noto-sans-symbols-2/symbols-400.css";
import "@fontsource/noto-sans-symbols-2/math-400.css";
import "@fontsource/noto-sans-symbols-2/braille-400.css";
import "@fontsource/noto-sans-symbols/symbols-400.css";
import React from "react";
import ReactDOM from "react-dom/client";
import { App } from "./App";
import "./styles.css";
import { initTheme } from "./lib/theme";

initTheme();

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
