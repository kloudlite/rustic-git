import Link from "next/link";
import { ArrowLeft } from "lucide-react";

/** The way up from a detail page to the list it came from. One per page, at the top,
 *  naming the parent — so the reader never has to work out which tab to reach for. */
export function BackLink({ href, children }: { href: string; children: React.ReactNode }) {
  return (
    <Link
      href={href}
      className="inline-flex h-7 items-center gap-1.5 text-sm2 text-muted-foreground transition-colors hover:text-foreground"
    >
      <ArrowLeft className="size-3.5" />
      {children}
    </Link>
  );
}
