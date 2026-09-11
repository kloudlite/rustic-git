"use client";

import { useEffect } from "react";

/** Wires every rendered code block's Copy button. The buttons are in the markdown's HTML, so
 *  one delegated listener on the article is the whole component. */
export function CopyButtons() {
  useEffect(() => {
    const onClick = async (e: MouseEvent) => {
      const btn = (e.target as HTMLElement).closest<HTMLButtonElement>("button[data-copy]");
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
