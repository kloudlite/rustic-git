"use client";

import { useEffect } from "react";
import { useTheme } from "@/components/theme-provider";

/** Draws every ```mermaid fence in the README beside it.
 *
 *  The markdown is HTML from `lib/readme.ts` now, so a fence cannot be a React component; this is
 *  the same delegated shape the docs' `CopyButtons` uses — one client component that finds the
 *  blocks the server wrote and wires them (2026-09-12).
 *
 *  The library is loaded on first use only (~2 MB, most READMEs have no diagram), rendered with
 *  `securityLevel: "strict"` — labels are sanitized and click/link directives are inert — and
 *  re-drawn when the theme flips. A diagram that will not parse keeps its source, the way the
 *  fence looked before there was a renderer, and the source is ALSO what shows until the diagram
 *  is ready: an empty slot that grows a second after the page painted reads as a flicker. The SVG
 *  goes in through innerHTML: it is mermaid's own output from sanitized input, not the README's
 *  bytes, which is the line `renderReadme` draws. */
export function MermaidBlocks() {
  const { resolvedTheme } = useTheme();

  useEffect(() => {
    const blocks = [...document.querySelectorAll<HTMLPreElement>("pre[data-mermaid]")];
    if (blocks.length === 0) return;
    let live = true;
    (async () => {
      const mermaid = (await import("mermaid")).default;
      mermaid.initialize({
        startOnLoad: false,
        securityLevel: "strict",
        theme: resolvedTheme === "dark" ? "dark" : "neutral",
        fontFamily: "inherit",
      });
      for (const pre of blocks) {
        const target = pre.nextElementSibling as HTMLElement | null;
        if (!live || !target) return;
        try {
          // Rendered inside our own block: without a container mermaid measures the SVG in a
          // temporary element appended to <body>, which for one frame makes the document taller
          // than the app frame and flashes a window scrollbar beside the header.
          const { svg } = await mermaid.render(`mmd-${Math.random().toString(36).slice(2)}`, pre.textContent ?? "", target);
          if (!live) return;
          target.innerHTML = svg;
          target.hidden = false;
          pre.hidden = true;
        } catch {
          // Keep the source visible; a diagram that will not parse is not an error page.
        }
      }
    })();
    return () => {
      live = false;
    };
  }, [resolvedTheme]);

  return null;
}
