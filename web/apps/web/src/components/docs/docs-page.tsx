import Link from "next/link";
import { ArrowLeft, ArrowRight } from "lucide-react";
import { page } from "@/lib/docs";
import { Toc } from "@/components/docs/toc";
import { CopyButtons } from "@/components/docs/copy-buttons";

/** One documentation page: crumb, title, the rendered markdown, prev/next, and the outline
 *  beside it. The markdown is our own repository's, rendered by `lib/docs.ts` through marked
 *  and shiki, which is why `dangerouslySetInnerHTML` is acceptable here and nowhere else in
 *  the docs. */
export async function DocsPage({ slug }: { slug: string[] }) {
  const p = await page(slug);
  if (!p) return null;
  const isHome = p.slug === "";
  return (
    <>
      <main className="docs-main">
        <article className="docs-article">
          <nav className="docs-crumb" aria-label="Breadcrumb">
            <Link href="/docs">Docs</Link>
            {!isHome && (
              <>
                <span aria-hidden>/</span>
                <span>{p.section}</span>
              </>
            )}
          </nav>
          <header className="docs-title">
            <h1>{p.title}</h1>
            {p.description && !isHome && <p className="docs-lede">{p.description}</p>}
          </header>
          <div className="docs-prose" dangerouslySetInnerHTML={{ __html: p.html }} />
          <CopyButtons />
          {(p.prev || p.next) && (
            <footer className="docs-pager">
              {p.prev ? (
                <Link href={p.prev.href} className="docs-pager-link">
                  <span className="docs-pager-label"><ArrowLeft className="size-3.5" /> Previous</span>
                  <span className="docs-pager-title">{p.prev.title}</span>
                </Link>
              ) : (
                <span />
              )}
              {p.next ? (
                <Link href={p.next.href} className="docs-pager-link docs-pager-next">
                  <span className="docs-pager-label">Next <ArrowRight className="size-3.5" /></span>
                  <span className="docs-pager-title">{p.next.title}</span>
                </Link>
              ) : (
                <span />
              )}
            </footer>
          )}
        </article>
      </main>
      <aside className="docs-outline">
        <Toc headings={p.headings} />
      </aside>
    </>
  );
}
