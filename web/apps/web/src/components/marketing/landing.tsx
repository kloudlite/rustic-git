import Link from "next/link";
import { ArrowRight, Boxes, GitBranch, Layers, Workflow } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { ThemeToggle } from "@/components/theme-toggle";
import { Button } from "@/components/ui/button";

const CAPABILITIES = [
  { icon: GitBranch, title: "Git hosting", body: "Smart HTTP and SSH. Pack files in object storage, refs in an embedded database — no shared mutable state between repositories." },
  { icon: Boxes, title: "Registries", body: "The image a commit became, stored beside the code that produced it and addressed by the same digest." },
  { icon: Workflow, title: "Pipelines", body: "Every push builds. The result is attached to the commit, not filed away in a separate tool." },
  { icon: Layers, title: "Environments", body: "See which commit is running where, and how long it has been there." },
];

export function Landing() {
  return (
    <div className="flex min-h-svh flex-col">
      <header className="border-b border-border">
        <div className="mx-auto flex h-14 max-w-[1120px] items-center gap-4 px-6">
          <Link href="/" aria-label="kloudlite home"><Logo className="h-5" /></Link>
          <nav className="ml-4 hidden items-center gap-5 text-[13.5px] text-muted-foreground md:flex">
            <a href="https://kloudlite.io/docs" className="hover:text-foreground">Docs</a>
            <a href="https://kloudlite.io/pricing" className="hover:text-foreground">Pricing</a>
            <a href="https://github.com/kloudlite/kloudlite" className="hover:text-foreground">GitHub</a>
          </nav>
          <div className="flex-1" />
          <ThemeToggle className="hidden sm:inline-flex" />
          <Link href="/login" className="text-[13.5px] font-semibold hover:text-foreground">Sign in</Link>
          <Button asChild size="sm" className="font-semibold">
            <Link href="/signup">Get started</Link>
          </Button>
        </div>
      </header>

      <main className="flex-1">
        <section className="mx-auto max-w-[1120px] px-6 py-20 md:py-28">
          <p className="text-[13px] font-semibold uppercase tracking-[0.12em] text-muted-foreground">
            Source, build, ship
          </p>
          <h1 className="mt-5 max-w-[780px] text-[38px] font-bold leading-[1.1] tracking-tight md:text-[52px]">
            Git hosting that knows what happened to your code.
          </h1>
          <p className="mt-6 max-w-[600px] text-[16.5px] leading-relaxed text-muted-foreground">
            Most git hosts stop at the push. kloudlite carries a commit through to the image it
            became, the pipeline that built it and the environment running it — so the answer to
            &ldquo;what is in production?&rdquo; is on the commit itself.
          </p>
          <div className="mt-9 flex flex-wrap items-center gap-3">
            <Button asChild size="lg" className="h-11 font-semibold">
              <Link href="/signup">Get started<ArrowRight className="size-4" /></Link>
            </Button>
            <Button asChild variant="outline" size="lg" className="h-11 font-semibold">
              <a href="https://kloudlite.io/docs">Read the docs</a>
            </Button>
          </div>
        </section>

        <section className="border-y border-border">
          <div className="mx-auto grid max-w-[1120px] grid-cols-1 gap-px bg-border sm:grid-cols-2 lg:grid-cols-4">
            {CAPABILITIES.map(({ icon: Icon, title, body }) => (
              <div key={title} className="bg-background p-7">
                <Icon className="size-5 text-primary" />
                <h2 className="mt-4 text-[15px] font-bold">{title}</h2>
                <p className="mt-2 text-[13.5px] leading-relaxed text-muted-foreground">{body}</p>
              </div>
            ))}
          </div>
        </section>

        <section className="mx-auto max-w-[1120px] px-6 py-20">
          <div className="border border-border p-8 md:p-12">
            <h2 className="max-w-[560px] text-[26px] font-bold leading-tight tracking-tight md:text-[30px]">
              Self-host it, or let us run it.
            </h2>
            <p className="mt-4 max-w-[560px] text-[15px] leading-relaxed text-muted-foreground">
              The server is open source and runs on your own object storage. The hosted version is
              the same software with the operations taken care of.
            </p>
            <div className="mt-7 flex flex-wrap gap-3">
              <Button asChild className="font-semibold"><Link href="/signup">Create an account</Link></Button>
              <Button asChild variant="outline" className="font-semibold">
                <a href="https://github.com/kloudlite/kloudlite">View source</a>
              </Button>
            </div>
          </div>
        </section>
      </main>

      <footer className="border-t border-border">
        <div className="mx-auto flex max-w-[1120px] flex-wrap items-center gap-x-6 gap-y-2 px-6 py-6 text-[13px] text-muted-foreground">
          <span>© {new Date().getFullYear()} kloudlite</span>
          <a href="https://kloudlite.io/privacy" className="hover:text-foreground">Privacy</a>
          <a href="https://kloudlite.io/terms" className="hover:text-foreground">Terms</a>
          <a href="https://kloudlite.io/branding" className="hover:text-foreground">Branding</a>
        </div>
      </footer>
    </div>
  );
}
