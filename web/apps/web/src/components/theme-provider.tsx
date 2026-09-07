"use client";

import { createContext, useCallback, useContext, useEffect, useMemo, useSyncExternalStore } from "react";

export type Theme = "light" | "dark" | "system";
const KEY = "theme";
const QUERY = "(prefers-color-scheme: dark)";
const CHANGE = "themechange";

/* The class is set by the inline script in `layout.tsx` before first paint and by this
   provider after; both agree on what "apply" means. */
function apply(resolved: "light" | "dark") {
  const root = document.documentElement;
  root.classList.remove("light", "dark");
  root.classList.add(resolved);
  root.style.colorScheme = resolved;
}

const subscribeStored = (cb: () => void) => {
  addEventListener("storage", cb);
  addEventListener(CHANGE, cb);
  return () => {
    removeEventListener("storage", cb);
    removeEventListener(CHANGE, cb);
  };
};
const readStored = (): Theme => {
  const t = localStorage.getItem(KEY);
  return t === "light" || t === "dark" ? t : "system";
};
const subscribeSystem = (cb: () => void) => {
  const mq = matchMedia(QUERY);
  mq.addEventListener("change", cb);
  return () => mq.removeEventListener("change", cb);
};

const Ctx = createContext<{ theme: Theme; resolvedTheme: "light" | "dark"; setTheme: (t: Theme) => void }>({
  theme: "system",
  resolvedTheme: "light",
  setTheme: () => {},
});

/** Theme state without next-themes: that library renders its bootstrap <script> as a React
 *  element, and React warns whenever it has to create one on the client (a client re-render
 *  of the root, which in dev Fast Refresh does on every layout edit). Here the script is a
 *  plain server-rendered tag in the layout's head, and this provider only holds the choice —
 *  storage and the media query read as external stores, so the server snapshot is
 *  "system"/light and the client's first render already has the real value. */
export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const theme = useSyncExternalStore(subscribeStored, readStored, () => "system" as Theme);
  const systemDark = useSyncExternalStore(subscribeSystem, () => matchMedia(QUERY).matches, () => false);
  const resolvedTheme = theme === "system" ? (systemDark ? "dark" : "light") : theme;
  useEffect(() => apply(resolvedTheme), [resolvedTheme]);

  const setTheme = useCallback((t: Theme) => {
    if (t === "system") localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, t);
    dispatchEvent(new Event(CHANGE));
  }, []);

  const value = useMemo(() => ({ theme, resolvedTheme, setTheme }), [theme, resolvedTheme, setTheme]);
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

export function useTheme() {
  return useContext(Ctx);
}

/** Runs before first paint so the page never flashes the wrong theme. Server-rendered on
 *  purpose (see `ThemeProvider`); the source is a string, so it is never a React <script>. */
export const THEME_SCRIPT = `(function(){try{var t=localStorage.getItem("${KEY}");var d=t==="dark"||(t!=="light"&&matchMedia("${QUERY}").matches);var r=document.documentElement;r.classList.add(d?"dark":"light");r.style.colorScheme=d?"dark":"light"}catch(e){}})()`;
