import Link from "next/link";
import { Logo } from "@/components/brand/logo";
import { Button } from "@/components/ui/button";

export default function NotFound() {
  return (
    <div className="flex min-h-svh flex-col">
      <header className="flex h-14 items-center px-6">
        <Link href="/" aria-label="kloudlite home" className="inline-flex">
          <Logo className="h-5" />
        </Link>
      </header>
      <main className="flex flex-1 items-center justify-center px-6 pb-20">
        <div className="w-full max-w-auth">
          <p className="text-caption font-semibold uppercase tracking-eyebrow text-muted-foreground">
            404
          </p>
          <h1 className="mt-3 text-title font-semibold tracking-title">
            There&rsquo;s nothing at this address.
          </h1>
          <p className="mt-2 text-sm2 leading-relaxed text-muted-foreground">
            The page may have moved, or the link may be wrong.
          </p>
          <Button asChild variant="outline" className="mt-6 border-edge hover:border-edge-hover">
            <Link href="/">Back to home</Link>
          </Button>
        </div>
      </main>
    </div>
  );
}
