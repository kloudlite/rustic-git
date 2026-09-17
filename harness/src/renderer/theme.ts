import { createSignal, createEffect } from "solid-js";

/**
 * Theme mode. "system" is the default and sets no attribute, so the media query
 * in app.css decides; the other two stamp `data-theme` and win over it. The
 * resolved mode is handed to the main process as well, because the preview
 * window's native title bar is painted by the OS, not by this stylesheet.
 */
export type ThemeMode = "system" | "light" | "dark";

const KEY = "harness.theme";

function stored(): ThemeMode {
  // An explicit ?theme= wins over the remembered choice, so a window can be
  // opened straight into one theme (the screenshot mode uses this).
  const q = new URLSearchParams(location.search).get("theme");
  if (q === "light" || q === "dark" || q === "system") return q;
  try {
    const v = localStorage.getItem(KEY);
    if (v === "light" || v === "dark" || v === "system") return v;
  } catch {
    /* private window, cleared site data: fall through to the default */
  }
  return "system";
}

const [mode, setMode] = createSignal<ThemeMode>(stored());

createEffect(() => {
  const m = mode();
  const root = document.documentElement;
  if (m === "system") root.removeAttribute("data-theme");
  else root.setAttribute("data-theme", m);
  void window.harness?.setTheme?.(m);
});

export { mode };

/** Cycles system → light → dark → system, which is the order a person expects.
    Only a deliberate change is remembered, so opening a window with `?theme=`
    shows that theme without overwriting what the person chose. */
export function cycleTheme() {
  const next = mode() === "system" ? "light" : mode() === "light" ? "dark" : "system";
  setMode(next);
  try {
    localStorage.setItem(KEY, next);
  } catch {
    /* not being able to remember the choice is not a reason to refuse it */
  }
}

export const THEME_ICON: Record<ThemeMode, string> = { system: "monitor", light: "sun", dark: "moon" };
