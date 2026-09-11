"use client";

import { useEffect } from "react";

const TAB_KEY = "docs.tab";

/** Wires every rendered code block's Copy button and every tab bar. Both are in the markdown's
 *  HTML, so one delegated listener on the document is the whole component. A tab choice is
 *  remembered across pages by name (`kl-connect`, `API`, `Console`): a reader who picked API once
 *  sees API everywhere, which is the reason tabs exist. */
export function CopyButtons() {
  useEffect(() => {
    let remembered: string | null = null;
    try {
      remembered = localStorage.getItem(TAB_KEY);
    } catch {}
    if (remembered) select(remembered, false);

    const onClick = async (e: MouseEvent) => {
      const el = e.target as HTMLElement;
      const tab = el.closest<HTMLButtonElement>("[role=tab][data-tab]");
      if (tab) {
        select(tab.dataset.tab!, true);
        return;
      }
      const btn = el.closest<HTMLButtonElement>("button[data-copy]");
      if (!btn) return;
      const code = btn.closest(".docs-code")?.querySelector("code")?.textContent ?? "";
      try {
        await navigator.clipboard.writeText(code);
        btn.textContent = "Copied";
        setTimeout(() => (btn.textContent = "Copy"), 1500);
      } catch {
        btn.textContent = "Copy failed";
      }
    };
    document.addEventListener("click", onClick);
    return () => document.removeEventListener("click", onClick);
  }, []);
  return null;
}

function select(name: string, remember: boolean) {
  for (const group of document.querySelectorAll<HTMLElement>(".docs-tabs")) {
    const tabs = [...group.querySelectorAll<HTMLElement>("[role=tab]")];
    if (!tabs.some((t) => t.dataset.tab === name)) continue;
    for (const t of tabs) t.setAttribute("aria-selected", String(t.dataset.tab === name));
    for (const p of group.querySelectorAll<HTMLElement>("[role=tabpanel]")) p.hidden = p.dataset.tab !== name;
  }
  if (remember) {
    try {
      localStorage.setItem(TAB_KEY, name);
    } catch {}
  }
}
