import Link from "next/link";
import { Copy, Download, History } from "lucide-react";
import { Button } from "@/components/ui/button";
import { CodeBlock } from "@/components/repo/code-block";
import { RepoSidebar } from "@/components/repo/repo-sidebar";
import { FILE, REPO } from "@/lib/mock-repo";

/** A file: same tree on the left, the file on the right. Line numbers are anchors
 *  (#L12), the code scrolls horizontally inside its own box, never the page. */
export function FileView({ owner }: { owner: string }) {
  const base = `/${owner}/${REPO.name}`;
  const parts = FILE.path.split("/");
  const dir = parts.slice(0, -1).join("/");

  return (
    <div className="grid gap-8 lg:grid-cols-code">
      <aside className="hidden lg:block">
        <div className="sticky top-28">
          <RepoSidebar base={base} openDir={dir} activePath={FILE.path} />
        </div>
      </aside>

      <section className="min-w-0">
        <div className="flex h-8 items-center gap-3">
          <nav aria-label="Path" className="flex min-w-0 items-center gap-1 text-sm2">
            <Link href={base} className="font-medium underline-offset-4 hover:underline">{REPO.name}</Link>
            {parts.map((p, i) => (
              <span key={i} className="flex items-center gap-1">
                <span className="text-muted-foreground">/</span>
                {i === parts.length - 1
                  ? <span className="font-medium">{p}</span>
                  : <Link href={`${base}/tree/${parts.slice(0, i + 1).join("/")}`} className="text-primary underline-offset-4 hover:underline">{p}</Link>}
              </span>
            ))}
            <button type="button" aria-label="Copy path" className="ml-1 text-muted-foreground transition-colors hover:text-foreground"><Copy className="size-3.5" /></button>
          </nav>
        </div>

        <div className="mt-4 border border-border">
          <div className="flex items-center gap-3 border-b border-border bg-muted/40 px-4 py-2 text-caption text-muted-foreground">
            <span>{FILE.lines.length} lines</span>
            <span aria-hidden>·</span>
            <span>{FILE.size}</span>
            <span aria-hidden>·</span>
            <span className="font-mono">{FILE.path.split(".").pop()}</span>
            <div className="ml-auto flex items-center gap-1">
              <Button variant="ghost" size="sm" className="text-caption"><Copy />Copy</Button>
              <Button variant="ghost" size="sm" className="text-caption">Raw</Button>
              <Button variant="ghost" size="sm" className="text-caption"><Download />Download</Button>
              <Button asChild variant="ghost" size="sm" className="text-caption">
                <Link href={`${base}/commits`}><History />History</Link>
              </Button>
            </div>
          </div>
          <CodeBlock code={FILE.lines.join("\n")} path={FILE.path} />
        </div>
      </section>
    </div>
  );
}
