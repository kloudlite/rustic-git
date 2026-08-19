import Link from "next/link";
import { Copy, Download, History } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ScrollArea, ScrollBar } from "@/components/ui/scroll-area";
import { FileTree } from "@/components/repo/file-tree";
import { RefPicker } from "@/components/repo/ref-picker";
import { FILE, REPO } from "@/lib/mock-repo";

/** A file: same tree on the left, the file on the right. Line numbers are in a
 *  fixed gutter, the code scrolls horizontally inside its own box, never the page. */
export function FileView({ owner }: { owner: string }) {
  const base = `/${owner}/${REPO.name}`;
  const parts = FILE.path.split("/");
  const dir = parts.slice(0, -1).join("/");

  return (
    <div className="grid gap-8 lg:grid-cols-code">
      <aside className="hidden lg:block">
        <div className="sticky top-28">
          <FileTree base={base} openDir={dir} activePath={FILE.path} />
        </div>
      </aside>

      <section className="min-w-0">
        <div className="flex flex-wrap items-center gap-3">
          <RefPicker current={REPO.defaultBranch} branches={REPO.branches} tags={REPO.tags} />
          <nav aria-label="Path" className="flex items-center gap-1 text-sm2">
            <Link href={base} className="font-medium text-primary underline-offset-4 hover:underline">{REPO.name}</Link>
            {parts.map((p, i) => (
              <span key={i} className="flex items-center gap-1">
                <span className="text-muted-foreground">/</span>
                {i === parts.length - 1
                  ? <span className="font-medium">{p}</span>
                  : <Link href={`${base}/tree/${parts.slice(0, i + 1).join("/")}`} className="text-primary underline-offset-4 hover:underline">{p}</Link>}
              </span>
            ))}
          </nav>
        </div>

        <div className="mt-4 border border-border">
          <div className="flex items-center gap-3 border-b border-border bg-muted/40 px-4 py-2 text-caption text-muted-foreground">
            <span>{FILE.lines.length} lines</span>
            <span aria-hidden>·</span>
            <span>{FILE.size}</span>
            <div className="ml-auto flex items-center gap-1">
              <Button variant="ghost" size="sm" className="text-caption"><Copy />Raw</Button>
              <Button variant="ghost" size="sm" className="text-caption"><Download />Download</Button>
              <Button asChild variant="ghost" size="sm" className="text-caption">
                <Link href={`${base}/commits`}><History />History</Link>
              </Button>
            </div>
          </div>
          <ScrollArea className="w-full">
            <table className="w-full border-collapse font-mono text-caption leading-5">
              <tbody>
                {FILE.lines.map((l, i) => (
                  <tr key={i} className="hover:bg-muted/40">
                    <td className="w-12 select-none border-r border-border pr-3 text-right align-top text-muted-foreground/60">{i + 1}</td>
                    <td className="whitespace-pre pl-4 pr-6 text-foreground/90">{l || " "}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            <ScrollBar orientation="horizontal" />
          </ScrollArea>
        </div>
      </section>
    </div>
  );
}
