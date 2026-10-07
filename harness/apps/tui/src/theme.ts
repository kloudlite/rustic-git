/** Semantic palette. Theme model follows opencode's. */
type Palette = {
  border: string;
  bg: string;
  /** Default text color (opentui has no "terminal default fg" — pure white otherwise). */
  fg: string;
  sidebarBg: string;
  placeholder: string;
  accent: string;
  muted: string;
  success: string;
  warning: string;
  error: string;
  selection: string;
  surface: string;
  surfaceRaised: string;
  diffAdded: string;
  diffRemoved: string;
  diffAddedBg: string;
  diffRemovedBg: string;
};

export const themes: Record<string, Palette> = {
  // opencode's tokyonight.json, resolved: bg/panel/element steps, text, muted;
  // accent = their agent color (theme.secondary), selection = theme.primary
  tokyonight: {
    border: "#737aa2",
    bg: "#1a1b26",
    fg: "#c8d3f5",
    sidebarBg: "#1e2030",
    placeholder: "#828bb8",
    accent: "#c099ff",
    muted: "#828bb8",
    success: "#c3e88d",
    warning: "#ff966c",
    error: "#ff757f",
    selection: "#82aaff",
    surface: "#1e2030",
    surfaceRaised: "#222436",
      diffAdded: "#c3e88d",
    diffRemoved: "#ff757f",
    diffAddedBg: "#20303b",
    diffRemovedBg: "#37222c",
  },
  // default — slate, derived from the reference screenshots with opencode's generator math
  // surfaces follow opencode's step deltas (tokyonight): panel ≈ bg+5, element ≈ bg+10
  "kloudlite-dark": {
    border: "#556074",
    bg: "#262b34",
    fg: "#d5dce6",
    sidebarBg: "#2a303e",
    placeholder: "#6f7a88",
    accent: "#4f9fff",
    muted: "#7e8a9a",
    success: "#56d364",
    warning: "#d29922",
    error: "#ff7b72",
    selection: "#57b2e8",
    surface: "#2a303e",
    surfaceRaised: "#2e3444",
      diffAdded: "#56d364",
    diffRemoved: "#ff7b72",
    diffAddedBg: "#24332b",
    diffRemovedBg: "#3a262b",
  },
  // opencode's own default dark palette (their theme/assets/opencode.json defs)
  opencode: {
    border: "#484848",
    bg: "#0a0a0a",
    fg: "#eeeeee",
    sidebarBg: "#141414",
    placeholder: "#808080",
    accent: "#9d7cd8",
    muted: "#808080",
    success: "#7fd88f",
    warning: "#f5a742",
    error: "#e06c75",
    selection: "#fab283",
    surface: "#141414",
    surfaceRaised: "#1e1e1e",
      diffAdded: "#7fd88f",
    diffRemoved: "#e06c75",
    diffAddedBg: "#1e2a1e",
    diffRemovedBg: "#2e1d1f",
  },
  "kloudlite-light": {
    border: "#d1d5db",
    bg: "#ffffff",
    fg: "#1f2937",
    sidebarBg: "#fafafa",
    placeholder: "#9ca3af",
    accent: "#2563eb",
    muted: "#6b7280",
    success: "#166534",
    warning: "#b45309",
    error: "#dc2626",
    selection: "#2563eb",
    surface: "#fafafa",
    surfaceRaised: "#f5f5f5",
      diffAdded: "#166534",
    diffRemoved: "#dc2626",
    diffAddedBg: "#d5e5d5",
    diffRemovedBg: "#f7d8db",
  },
};

export const themeNames = Object.keys(themes);

import { readSettings } from "@kloudlite-tui/agent";

function initialName(): string {
  const env = process.env.KLOUDLITE_THEME ?? "";
  if (env in themes) return env;
  if (env === "light") return "kloudlite-light";
  const saved = readSettings().theme;
  if (saved && saved in themes) return saved;
  return "kloudlite-dark";
}

/**
 * Mutable singleton: components read `theme.x` at render time, so switching
 * only needs an in-place assign plus one React re-render (App bumps state).
 * ponytail: no persistence — theme resets to KLOUDLITE_THEME on restart.
 */
export const theme: Palette = { ...themes[initialName()]! };

export function setTheme(name: string): void {
  const next = themes[name];
  if (next) Object.assign(theme, next);
}
