"use client";

import { useEffect, useRef, useState } from "react";
import { useTheme } from "next-themes";

/** A ```mermaid fence, drawn. The library is loaded on first use only (it is ~2 MB and most
 *  READMEs have no diagram), rendered with `securityLevel: "strict"` — labels are sanitized and
 *  click/link directives are inert — and re-drawn when the theme flips. A diagram that will not
 *  parse falls back to its source, the way the fence looked before there was a renderer. The
 *  SVG goes in through innerHTML: it is mermaid's own output from sanitized input, not the
 *  README's bytes, which is the line `Markdown` draws. */
export function MermaidBlock({ source }: { source: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const { resolvedTheme } = useTheme();
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let live = true;
    (async () => {
      try {
        const mermaid = (await import("mermaid")).default;
        mermaid.initialize({
          startOnLoad: false,
          securityLevel: "strict",
          theme: resolvedTheme === "dark" ? "dark" : "neutral",
          fontFamily: "inherit",
        });
        const { svg } = await mermaid.render(`mmd-${Math.random().toString(36).slice(2)}`, source);
        if (live && ref.current) ref.current.innerHTML = svg;
      } catch {
        if (live) setFailed(true);
      }
    })();
    return () => {
      live = false;
    };
  }, [source, resolvedTheme]);

  if (failed) return <pre className="overflow-x-auto px-4 py-3 font-mono text-caption">{source}</pre>;
  return <div ref={ref} className="mermaid-block overflow-x-auto px-4 py-3" />;
}
