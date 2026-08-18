import Link from "next/link";
import { ArrowRight, Boxes, FolderCode, Layers, LaptopMinimal, Workflow } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { ThemeToggle } from "@/components/theme-toggle";
import { Button } from "@/components/ui/button";

const CAPABILITIES = [
  { icon: FolderCode, title: "Code Repos", body: "Your source, versioned and hosted, with every commit traceable to what it became." },
  { icon: Boxes, title: "Package Registries", body: "Artifacts stored beside the code that produced them, addressed by digest." },
  { icon: LaptopMinimal, title: "Workspaces", body: "A ready development environment defined in the repo — no setup on a laptop." },
  { icon: Layers, title: "Environments", body: "Fork, snapshot and switch whole environments without losing state." },
  { icon: Workflow, title: "CI Triggers", body: "A push builds and ships. The result attaches to the commit, not a separate tool." },
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
            Cloud development environments
          </p>
          <h1 className="mt-5 max-w-[820px] text-[38px] font-bold leading-[1.1] tracking-tight md:text-[52px]">
            Designed to reduce the development loop.
          </h1>
          <p className="mt-6 max-w-[620px] text-[16.5px] leading-relaxed text-muted-foreground">
            No setup, no builds, no deployments. Your code, its packages, the workspace you write it
            in and the environment it runs in are one system — so the distance between an edit and
            seeing it live is as short as it can be.
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
          <div className="mx-auto grid max-w-[1120px] grid-cols-1 gap-px bg-border sm:grid-cols-2 lg:grid-cols-5">
            {CAPABILITIES.map(({ icon: Icon, title, body }) => (
              <div key={title} className="bg-background p-6">
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
              Focus on code, not ops.
            </h2>
            <p className="mt-4 max-w-[560px] text-[15px] leading-relaxed text-muted-foreground">
              Set up a workspace, push, and watch it run — without touching a build pipeline or a
              deployment script.
            </p>
            <div className="mt-7">
              <Button asChild className="font-semibold"><Link href="/signup">Create an account</Link></Button>
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
