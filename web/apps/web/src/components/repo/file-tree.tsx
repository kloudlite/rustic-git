import Link from "next/link";
import { ChevronDown, ChevronRight, File, Folder } from "lucide-react";
import { TREE } from "@/lib/mock-repo";
import { cn } from "@/lib/utils";
import { ScrollArea } from "@/components/ui/scroll-area";

/** The fixed left column of the Code view. Always the same tree for the same ref,
 *  regardless of which file is open — the file is the moving part, the tree the
 *  fixed one, so the eye never loses its place. */
export function FileTree({ base, openDir, activePath }: { base: string; openDir?: string; activePath?: string }) {
  const root = TREE[""];
  return (
    <ScrollArea className="max-h-sidecol">
    <nav aria-label="Files" className="text-sm2">
      <ul className="grid gap-px">
        {root.map((e) => {
          const path = e.name;
          const isOpen = e.kind === "dir" && openDir === path;
          const isActive = activePath === path;
          return (
            <li key={path}>
              <Link
                href={e.kind === "dir" ? `${base}/tree/${path}` : `${base}/blob/${path}`}
                className={cn(
                  "flex h-7 items-center gap-1.5 px-2 transition-colors hover:bg-muted",
                  isActive ? "bg-muted font-medium text-foreground" : "text-foreground/80",
                )}
              >
                {e.kind === "dir" ? (
                  isOpen ? <ChevronDown className="size-3.5 text-muted-foreground" /> : <ChevronRight className="size-3.5 text-muted-foreground" />
                ) : (
                  <span className="size-3.5" />
                )}
                {e.kind === "dir" ? <Folder className="size-4 text-muted-foreground" /> : <File className="size-4 text-muted-foreground" />}
                <span className="truncate">{e.name}</span>
              </Link>
              {isOpen && TREE[path] && (
                <ul className="mt-px grid gap-px pl-5">
                  {TREE[path].map((c) => {
                    const cp = `${path}/${c.name}`;
                    const cActive = activePath === cp;
                    return (
                      <li key={cp}>
                        <Link
                          href={c.kind === "dir" ? `${base}/tree/${cp}` : `${base}/blob/${cp}`}
                          className={cn(
                            "flex h-7 items-center gap-1.5 px-2 transition-colors hover:bg-muted",
                            cActive ? "bg-muted font-medium text-foreground" : "text-foreground/80",
                          )}
                        >
                          <span className="size-3.5" />
                          {c.kind === "dir" ? <Folder className="size-4 text-muted-foreground" /> : <File className="size-4 text-muted-foreground" />}
                          <span className="truncate">{c.name}</span>
                        </Link>
                      </li>
                    );
                  })}
                </ul>
              )}
            </li>
          );
        })}
      </ul>
    </nav>
    </ScrollArea>
  );
}
