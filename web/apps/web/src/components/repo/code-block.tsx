import { ScrollArea } from "@/components/ui/scroll-area";
import { highlight, langFor } from "@/lib/highlight";
import type { BundledLanguage } from "shiki";

/** A highlighted source block. Async server component: shiki runs once per render
 *  on the server and the browser receives coloured spans, nothing to hydrate. */
export async function CodeBlock({ code, path, lang }: { code: string; path?: string; lang?: BundledLanguage | "text" }) {
  const html = await highlight(code, lang ?? (path ? langFor(path) : "text"));
  return (
    <ScrollArea orientation="horizontal" className="code-block w-full">
      <div dangerouslySetInnerHTML={{ __html: html }} />
    </ScrollArea>
  );
}
