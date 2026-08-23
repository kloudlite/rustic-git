import { highlight, langFor } from "@/lib/highlight";
import type { BundledLanguage } from "shiki";

/** A highlighted source block. Async server component: shiki runs once per render
 *  on the server and the browser receives coloured spans, nothing to hydrate —
 *  including the scrollbar, which is plain CSS overflow rather than a mounted
 *  ScrollArea per block. */
export async function CodeBlock({ code, path, lang }: { code: string; path?: string; lang?: BundledLanguage | "text" }) {
  const html = await highlight(code, lang ?? (path ? langFor(path) : "text"));
  return (
    <div className="code-block w-full overflow-x-auto">
      <div dangerouslySetInnerHTML={{ __html: html }} />
    </div>
  );
}
