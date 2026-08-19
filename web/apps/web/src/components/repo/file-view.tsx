import Link from "next/link";
import { Copy, Download, History } from "lucide-react";
import { Button } from "@/components/ui/button";
import { CodeBlock } from "@/components/repo/code-block";
import { FileSearch } from "@/components/repo/file-search";
import { RefPicker } from "@/components/repo/ref-picker";
import { RepoAbout } from "@/components/repo/repo-about";
import { FILE, PATHS, REPO } from "@/lib/mock-repo";

/** A file: same tree on the left, the file on the right. Line numbers are anchors
 *  (#L12), the code scrolls horizontally inside its own box, never the page. */
export function FileView({ owner }: { owner: string }) {
  const base = `/${owner}/${REPO.name}`;
  const parts = FILE.path.split("/");

  return (
    <div className="grid gap-10 xl:grid-cols-code-rail">
      <section className="min-w-0">
        <div className="flex flex-wrap items-center gap-3">
          <RefPicker current={REPO.defaultBranch} defaultBranch={REPO.defaultBranch} branches={REPO.branches} tags={REPO.tags} />
          <FileSearch base={base} entries={PATHS} className="w-full max-w-xs" />
        </div>

        <nav aria-label="Path" className="mt-5 flex min-w-0 items-center gap-1 text-sm2">
          <Link href={`/${owner}`} className="text-muted-foreground underline-offset-4 hover:text-foreground hover:underline">{owner}</Link>
          <span className="text-muted-foreground">/</span>
          <Link href={base} className="text-primary underline-offset-4 hover:underline">{REPO.name}</Link>
          {parts.map((p, i) => (
            <span key={i} className="flex items-center gap-1">
              <span className="text-muted-foreground">/</span>
              {i === parts.length - 1
                ? <span className="font-medium">{p}</span>
                : <Link href={`${base}/tree/${parts.slice(0, i + 1).join("/")}`} className="text-primary underline-offset-4 hover:underline">{p}</Link>}
            </span>
          ))}
          <Button type="button" variant="ghost" size="icon-xs" aria-label="Copy path" className="ml-1 text-muted-foreground hover:text-foreground"><Copy /></Button>
        </nav>

        <div className="mt-3 border border-border bg-card">
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

      <aside className="hidden xl:block">
        <RepoAbout base={base} />
      </aside>
    </div>
  );
}
