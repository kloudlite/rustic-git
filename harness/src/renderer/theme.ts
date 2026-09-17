import { createSignal, createEffect } from "solid-js";
import { applyTheme, setColorScheme } from "@opencode-ai/ui/theme/loader";
import oneDark from "@opencode-ai/ui/theme/themes/one-dark.json";

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

/**
 * One Dark, through opencode's own loader (spec §23). Their theme JSON carries BOTH variants —
 * there is no `one-light.json` upstream — and `applyTheme` writes the resolved tokens into a
 * `<style id="opencode-theme">` and stamps `data-theme`. Our own `app.css` tokens keep the sidebar,
 * inspector and terminal; inside the pane their tokens are what the port reads.
 */
applyTheme(oneDark as never, "one-dark");

createEffect(() => {
  const m = mode();
  const root = document.documentElement;
  // `data-theme` is their loader's now; ours is the colour scheme beside it, which is what our
  // own stylesheet's light/dark blocks key off.
  root.setAttribute("data-scheme", m === "system" ? "" : m);
  if (m === "system") root.removeAttribute("data-scheme");
  setColorScheme(m === "system" ? "auto" : m);
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
