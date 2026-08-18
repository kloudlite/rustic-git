import Link from "next/link";
import { Wordmark } from "@/components/brand/logo";
import { ThemeToggle } from "@/components/theme-toggle";

/** Two panels on desktop: the form on the left at a readable measure, and a panel on the
 *  right that says what the product is. Below lg the right panel is dropped rather than
 *  stacked — on a phone it would be a wall of text between the user and the sign-in. */
export default function AuthLayout({ children }: { children: React.ReactNode }) {
  return (
    <div className="grid min-h-svh lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
      <div className="flex flex-col">
        <header className="flex items-center justify-between p-6 lg:p-8">
          <Link href="/" aria-label="kloudlite home">
            <Wordmark />
          </Link>
          <ThemeToggle />
        </header>

        <main className="flex flex-1 items-center justify-center px-6 py-8 lg:px-8">
          <div className="w-full max-w-[400px]">{children}</div>
        </main>

        <footer className="flex flex-wrap items-center gap-x-5 gap-y-2 p-6 text-[13px] text-muted-foreground lg:p-8">
          <span>© {new Date().getFullYear()} kloudlite</span>
          <Link href="/privacy" className="hover:text-foreground">Privacy</Link>
          <Link href="/terms" className="hover:text-foreground">Terms</Link>
          <Link href="/docs" className="hover:text-foreground">Docs</Link>
        </footer>
      </div>

      <aside className="relative hidden overflow-hidden border-l border-border bg-muted/40 lg:flex lg:flex-col lg:justify-center lg:px-14 xl:px-20">
        <div
          aria-hidden
          className="pointer-events-none absolute inset-0 opacity-[0.4] dark:opacity-[0.25]
                     [background-image:linear-gradient(to_right,var(--border)_1px,transparent_1px),linear-gradient(to_bottom,var(--border)_1px,transparent_1px)]
                     [background-size:56px_56px]"
        />
        <div className="relative max-w-[440px]">
          <p className="text-[13px] font-semibold uppercase tracking-[0.14em] text-muted-foreground">
            Built for teams that ship
          </p>
          <h2 className="mt-5 text-3xl font-bold leading-[1.15] tracking-tight text-foreground xl:text-[34px]">
            Repositories, environments and pipelines in one place.
          </h2>
          <p className="mt-5 text-[15px] leading-relaxed text-muted-foreground">
            Git hosting that knows what happened to your code after it was pushed — which
            image it became, which pipeline built it, and which environment is running it
            right now.
          </p>

          <dl className="mt-10 grid grid-cols-2 gap-px border border-border bg-border">
            {[
              ["Git over HTTP and SSH", "Smart protocol, both transports"],
              ["Content-addressed", "Immutable object storage"],
              ["Environments", "See what is deployed where"],
              ["Registries", "Images beside the code"],
            ].map(([title, sub]) => (
              <div key={title} className="bg-background p-5">
                <dt className="text-[13.5px] font-semibold text-foreground">{title}</dt>
                <dd className="mt-1 text-[12.5px] leading-relaxed text-muted-foreground">{sub}</dd>
              </div>
            ))}
          </dl>
        </div>
      </aside>
    </div>
  );
}
