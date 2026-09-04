# Web Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land every web-app finding from the 2026-08-23 code review — the passkey sign-in that can never succeed first, then the crashes and wrong-namespace bugs, then the medium hygiene, then the redundancy — each as its own commit.

**Architecture:** All source is under `web/apps/web/src/`. Fixes follow the patterns already in the tree: `useActionState` + `{ error }` for anything that can fail, `cache()`-wrapped guards shared by a layout and its pages, `repo-list.tsx`-style local filtering, tokens over raw colours, `--radius: 0`. Features that do not exist (team editing, issues, workspaces, environments, CI) are replaced by an explicit "not available yet" empty state — nothing in this plan builds a feature. Mock data files are deleted once their last importer goes.

**Tech Stack:** Next.js 16 app router (read `node_modules/next/dist/docs/` when unsure — it differs from training data), React 19, Auth.js v5 beta, shiki 4, Tailwind 4, bun 1.3.14 (`bun test` for the one unit test — no new dependencies).

**Spec:** `docs/code-review-2026-08-23.md` — sections 0 (#2), 1 (Low: `(auth)/actions.ts`), 2 (Critical/High web rows, Medium web rows, Low web rows), 3 (`lib/highlight.ts`), 4 (Web), 5 (last bullet), 6 (`web/` has no tests).

## Global Constraints

- After EVERY task, from `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` must both pass. Editor TS diagnostics are frequently stale; trust `tsc`.
- No new dependencies. The only test runner is `bun test` (already works — verified with a throwaway file), reading `*.test.ts` next to the code.
- House style (`CLAUDE.md`): comments explain WHY, never what. Deliberate shortcuts are marked `// ponytail: <ceiling and upgrade path>`. Commit subjects are imperative sentence case, NO tool attribution, no "claude" anywhere in the message.
- `import "server-only"` modules (`lib/api.ts`, `lib/browse.ts`, `lib/passkey.ts`, `lib/session.ts`, …) cannot be imported by `"use client"` components except as `import type`. Anything a client component needs at runtime goes in a module without that import (`lib/utils.ts`, a new file, or the component itself).
- We are in implementation phase: where a feature does not exist, render an honest empty state; do NOT build the feature.
- The web image only rebuilds when `web/**` changes and `deploy/kloudlite-git-web.yaml` is pinned by hand (see `CLAUDE.md` "Deploying"). Deploying is outside this plan.

---

## CRITICAL

### Task 1: Make the passkey assertion survive an email with a dot (and add the first web test)

**Files:**
- Create: `web/apps/web/src/lib/assertion.ts`
- Create: `web/apps/web/src/lib/assertion.test.ts`
- Modify: `web/apps/web/src/lib/passkey.ts:62-94` (move `signAssertion`/`verifyAssertion` out)
- Modify: `web/apps/web/src/auth.ts:6` (import path)
- Modify: `web/apps/web/package.json` (add `test` script)
- Modify: `web/apps/web/tsconfig.json` (exclude `*.test.ts` — no `bun-types` installed, so `tsc` cannot resolve `bun:test`)

**Context:** `signAssertion` produces `"${email}.${exp}.${mac}"`; `verifyAssertion` does `assertion.split(".")` and requires exactly 3 parts. `a.b@c.com` has dots, so every real assertion has ≥4 parts and is rejected — passkey sign-in cannot succeed. Fix by cutting from the END (`lastIndexOf` twice): `exp` is digits and `mac` is base64url, neither contains a dot, so the email is everything before the second-last dot.

The functions move to a module with no `server-only`/`next/headers` import so `bun test` can load it (`server-only` throws outside a React server context). `passkey.ts` keeps re-exporting nothing — callers import from `lib/assertion` directly.

**Interfaces:**
- Produces: `signAssertion(email: string, now?: number): string`, `verifyAssertion(assertion: string, now?: number): string | null` in `@/lib/assertion`. `now` defaults to `Date.now()` and exists only so the test can make an expired assertion.
- Consumers: `src/auth.ts` (`verifyAssertion`), `src/app/(auth)/passkey/actions.ts:19` (`signAssertion` — currently imported from `@/lib/passkey`).

- [ ] **Step 1: Write the failing test**

Create `web/apps/web/src/lib/assertion.test.ts`:

```ts
import { expect, test } from "bun:test";
import { signAssertion, verifyAssertion } from "./assertion";

process.env.AUTH_SECRET = "test-secret";

test("an email with dots round-trips", () => {
  const email = "first.last@example.co.uk";
  expect(verifyAssertion(signAssertion(email))).toBe(email);
});

test("the email is lowercased on the way in", () => {
  expect(verifyAssertion(signAssertion("Ada@Example.com"))).toBe("ada@example.com");
});

test("an expired assertion is refused", () => {
  const stale = signAssertion("ada@example.com", Date.now() - 120_000);
  expect(verifyAssertion(stale)).toBeNull();
});

test("a tampered email is refused", () => {
  const a = signAssertion("ada@example.com");
  const swapped = `eve@example.com${a.slice("ada@example.com".length)}`;
  expect(verifyAssertion(swapped)).toBeNull();
});

test("a tampered mac is refused", () => {
  const a = signAssertion("ada@example.com");
  expect(verifyAssertion(a.slice(0, -1) + (a.endsWith("A") ? "B" : "A"))).toBeNull();
});

test("garbage is refused", () => {
  expect(verifyAssertion("")).toBeNull();
  expect(verifyAssertion("nodots")).toBeNull();
  expect(verifyAssertion("one.dot")).toBeNull();
});
```

- [ ] **Step 2: Run it to verify it fails**

Run from `web/apps/web`: `bun test src/lib/assertion.test.ts`
Expected: FAIL — `Cannot find module "./assertion"`.

- [ ] **Step 3: Create `lib/assertion.ts`**

```ts
import { createHmac, timingSafeEqual } from "node:crypto";

/**
 * A one-minute, single-purpose proof that the server verified a passkey.
 *
 * Auth.js exposes every credentials provider at a public callback URL, so a
 * provider that accepted `{ email }` would let anyone POST their way into any
 * account. The provider therefore accepts only this: an HMAC over the email and
 * an expiry, keyed by AUTH_SECRET, which the browser cannot produce.
 *
 * Kept free of `server-only` and `next/headers` so it can be unit-tested; the
 * WebAuthn ceremony that calls it stays in `lib/passkey.ts`.
 */
function assertionKey() {
  const secret = process.env.AUTH_SECRET;
  if (!secret) throw new Error("AUTH_SECRET is required to sign a passkey assertion");
  return secret;
}

export function signAssertion(email: string, now = Date.now()): string {
  const exp = now + 60_000;
  const body = `${email.toLowerCase()}.${exp}`;
  const mac = createHmac("sha256", assertionKey()).update(body).digest("base64url");
  return `${body}.${mac}`;
}

/** The email, if this really was signed here and has not expired. */
export function verifyAssertion(assertion: string, now = Date.now()): string | null {
  // Cut from the end: an email can contain any number of dots, but the expiry is
  // digits and the mac is base64url, so the last two dots are the separators.
  const macAt = assertion.lastIndexOf(".");
  if (macAt <= 0) return null;
  const expAt = assertion.lastIndexOf(".", macAt - 1);
  if (expAt <= 0) return null;
  const email = assertion.slice(0, expAt);
  const exp = assertion.slice(expAt + 1, macAt);
  const mac = assertion.slice(macAt + 1);
  const expected = createHmac("sha256", assertionKey()).update(`${email}.${exp}`).digest("base64url");
  const a = Buffer.from(mac);
  const b = Buffer.from(expected);
  if (a.length !== b.length || !timingSafeEqual(a, b)) return null;
  if (!/^\d+$/.test(exp) || Number(exp) < now) return null;
  return email;
}
```

- [ ] **Step 4: Remove the two functions from `passkey.ts` and repoint imports**

In `web/apps/web/src/lib/passkey.ts` delete lines 62–94 (the doc comment starting "A one-minute, single-purpose proof…", `assertionKey`, `signAssertion`, `verifyAssertion`) and drop the now-unused `createHmac, timingSafeEqual` import on line 3.

In `web/apps/web/src/auth.ts` line 6: `import { verifyAssertion } from "@/lib/assertion";`

In `web/apps/web/src/app/(auth)/passkey/actions.ts` line 19 becomes two lines:
```ts
import { relyingParty, rememberChallenge, takeChallenge } from "@/lib/passkey";
import { signAssertion } from "@/lib/assertion";
```

- [ ] **Step 5: Wire `bun test` in without breaking `tsc`**

`web/apps/web/package.json` scripts — add `"test": "bun test"` after `"typecheck"`.

`web/apps/web/tsconfig.json` — change `"exclude": ["node_modules"]` to `"exclude": ["node_modules", "**/*.test.ts"]`. (No `bun-types`/`@types/bun` is installed and we add no deps, so `tsc` must not see `import … from "bun:test"`.)

- [ ] **Step 6: Run the test, lint, typecheck**

From `web/apps/web`: `bun test` → Expected: 6 pass.
From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → Expected: clean. If eslint complains about resolving `bun:test`, add `"**/*.test.ts"` to `globalIgnores` in `web/apps/web/eslint.config.mjs` and say so in the commit body.

- [ ] **Step 7: Commit**

```bash
git add web/apps/web/src/lib/assertion.ts web/apps/web/src/lib/assertion.test.ts web/apps/web/src/lib/passkey.ts web/apps/web/src/auth.ts "web/apps/web/src/app/(auth)/passkey/actions.ts" web/apps/web/package.json web/apps/web/tsconfig.json
git commit -m "Parse the passkey assertion from the end so emails with dots verify"
```

---

## HIGH

### Task 2: Unknown fence languages fall back to text; grammars load lazily; cap highlight size

**Files:**
- Modify: `web/apps/web/src/lib/highlight.ts` (whole file)
- Modify: `web/apps/web/src/components/repo/code.tsx:3,34`

**Context:** `code.tsx:34` casts any fence word to `BundledLanguage`; shiki throws on `console`, `mermaid`, … and the whole repo page 500s. `highlight.ts` also loads 40 grammars on first use and highlights blobs of any size. Verified in `node_modules/.bun/@shikijs+types@4.4.3/…/index.d.mts:770`: `loadLanguage(...langs: (LanguageInput | BundledLangKeys | SpecialLanguage)[]) => Promise<void>` exists on the highlighter, so `createHighlighter({ langs: [] })` + per-language `loadLanguage` is supported.

**Interfaces:**
- Produces: `fenceLang(name: string | undefined): BundledLanguage | "text"` exported from `@/lib/highlight`. `highlight(code, lang)` signature unchanged.

- [ ] **Step 1: Rewrite `lib/highlight.ts`**

Replace the top of the file (lines 1–62, through `langFor`) with:

```ts
import "server-only";
import { createHighlighter, type Highlighter, type BundledLanguage } from "shiki";

/** Server-side syntax highlighting. One highlighter for the process; themes are
 *  emitted as CSS variables (--shiki-light / --shiki-dark) so the page's theme
 *  class picks the colour with no re-render and no client JavaScript.
 *
 *  Grammars are loaded on first use, not up front: forty grammars took seconds on
 *  the first request and most deployments only ever render a handful. */
let instance: Promise<Highlighter> | null = null;

function highlighter() {
  instance ??= createHighlighter({ themes: ["github-light", "github-dark"], langs: [] });
  return instance;
}

/** Every grammar this app will ever load, so a fence or a filename can be checked
 *  against a closed set. A name not here renders as text — shiki throws on an
 *  unknown language, and a README fence is not worth a 500. */
const LANGS = new Set<string>([
  "rust", "toml", "yaml", "json", "markdown", "bash", "typescript", "tsx",
  "javascript", "jsx", "hcl", "dockerfile", "diff", "python", "go", "java",
  "kotlin", "swift", "ruby", "php", "c", "cpp", "csharp", "sql", "css",
  "scss", "html", "vue", "svelte", "lua", "zig", "dart", "elixir", "haskell",
  "scala", "ini", "xml", "graphql", "proto", "nix", "make",
]);

const loaded = new Map<string, Promise<void>>();

/** One in-flight load per grammar, so two concurrent renders of the first `.rs`
 *  file do not both load rust. */
async function ensure(lang: BundledLanguage) {
  const h = await highlighter();
  let p = loaded.get(lang);
  if (!p) {
    p = h.loadLanguage(lang);
    loaded.set(lang, p);
  }
  await p;
}

/** Extension to grammar. Kept beside `lib/languages.ts`, which maps the same
 *  extensions to display names and colours — one answers "how do I colour this
 *  file", the other "what is this repo written in", and a file type missing from
 *  either is a file that looks wrong in one place and invisible in the other. */
const BY_EXT: Record<string, BundledLanguage> = {
  rs: "rust", toml: "toml", yaml: "yaml", yml: "yaml", json: "json", jsonc: "json",
  md: "markdown", mdx: "markdown", sh: "bash", bash: "bash", zsh: "bash", fish: "bash",
  ts: "typescript", mts: "typescript", cts: "typescript", tsx: "tsx",
  js: "javascript", mjs: "javascript", cjs: "javascript", jsx: "jsx",
  tf: "hcl", hcl: "hcl", diff: "diff", patch: "diff",
  py: "python", go: "go", java: "java", kt: "kotlin", kts: "kotlin", swift: "swift",
  rb: "ruby", php: "php", c: "c", h: "c", cpp: "cpp", cc: "cpp", cxx: "cpp",
  hpp: "cpp", hh: "cpp", cs: "csharp", sql: "sql", css: "css", scss: "scss",
  sass: "scss", html: "html", htm: "html", vue: "vue", svelte: "svelte",
  lua: "lua", zig: "zig", dart: "dart", ex: "elixir", exs: "elixir",
  hs: "haskell", scala: "scala", sbt: "scala", ini: "ini", cfg: "ini",
  xml: "xml", svg: "xml", graphql: "graphql", gql: "graphql", proto: "proto",
  nix: "nix", mk: "make",
};

/** Files whose type is their whole name. */
const BY_NAME: Record<string, BundledLanguage> = {
  Dockerfile: "dockerfile",
  Containerfile: "dockerfile",
  Makefile: "make",
  Gemfile: "ruby",
  Rakefile: "ruby",
  ".gitignore": "ini",
  ".dockerignore": "ini",
  ".env": "ini",
};

export function langFor(path: string): BundledLanguage | "text" {
  const base = path.split("/").pop() ?? "";
  if (BY_NAME[base]) return BY_NAME[base];
  // A dotfile with nothing after the leading dot has no extension to read.
  const dot = base.lastIndexOf(".");
  if (dot <= 0) return "text";
  return BY_EXT[base.slice(dot + 1).toLowerCase()] ?? "text";
}

/** The word after a markdown fence — `rs`, `rust`, `console`, nothing. Accepts a
 *  grammar name or a file extension; anything else is text. */
export function fenceLang(name: string | undefined): BundledLanguage | "text" {
  if (!name) return "text";
  const n = name.toLowerCase();
  if (BY_EXT[n]) return BY_EXT[n];
  return LANGS.has(n) ? (n as BundledLanguage) : "text";
}

/** Past this, highlighting is seconds of CPU for a file nobody reads as prose. */
// ponytail: fixed 200 KB cap; make it per-language if a generated file type ever needs colour
const MAX_HIGHLIGHT = 200_000;
```

Then replace `highlight` (the old lines 64–82) with:

```ts
/** Returns `<pre class="shiki"><code>…</code></pre>` with one `<span class="line"
 *  id="L<n>" data-line="<n>">` per line, so numbers and anchors are CSS and links. */
export async function highlight(code: string, lang: BundledLanguage | "text") {
  const use = lang !== "text" && code.length <= MAX_HIGHLIGHT ? lang : "text";
  if (use !== "text") await ensure(use);
  const h = await highlighter();
  const html = await h.codeToHtml(code, {
    lang: use,
    themes: { light: "github-light", dark: "github-dark" },
    defaultColor: false,
    transformers: [
      {
        line(node, line) {
          node.properties.id = `L${line}`;
          node.properties["data-line"] = String(line);
        },
      },
    ],
  });
  return blockLines(html);
}
```

Leave `blockLines` as is.

- [ ] **Step 2: Use `fenceLang` in the README renderer**

`web/apps/web/src/components/repo/code.tsx`: delete line 3 (`import type { BundledLanguage } from "shiki";`), add `import { fenceLang } from "@/lib/highlight";`, and change line 34 to:

```tsx
          const lang = fenceLang(b.match(/^```(\w+)/)?.[1]);
```

(The old default of `"bash"` for a bare fence is gone on purpose: an untagged fence is text.)

- [ ] **Step 3: Verify**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean.
Manual: run the app against a repo whose README has a ```` ```console ```` fence (or temporarily add one to any repo) and confirm the page renders instead of 500ing. If you cannot run the app, note that in the commit body.

- [ ] **Step 4: Commit**

```bash
git add web/apps/web/src/lib/highlight.ts web/apps/web/src/components/repo/code.tsx
git commit -m "Render unknown fence languages as text and load grammars on demand"
```

---

### Task 3: The shell reads the owner from the URL; ⌘K searches real repos

**Files:**
- Modify: `web/apps/web/src/components/app/shell-nav.tsx` (whole file)
- Modify: `web/apps/web/src/components/app/app-shell.tsx` (whole file)
- Modify: `web/apps/web/src/components/app/global-search.tsx` (whole file)

**Context:** `app-shell.tsx:50` takes `owner = session.user.owner` and builds every org tab, crumb link and the team switcher from it — so on `/some-team/...` the chrome shows the person's own namespace. The shell is a layout that stays mounted across navigations, so the fix is the one the file's own comment describes: the client components read the URL. `place()` in `shell-nav.tsx` already parses it; it just never returned the owner for the org case. `global-search.tsx` renders hard-coded mock groups (`REPOS`, `WORKSPACE_SESSIONS`, …) linking to a non-existent `kloudlite` repo; it gets the real repo list instead (fetched once per shell render for every owner the person can act in) and the mock groups go.

**Interfaces:**
- Produces: `place(pathname: string, me: string)` and `useOwner(me: string): string` exported from `shell-nav.tsx`. `ShellTabs({ repoTabs, imageTabs, me, className })`, `ShellCrumb({ me, owners })`, `GlobalSearch({ me, owners, repos })`.
- Consumes: `sections(owner)` / `settingsSection(owner)` from `sections.ts` (plain module, safe in client code); `RESERVED` from `lib/reserved.ts`; `ApiRepo` type from `lib/api.ts` (type-only import).

- [ ] **Step 1: Rewrite `shell-nav.tsx`**

```tsx
"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { NavTabs } from "@/components/app/nav-tabs";
import { useRepoMeta } from "@/components/app/shell-context";
import { sections, settingsSection } from "@/components/app/sections";
import { TeamSwitcher, type SwitcherOwner } from "@/components/app/team-switcher";
import { RESERVED } from "@/lib/reserved";
import { Badge } from "@/components/ui/badge";

/** A repo tab, as the shell is given it: the icon is already rendered, because a
 *  component cannot cross from the server into here, and the href is a suffix
 *  because which repo it belongs to is only known from the URL. */
export type RepoTabSpec = { suffix: string; label: string; icon: React.ReactNode; end?: boolean };

/** Pages that hang off the root rather than off an owner. A URL starting with one
 *  of these names nobody's namespace, so the chrome shows the person's own. */
// ponytail: a person whose handle is one of these words would see the wrong crumb; `settings` is already refused as a handle, the other two are not
const ROOT_PAGES = ["settings", "new-repo", "new-team"];

/** Where the URL is, in the terms the chrome cares about.
 *
 *  `/{owner}/{x}` is unambiguous because the names the namespace has spent —
 *  settings, activity, ci, and the rest — cannot be repo names; repo creation
 *  refuses them. So the second segment names a repo or it names a section, and
 *  the chrome can tell which without asking anyone.
 *
 *  `/{owner}/registries/{image}` is a third place, one level deeper: `registries`
 *  is itself a reserved section (the Container Images list), so it is already
 *  caught by the `repo` branch above at two segments — the third segment is what
 *  tells an image page apart from the list page it hangs off of.
 *
 *  The owner is the first segment whenever there is one. The shell is a layout
 *  that stays mounted across navigations, so it cannot be handed the owner by a
 *  page; reading the URL is the only way a team's pages get the team's chrome. */
export function place(pathname: string, me: string) {
  const parts = pathname.split("/").filter(Boolean);
  const owner = parts[0] && !ROOT_PAGES.includes(parts[0]) ? parts[0] : me;
  if (parts.length >= 3 && parts[1] === "registries") {
    return { kind: "image" as const, owner, image: parts[2] };
  }
  if (parts.length >= 2 && !(RESERVED as readonly string[]).includes(parts[1])) {
    return { kind: "repo" as const, owner, repo: parts[1] };
  }
  return { kind: "org" as const, owner };
}

export function useOwner(me: string) {
  return place(usePathname(), me).owner;
}

export function ShellTabs({
  repoTabs,
  imageTabs,
  me,
  className,
}: {
  repoTabs: RepoTabSpec[];
  imageTabs: RepoTabSpec[];
  /** The signed-in person's own handle: what the chrome falls back to at `/`. */
  me: string;
  className?: string;
}) {
  const at = place(usePathname(), me);
  if (at.kind === "org") {
    const tabs = [...sections(at.owner), settingsSection(at.owner)].map(
      ({ href, label, icon: Icon }, i, all) => ({ href, label, icon: <Icon />, end: i === all.length - 1 }),
    );
    return <NavTabs tabs={tabs} className={className} aria-label="Sections" />;
  }
  if (at.kind === "image") {
    const base = `/${at.owner}/registries/${at.image}`;
    return (
      <NavTabs
        tabs={imageTabs.map((t) => ({ href: `${base}${t.suffix}`, label: t.label, icon: t.icon, end: t.end }))}
        back={{ href: `/${at.owner}/registries`, label: "Container Images" }}
        className={className}
        aria-label={at.image}
      />
    );
  }
  const base = `/${at.owner}/${at.repo}`;
  return (
    <NavTabs
      tabs={repoTabs.map((t) => ({ href: `${base}${t.suffix}`, label: t.label, icon: t.icon, end: t.end }))}
      back={{ href: `/${at.owner}`, label: "Repos" }}
      className={className}
      aria-label={at.repo}
    />
  );
}

/** The list a repo or an image came from, as a crumb segment. */
function SectionLink({ owner, label }: { owner: string; label: "Code Repos" | "Container Images" }) {
  const s = sections(owner).find((x) => x.label === label)!;
  const Icon = s.icon;
  return (
    <Link
      href={s.href}
      className="flex h-8 items-center gap-1.5 px-2 text-sm2 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
    >
      <Icon className="size-3.5" />
      {s.label}
    </Link>
  );
}

function OwnerLink({ owner }: { owner: string }) {
  return (
    <Link
      href={`/${owner}`}
      className="flex h-8 items-center gap-2 px-2 text-sm2 text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
    >
      <span className="size-3.5 shrink-0 bg-primary" aria-hidden />
      {owner}
    </Link>
  );
}

/** The breadcrumb, which grows a segment inside a repo or an image. */
export function ShellCrumb({ me, owners }: { me: string; owners: SwitcherOwner[] }) {
  const at = place(usePathname(), me);
  const meta = useRepoMeta();
  if (at.kind === "org") return <TeamSwitcher current={at.owner} owners={owners} />;

  const sep = <span className="text-muted-foreground/40" aria-hidden>/</span>;
  if (at.kind === "image") {
    return (
      <>
        <OwnerLink owner={at.owner} />
        {sep}
        <SectionLink owner={at.owner} label="Container Images" />
        {sep}
        <Link
          href={`/${at.owner}/registries/${at.image}`}
          className="flex h-8 items-center gap-2 px-2 text-sm2 font-medium transition-colors hover:bg-muted"
        >
          {at.image}
        </Link>
      </>
    );
  }

  return (
    <>
      <OwnerLink owner={at.owner} />
      {sep}
      <SectionLink owner={at.owner} label="Code Repos" />
      {sep}
      <Link
        href={`/${at.owner}/${at.repo}`}
        className="flex h-8 items-center gap-2 px-2 text-sm2 font-medium transition-colors hover:bg-muted"
      >
        {at.repo}
        {/* Only once the layout beneath has said so. A badge that guessed would
            be worse than one that arrives a moment later. */}
        {meta && <Badge variant="outline">{meta.visibility}</Badge>}
      </Link>
    </>
  );
}
```

- [ ] **Step 2: Rewrite `app-shell.tsx`**

```tsx
import Link from "next/link";
import { CircleDot, Code, Container, GitPullRequest, Settings, Tag } from "lucide-react";
import { Logo } from "@/components/brand/logo";
import { UserMenu } from "@/components/app/user-menu";
import { GlobalSearch } from "@/components/app/global-search";
import { ShellState } from "@/components/app/shell-context";
import { ShellCrumb, ShellTabs, type RepoTabSpec } from "@/components/app/shell-nav";
import { ownersFor } from "@/lib/owners";
import { apiToken } from "@/lib/api-token";
import { listRepos } from "@/lib/api";
import { ScrollArea } from "@/components/ui/scroll-area";
import type { Session } from "@/lib/session";

/** The repo's tabs, as suffixes — which repo they belong to is a fact about the
 *  URL, and the shell reads that itself. */
const REPO_TABS: RepoTabSpec[] = [
  { suffix: "", label: "Code", icon: <Code /> },
  { suffix: "/issues", label: "Issues", icon: <CircleDot /> },
  { suffix: "/pulls", label: "Pull requests", icon: <GitPullRequest /> },
  { suffix: "/settings", label: "Settings", icon: <Settings />, end: true },
];

/** An image's tabs, same shape as `REPO_TABS` — which image they belong to is a
 *  fact about the URL, read by the shell itself. */
const IMAGE_TABS: RepoTabSpec[] = [
  { suffix: "", label: "Details", icon: <Container /> },
  { suffix: "/tags", label: "Tags", icon: <Tag /> },
  { suffix: "/settings", label: "Settings", icon: <Settings />, end: true },
];

/**
 * The chrome, mounted ONCE for every signed-in page.
 *
 * It is a layout and nothing renders a second one, because a tab row that is torn
 * down and rebuilt cannot animate — it can only reappear somewhere else. That is
 * also why neither the tabs nor the owner are passed in: a page being replaced
 * beneath the shell cannot hand it anything. The shell reads the URL and decides
 * for itself, which it can do because the names the namespace has spent are not
 * legal repo names. All this server component contributes is what the URL cannot
 * say: who is signed in, which namespaces they can act in, and what is in them.
 *
 * Chrome never gains a third row: anything deeper navigates inside the content.
 */
export async function AppShell({
  session,
  children,
}: {
  session: NonNullable<Session>;
  children: React.ReactNode;
}) {
  const me = session.user.owner;
  const owners = await ownersFor(session);
  // Every repo the person can jump to, for ⌘K. One list call per namespace, on
  // a full render only — client navigations keep this layout mounted.
  // ponytail: N calls per hard load; a single cross-owner list endpoint when teams grow
  const token = await apiToken();
  const lists = token ? await Promise.all(owners.map((o) => listRepos(token, o.slug))) : [];
  const repos = lists.flatMap((r) => (r.ok ? r.value : []));

  return (
    <ShellState>
      <div className="flex h-screen flex-col">
        {/* Chrome is a flex sibling of the scroll region, not sticky inside it: the
            header never scrolls, and the scrollbar belongs to the content alone. */}
        <header className="shrink-0 border-b border-border bg-card">
          <div className="mx-auto flex h-14 max-w-page items-center gap-3 px-6">
            <Link href="/" aria-label="kloudlite home" className="inline-flex">
              <Logo className="h-5" />
            </Link>
            <span className="text-muted-foreground/40" aria-hidden>/</span>

            <ShellCrumb me={me} owners={owners} />

            <div className="flex-1" />

            <GlobalSearch me={me} owners={owners} repos={repos} />
            <UserMenu name={session.user.name} email={session.user.email} />
          </div>

          <ShellTabs
            repoTabs={REPO_TABS}
            imageTabs={IMAGE_TABS}
            me={me}
            className="mx-auto max-w-page px-5"
          />
        </header>

        <ScrollArea className="flex-1">{children}</ScrollArea>
      </div>
    </ShellState>
  );
}
```

- [ ] **Step 3: Rewrite `global-search.tsx`**

```tsx
"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { Globe, Lock, Package, Search, SquareCode } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Kbd } from "@/components/ui/kbd";
import {
  CommandDialog,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandSeparator,
} from "@/components/ui/command";
import { sections, settingsSection } from "@/components/app/sections";
import { useOwner } from "@/components/app/shell-nav";
import type { SwitcherOwner } from "@/components/app/team-switcher";
import type { ApiRepo } from "@/lib/api";

/** ⌘K over everything in the current owner. One list, grouped by section, so the
 *  answer to "where is X" is the same keystroke regardless of what X turns out to
 *  be. Filtering is cmdk's — it scores against the item's text, so the item value
 *  carries the words someone would actually type, not just the display label.
 *
 *  Scope: what this owner has. It is a jump-to, not a content search — nothing
 *  here reads file contents, because no endpoint serves them yet. Only repos are
 *  listed: they are the one thing the api serves a list of. */
export function GlobalSearch({
  me,
  owners,
  repos,
}: {
  me: string;
  owners: SwitcherOwner[];
  /** Every repo across every owner; filtered to the owner in the URL here. */
  repos: ApiRepo[];
}) {
  const owner = useOwner(me);
  const [open, setOpen] = useState(false);
  const router = useRouter();

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        setOpen((v) => !v);
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, []);

  const go = (href: string) => {
    setOpen(false);
    router.push(href);
  };

  const mine = repos.filter((r) => r.owner === owner);

  return (
    <>
      <Button
        variant="outline"
        onClick={() => setOpen(true)}
        className="hidden w-64 justify-start border-edge font-normal text-muted-foreground hover:border-edge-hover hover:text-foreground md:flex"
      >
        <Search />
        Search
        <Kbd className="ml-auto">⌘K</Kbd>
      </Button>

      <CommandDialog open={open} onOpenChange={setOpen} title="Search" description="Jump to anything in this team">
        <CommandInput placeholder="Search repos…" />
        <CommandList>
          <CommandEmpty>Nothing matches that.</CommandEmpty>

          {mine.length > 0 && (
            <CommandGroup heading="Code Repos">
              {mine.map((r) => (
                <CommandItem key={r._id} value={`repo ${r.name} ${r.description}`} onSelect={() => go(`/${owner}/${r.name}`)}>
                  <SquareCode /> {r.name}
                  <span className="ml-auto flex items-center gap-1 text-caption text-muted-foreground">
                    {r.public ? <Globe className="size-3" /> : <Lock className="size-3" />}
                    {r.public ? "public" : "private"}
                  </span>
                </CommandItem>
              ))}
            </CommandGroup>
          )}

          <CommandSeparator />

          {owners.length > 1 && (
            <CommandGroup heading="Switch to">
              {owners.filter((o) => o.slug !== owner).map((o) => (
                <CommandItem key={o.slug} value={`team ${o.slug} ${o.name}`} onSelect={() => go(`/${o.slug}`)}>
                  <Package /> {o.slug}
                </CommandItem>
              ))}
            </CommandGroup>
          )}

          <CommandGroup heading="Go to">
            {[...sections(owner), settingsSection(owner)].map(({ href, label, icon: Icon }) => (
              <CommandItem key={href} value={`go ${label}`} onSelect={() => go(href)}>
                <Icon /> {label}
              </CommandItem>
            ))}
          </CommandGroup>
        </CommandList>
      </CommandDialog>
    </>
  );
}
```

- [ ] **Step 4: Verify**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean. (`lib/mock.ts` still has importers in `team-*.tsx`; they go in Task 4.)
Manual: on `/{team}/registries` the crumb, switcher and tabs should all name the team; ⌘K lists that team's real repos.

- [ ] **Step 5: Commit**

```bash
git add web/apps/web/src/components/app/shell-nav.tsx web/apps/web/src/components/app/app-shell.tsx web/apps/web/src/components/app/global-search.tsx
git commit -m "Read the owner from the URL in the shell and search real repos"
```

---

### Task 4: Team pages work for every team and say what does not exist yet

**Files:**
- Create: `web/apps/web/src/components/app/not-yet.tsx`
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/settings/page.tsx`
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/ci/page.tsx`
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/environments/page.tsx`
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/workspaces/page.tsx`
- Modify: `web/apps/web/src/components/app/user-settings.tsx:1-61` (Profile section loses its form)
- Modify: `web/apps/web/src/app/(shell)/settings/actions.ts:11-14` (delete `updateProfile`)
- Delete: `web/apps/web/src/app/(shell)/[owner]/(org)/settings/actions.ts`, `src/components/app/team-settings.tsx`, `team-triggers.tsx`, `team-environments.tsx`, `team-workspaces.tsx`, `declared-list.tsx`, `src/lib/mock.ts`

**Context:** Four org pages do `if (owner !== session.user.owner) notFound()`, so every team page 404s. The `(org)/layout.tsx` already redirects the signed-out, and `(org)/page.tsx` documents the rule: the api answers 404 for a namespace the caller may not act in, so asking it IS the check. These four pages ask nothing — they render mock rows (`MEMBERS`, `TRIGGERS`, …) and no-op actions (`updateTeam`, `inviteMember`, `updateProfile`) with Save/Invite buttons, `<a href="#">Open</a>`, and filter inputs with no handler. We are in implementation phase: drop the check, replace each with an explicit empty state, and delete the mocks. `lib/mock.ts`'s last importer (`global-search.tsx`) went in Task 3.

**Interfaces:**
- Produces: `NotYet({ title, children })` in `@/components/app/not-yet` — a titled empty state, reused by Task 5 for Issues.

- [ ] **Step 1: Create `components/app/not-yet.tsx`**

```tsx
/** A page for something that does not exist yet, saying so. Honest and blank beats
 *  a mock-up that invites clicks which go nowhere. */
export function NotYet({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section>
      <h1 className="text-title font-semibold tracking-title">{title}</h1>
      <p className="mt-6 border border-border bg-card px-5 py-14 text-center text-sm2 text-muted-foreground">
        {children}
      </p>
    </section>
  );
}
```

- [ ] **Step 2: Replace the four org pages**

`(org)/settings/page.tsx`:
```tsx
import type { Metadata } from "next";
import { NotYet } from "@/components/app/not-yet";

export const metadata: Metadata = { title: "Team settings" };

/** Membership is not checked here — see `(org)/page.tsx`: the api decides who may
 *  act in a namespace, and there is nothing on this page to ask it about yet. */
export default async function SettingsPage({ params }: { params: Promise<{ owner: string }> }) {
  const { owner } = await params;
  return (
    <NotYet title="Team settings">
      Renaming {owner}, inviting members and deleting the team are not available yet.
    </NotYet>
  );
}
```

`(org)/ci/page.tsx`:
```tsx
import { NotYet } from "@/components/app/not-yet";

export default function Page() {
  return <NotYet title="CI Triggers">CI triggers are not available yet.</NotYet>;
}
```

`(org)/environments/page.tsx`:
```tsx
import { NotYet } from "@/components/app/not-yet";

export default function Page() {
  return <NotYet title="Environments">Environments are not available yet.</NotYet>;
}
```

`(org)/workspaces/page.tsx`:
```tsx
import { NotYet } from "@/components/app/not-yet";

export default function Page() {
  return <NotYet title="Workspaces">Workspaces are not available yet.</NotYet>;
}
```

- [ ] **Step 3: Delete the mock components, their actions, and `lib/mock.ts`**

```bash
cd web/apps/web
git rm "src/app/(shell)/[owner]/(org)/settings/actions.ts" \
  src/components/app/team-settings.tsx src/components/app/team-triggers.tsx \
  src/components/app/team-environments.tsx src/components/app/team-workspaces.tsx \
  src/components/app/declared-list.tsx src/lib/mock.ts
```

- [ ] **Step 4: Make the Profile section read-only**

`web/apps/web/src/app/(shell)/settings/actions.ts`: delete `updateProfile` (lines 11–14).

`web/apps/web/src/components/app/user-settings.tsx`: line 10 becomes `import { removeSshKey, revokeToken } from "@/app/(shell)/settings/actions";` and the Profile section (lines 43–61) becomes:

```tsx
          <Section title="Profile" description="How you appear to your teams. Your name and email come from the identity you signed in with; changing them here is not available yet.">
            <dl className="grid max-w-md gap-5">
              <div className="grid gap-2">
                <dt className="text-sm2 font-medium">Name</dt>
                <dd className="flex h-9 items-center border border-input bg-muted/40 px-2.5 text-sm2 text-muted-foreground">{session.user.name}</dd>
              </div>
              <div className="grid gap-2">
                <dt className="text-sm2 font-medium">Email</dt>
                <dd className="flex h-9 items-center border border-input bg-muted/40 px-2.5 text-sm2 text-muted-foreground">{session.user.email}</dd>
              </div>
              <div className="grid gap-2">
                <dt className="text-sm2 font-medium">Handle</dt>
                <dd className="flex h-9 items-center border border-input bg-muted/40 px-2.5 font-mono text-sm2 text-muted-foreground">
                  @<span className="text-foreground">{session.user.owner}</span>
                </dd>
              </div>
            </dl>
          </Section>
```

Remove the now-unused imports `Input` (line 5) and `FieldLabel` (line 6) if nothing else in the file uses them (nothing does — check with `bun run lint`).

- [ ] **Step 5: Verify**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean. `grep -rn "lib/mock\"" web/apps/web/src` → no hits.

- [ ] **Step 6: Commit**

```bash
git add -A web/apps/web/src
git commit -m "Serve team pages for any owner and replace mock team data with empty states"
```

---

### Task 5: Delete the mock Compare route and Issues list

**Files:**
- Delete: `web/apps/web/src/app/(shell)/[owner]/[repo]/compare/page.tsx`, `src/components/repo/compare.tsx`, `src/components/repo/issues.tsx`, `src/lib/mock-repo.ts`
- Modify: `web/apps/web/src/app/(shell)/[owner]/[repo]/issues/page.tsx`

**Context:** `compare/` duplicates `pulls/new` with a form that has no action and mock commits; nothing links to it (`grep -rn "/compare" src` finds only the api call in `lib/api.ts`). `issues.tsx` renders `ISSUES` from `lib/mock-repo.ts` against a hard-coded `kloudlite` repo. The Issues tab stays in `REPO_TABS`; the page says what it is.

- [ ] **Step 1: Delete**

```bash
cd web/apps/web
git rm "src/app/(shell)/[owner]/[repo]/compare/page.tsx" src/components/repo/compare.tsx src/components/repo/issues.tsx src/lib/mock-repo.ts
```

- [ ] **Step 2: Replace the issues page**

`web/apps/web/src/app/(shell)/[owner]/[repo]/issues/page.tsx`:
```tsx
import { NotYet } from "@/components/app/not-yet";
import { guardRepo } from "@/app/(shell)/[owner]/[repo]/guard";

export default async function Page({ params }: { params: Promise<{ owner: string; repo: string }> }) {
  const { owner, repo } = await params;
  await guardRepo(owner, repo);
  return <NotYet title="Issues">Issues are not available yet.</NotYet>;
}
```

- [ ] **Step 3: Verify**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean. `grep -rn "mock-repo" web/apps/web/src` → no hits.

- [ ] **Step 4: Commit**

```bash
git add -A web/apps/web/src
git commit -m "Remove the mock compare route and issues list"
```

---

### Task 6: Stop offering "Rebase and merge"

**Files:**
- Modify: `web/apps/web/src/components/repo/pull-actions.tsx:85-96`

**Context:** `pulls/actions.ts:57-59` maps any strategy other than `squash`/`merge` to `fast-forward`, so choosing "Rebase and merge" silently fast-forwards. `api.MergeStrategy` has no rebase. Remove the entry.

- [ ] **Step 1: Delete the rebase entry and its label branch**

Remove lines 85–89 (the `{ value: "rebase", … }` object) and line 95 (`: strategy === "rebase" ? "Rebase and merge"`). The `label` expression becomes:

```tsx
  const label =
    strategy === "squash" ? "Squash and merge"
    : strategy === "merge" ? "Create a merge commit"
    : "Merge pull request";
```

- [ ] **Step 2: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean.

```bash
git add web/apps/web/src/components/repo/pull-actions.tsx
git commit -m "Drop the rebase merge option the server does not implement"
```

---

## MEDIUM

### Task 7: Browse a repo at a commit (`?ref=<oid>`)

**Files:**
- Modify: `web/apps/web/src/lib/browse.ts` (add `Head`, `resolveRef`)
- Modify: `web/apps/web/src/components/repo/code.tsx:87-88,127`
- Modify: `web/apps/web/src/components/repo/file-view.tsx:36-38,55`
- Modify: `web/apps/web/src/app/(shell)/[owner]/[repo]/edit/[...path]/page.tsx:20-22`
- Modify: `web/apps/web/src/components/repo/pull-commits.tsx:76-78`

**Context:** `pull-commits.tsx:77` links to `${base}/tree?ref=<oid>` — `/tree` with no path is not a route (404), and `CodeView` only matches `?ref=` against branch/tag names, so an oid falls back to the default branch. Every browse call is keyed by oid already, so accepting a 40-hex `ref` is one resolution step. It is shared by the three places that resolve `?ref=` so a file link clicked while browsing at a commit keeps the commit.

**Interfaces:**
- Produces in `@/lib/browse`: `type Head = { name: string; oid: string; kind: "branch" | "tag" | "commit" }`, `resolveRef(all: Ref[], refName?: string): Head | undefined`.

- [ ] **Step 1: Add `resolveRef` to `lib/browse.ts`**

After `shortOid` (around line 140):

```ts
/** What `?ref=` resolved to. A `commit` is a bare oid: browsable like a branch,
 *  but nothing can be committed onto it and nothing names it. */
export type Head = { name: string; oid: string; kind: "branch" | "tag" | "commit" };

/** The ref a page opens on: the named branch or tag if it exists, a commit if the
 *  name is an oid, else the default branch. An unknown NAME falls back rather than
 *  404s — a branch can be deleted while someone still holds the link. */
export function resolveRef(all: Ref[], refName?: string): Head | undefined {
  if (refName) {
    const named = all.find((r) => shortRef(r.name) === refName);
    if (named) return named;
    if (/^[0-9a-f]{40}$/.test(refName)) return { name: refName, oid: refName, kind: "commit" };
  }
  return defaultBranch(all);
}
```

- [ ] **Step 2: Use it in `code.tsx`**

Line 13's import list gains `resolveRef` and `shortOid` is already there. Replace line 88:
```ts
  const head = resolveRef(all.value, refName);
```
Replace line 127 (the RefPicker `current`):
```tsx
            current={head.kind === "commit" ? shortOid(head.oid) : shortRef(head.name)}
```

- [ ] **Step 3: Use it in `file-view.tsx`**

Line 9 import gains `resolveRef, shortOid`. Replace line 37:
```ts
  const head = resolveRef(all.value, refName);
```
Replace line 55:
```tsx
            current={head.kind === "commit" ? shortOid(head.oid) : shortRef(head.name)}
```
(Line 94's `head.kind === "branch"` already hides Edit for a commit.)

- [ ] **Step 4: Use it in the edit page**

`edit/[...path]/page.tsx` line 4 import gains `resolveRef`; replace line 21:
```ts
  const head = resolveRef(all.value, ref);
```
Line 27's `if (head.kind !== "branch") redirect(...)` now also covers a commit.

- [ ] **Step 5: Fix the link in `pull-commits.tsx`**

Line 78:
```tsx
                    href={`${base}?ref=${c.oid}`}
```
(An oid is hex; no encoding needed.)

- [ ] **Step 6: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean. Manual: on a PR's Commits tab, the `<>` button opens the tree at that commit and the ref picker shows the short oid.

```bash
git add web/apps/web/src/lib/browse.ts web/apps/web/src/components/repo/code.tsx web/apps/web/src/components/repo/file-view.tsx "web/apps/web/src/app/(shell)/[owner]/[repo]/edit/[...path]/page.tsx" web/apps/web/src/components/repo/pull-commits.tsx
git commit -m "Browse a repo at a commit when ?ref= is an oid"
```

---

### Task 8: Make the changed-files tree actually jump to the file

**Files:**
- Modify: `web/apps/web/src/components/repo/diff-files.tsx:21-27`
- Modify: `web/apps/web/src/components/repo/pull-files.tsx:47-48`

**Context:** `pull-files.tsx:48` links to `#${n.path}` but no element carries that id. Give each `<details>` the path as its id, with `scroll-mt` so the sticky header does not cover it. The href is percent-encoded (a path can contain spaces); the browser decodes the fragment before matching ids.

- [ ] **Step 1: Add the id in `diff-files.tsx`**

Lines 21–27 become:
```tsx
            <details
              key={f.path}
              id={f.path}
              open={!big}
              className="group min-w-0 scroll-mt-24 overflow-hidden border border-border bg-card"
            >
```

- [ ] **Step 2: Encode the anchor in `pull-files.tsx`**

Line 48:
```tsx
              href={`#${encodeURIComponent(n.path)}`}
```

- [ ] **Step 3: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean.

```bash
git add web/apps/web/src/components/repo/diff-files.tsx web/apps/web/src/components/repo/pull-files.tsx
git commit -m "Anchor the changed-files tree to the diffs it lists"
```

---

### Task 9: An error page and a loading state inside the shell

**Files:**
- Create: `web/apps/web/src/app/(shell)/error.tsx`
- Create: `web/apps/web/src/app/(shell)/[owner]/[repo]/loading.tsx`

**Context:** No `error.tsx` anywhere, so an api outage (`throw new Error(r.message)` in many pages) shows Next's raw error page. `error.tsx` must be a client component and receives `{ error, reset }`. Placed under `(shell)` it renders INSIDE the shell layout, so the chrome survives. `loading.tsx` under the repo route gives the browse pages (several sequential api calls) a skeleton instead of a blank frame. Copy `not-found.tsx`'s typography.

- [ ] **Step 1: Create `(shell)/error.tsx`**

```tsx
"use client";

import { Button } from "@/components/ui/button";

/** What a page shows when it threw. Every browse page throws the api's message
 *  when a call fails for a reason that is not "sign in" or "not found", so this is
 *  mostly "the service is unavailable" — which is why there is a retry and no
 *  stack trace. Client component by Next's rule, not by choice. */
export default function ShellError({ error, reset }: { error: Error & { digest?: string }; reset: () => void }) {
  return (
    <main className="mx-auto max-w-page px-6 pt-16 pb-16">
      <div className="w-full max-w-auth">
        <p className="text-caption font-semibold uppercase tracking-eyebrow text-muted-foreground">
          Something went wrong
        </p>
        <h1 className="mt-3 text-title font-semibold tracking-title">This page could not be loaded.</h1>
        <p className="mt-2 text-sm2 leading-relaxed text-muted-foreground">
          {error.message || "The service is unavailable. Try again."}
        </p>
        <Button onClick={reset} variant="outline" className="mt-6 border-edge hover:border-edge-hover">
          Try again
        </Button>
      </div>
    </main>
  );
}
```

- [ ] **Step 2: Create `[owner]/[repo]/loading.tsx`**

```tsx
/** Shown while a repo page's api calls are in flight. Blocks the shape of the
 *  code view — toolbar, crumb, listing — so the page does not jump when it lands. */
export default function Loading() {
  return (
    <div className="animate-pulse" aria-busy="true" aria-label="Loading">
      <div className="flex items-center gap-3">
        <div className="h-8 w-36 bg-muted" />
        <div className="h-8 w-64 bg-muted" />
        <div className="ml-auto h-8 w-24 bg-muted" />
      </div>
      <div className="mt-5 h-5 w-48 bg-muted" />
      <div className="mt-3 border border-border bg-card">
        {Array.from({ length: 8 }, (_, i) => (
          <div key={i} className="flex items-center gap-4 border-b border-border px-4 py-3 last:border-b-0">
            <div className="size-4 bg-muted" />
            <div className="h-4 w-40 bg-muted" />
            <div className="ml-auto h-3 w-16 bg-muted" />
          </div>
        ))}
      </div>
    </div>
  );
}
```

- [ ] **Step 3: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean. Manual: stop the api (or point `KLOUDLITE_GIT_API_URL` at a dead port), open a repo — the error page renders inside the shell with a working "Try again".

```bash
git add "web/apps/web/src/app/(shell)/error.tsx" "web/apps/web/src/app/(shell)/[owner]/[repo]/loading.tsx"
git commit -m "Add an error boundary and a repo loading state inside the shell"
```

---

### Task 10: A missing pull, blob or commit is a 404, not a 500

**Files:**
- Modify: `web/apps/web/src/app/(shell)/[owner]/[repo]/pulls/[number]/pull-data.ts:1,19-20`
- Modify: `web/apps/web/src/components/repo/file-view.tsx:1,45`
- Modify: `web/apps/web/src/components/repo/diff.tsx:1,35`

**Context:** Each does `if (!r.ok) throw new Error(r.message)`, turning the api's `notFound` into a 500. `notFound()` from `next/navigation` renders `not-found.tsx`.

- [ ] **Step 1: `pull-data.ts`**

Add `import { notFound } from "next/navigation";` at the top. Replace line 20:
```ts
    if (!pull.ok) {
      if (pull.kind === "notFound") notFound();
      throw new Error(pull.message);
    }
```

- [ ] **Step 2: `file-view.tsx`**

Add `import { notFound } from "next/navigation";`. Replace line 45:
```ts
  if (!b.ok) {
    // A path that is not in this tree is a 404, same as a repo that is not here.
    if (b.kind === "notFound") notFound();
    throw new Error(b.message);
  }
```

- [ ] **Step 3: `diff.tsx`**

Add `import { notFound } from "next/navigation";`. Replace line 35:
```ts
  if (!r.ok) {
    if (r.kind === "notFound") notFound();
    throw new Error(r.message);
  }
```

- [ ] **Step 4: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean. Manual: `/{owner}/{repo}/blob/does-not-exist` and `/{owner}/{repo}/pulls/99999` show the 404 page.

```bash
git add "web/apps/web/src/app/(shell)/[owner]/[repo]/pulls/[number]/pull-data.ts" web/apps/web/src/components/repo/file-view.tsx web/apps/web/src/components/repo/diff.tsx
git commit -m "Render missing pulls, blobs and commits as 404"
```

---

### Task 11: One `pathHref` for every file path in a URL

**Files:**
- Modify: `web/apps/web/src/lib/utils.ts` (add `pathHref`)
- Modify: `web/apps/web/src/components/repo/code.tsx:187` (and the crumb links at 111, 150)
- Modify: `web/apps/web/src/components/repo/file-view.tsx:72,96`
- Modify: `web/apps/web/src/components/repo/file-editor.tsx:127`
- Modify: `web/apps/web/src/components/repo/diff-files.tsx:31`
- Modify: `web/apps/web/src/components/repo/file-search.tsx:73`
- Modify: `web/apps/web/src/app/(shell)/[owner]/[repo]/edit/[...path]/page.tsx:27,35`
- Modify: `web/apps/web/src/app/(shell)/[owner]/[repo]/edit/actions.ts:57`

**Context:** Seven sites interpolate a file path into an href raw, so `a b.md`, `#`, `?` or `%` in a filename produce a wrong link. One helper, mirroring `filePath` in `lib/browse.ts:56` (every segment escaped, slashes kept). The spec names `lib/browse.ts` as the home, but that module is `server-only` and `file-editor.tsx`/`file-search.tsx` are client components — so it lives in `lib/utils.ts`, which both sides already import.

**Interfaces:**
- Produces: `pathHref(path: string): string` in `@/lib/utils` — `"a b/c#d.md"` → `"a%20b/c%23d.md"`.

- [ ] **Step 1: Add the helper to `lib/utils.ts`**

```ts
/** A repo path as URL segments: every segment escaped, the slashes kept. The one
 *  way a file name becomes part of an href, so a `#` or a space in a filename is a
 *  file and not a fragment. Mirrors `filePath` in `lib/browse.ts`, which does the
 *  same for api calls and is server-only. */
export function pathHref(path: string): string {
  return path.split("/").filter(Boolean).map(encodeURIComponent).join("/");
}
```

- [ ] **Step 2: Apply it at each site**

Each file imports `pathHref` from `@/lib/utils` (several already import `cn` from there — extend that import).

`code.tsx`:
- line 111: `` (crumbs.length > 1 ? `${base}/tree/${pathHref(crumbs.slice(0, -1).join("/"))}` : base) + q ``
- line 131: `` base={dir ? `${base}/tree/${pathHref(dir)}` : base} ``
- line 150: `` href={`${base}/tree/${pathHref(crumbs.slice(0, i + 1).join("/"))}${q}`} ``
- line 187: `` href={`${base}/${e.kind === "tree" ? "tree" : "blob"}/${pathHref(path)}${q}`} ``

`file-view.tsx`:
- line 72: `` href={`${base}/tree/${pathHref(parts.slice(0, i + 1).join("/"))}${q}`} ``
- line 96: `` <Link href={`${base}/edit/${pathHref(path)}?ref=${encodeURIComponent(shortRef(head.name))}`}> ``

`file-editor.tsx` line 127:
```tsx
              <Link href={`/${owner}/${repo}/blob/${pathHref(path)}?ref=${encodeURIComponent(branch)}`}>Cancel</Link>
```

`diff-files.tsx` line 31:
```tsx
                <Link href={`${base}/blob/${pathHref(f.path)}`} className="truncate font-mono font-medium underline-offset-4 hover:underline">
```

`file-search.tsx` line 73:
```ts
    router.push(`${base}/${e.kind === "dir" ? "tree" : "blob"}/${pathHref(e.path)}`);
```

`edit/[...path]/page.tsx` lines 27 and 35: replace `${file}` with `${pathHref(file)}` in both `redirect(...)` calls.

`edit/actions.ts` line 57:
```ts
  redirect(`/${owner}/${repo}/blob/${pathHref(path)}?ref=${encodeURIComponent(landed)}`);
```

- [ ] **Step 3: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean. Manual: a file named `a b.txt` opens from the tree, from go-to-file, and from a diff header.

```bash
git add web/apps/web/src/lib/utils.ts web/apps/web/src/components/repo "web/apps/web/src/app/(shell)/[owner]/[repo]/edit"
git commit -m "Escape file paths everywhere they become an href"
```

---

### Task 12: The login form only offers what works

**Files:**
- Modify: `web/apps/web/src/app/(auth)/login/actions.ts` (whole file)
- Modify: `web/apps/web/src/components/auth/login-form.tsx` (whole file)
- Modify: `web/apps/web/src/app/(auth)/login/page.tsx` (read `?from=expired`)
- Delete: `web/apps/web/src/lib/sso.ts`

**Context:** The SSO step renders a "Continue to {org}" button with no handler, the password step links to `/reset` (404), and the password step is shown even when `passwordSignIn` is false (the error only appears after typing one). `?from=expired` is set in eight places and read nowhere. `lib/sso.ts` exists only to feed the SSO branch and `AUTH_SSO_DOMAINS` is set in no yaml — delete it rather than hide dead code.

**Interfaces:**
- `LoginState` loses the `sso` variant.

- [ ] **Step 1: Rewrite `login/actions.ts`**

```ts
"use server";

import { AuthError } from "next-auth";
import { signIn, passwordSignIn } from "@/auth";

export type LoginState =
  | { step: "email"; error?: string }
  | { step: "password"; email: string; error?: string };

/** Step one: we have an email and nothing else. Refuse here, not after a password
 *  has been typed, when the deployment has no password provider at all. */
export async function continueWithEmail(
  _prev: LoginState,
  formData: FormData,
): Promise<LoginState> {
  const email = String(formData.get("email") ?? "").trim();

  if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email)) {
    return { step: "email", error: "Enter a valid email address." };
  }
  if (!passwordSignIn) {
    return { step: "email", error: "Password sign-in is not available here. Use a provider or a passkey above." };
  }
  return { step: "password", email };
}

/** Step two, password path. On success `signIn` redirects, which it does by
 *  throwing — so only an AuthError is caught here, never the redirect. */
export async function signInWithPassword(
  _prev: LoginState,
  formData: FormData,
): Promise<LoginState> {
  const email = String(formData.get("email") ?? "");
  const password = String(formData.get("password") ?? "");
  if (password.length < 1) {
    return { step: "password", email, error: "Enter your password." };
  }
  if (!passwordSignIn) {
    return { step: "email", error: "Password sign-in is not available here. Use a provider or a passkey above." };
  }
  try {
    await signIn("credentials", { email, password, redirectTo: "/" });
  } catch (error) {
    if (error instanceof AuthError) {
      // Deliberately does not say which half was wrong.
      return { step: "password", email, error: "Incorrect email or password." };
    }
    throw error;
  }
  return { step: "password", email };
}
```

- [ ] **Step 2: Rewrite `login-form.tsx`**

```tsx
"use client";

import { useActionState } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { AuthHeader, FieldLabel } from "@/components/auth/auth-card";
import { continueWithEmail, signInWithPassword, type LoginState } from "@/app/(auth)/login/actions";

function FieldError({ children }: { children?: string }) {
  if (!children) return null;
  return (
    <p role="alert" className="text-sm2 font-medium text-destructive">
      {children}
    </p>
  );
}

/** Owns the whole card, heading included. The heading names the step, so it
 *  cannot live on the page — a page-level <h1> would sit above this one and
 *  still say "Sign in" while the card asks for a password. */
export function LoginForm({
  oauth,
  notice,
  title = "Sign in to kloudlite",
  subtitle = "Continue to your workspaces and repos.",
  submitLabel = "Continue",
}: {
  oauth?: React.ReactNode;
  /** Why the person is here, when it was not their idea — an expired session. */
  notice?: string;
  title?: string;
  subtitle?: string;
  submitLabel?: string;
}) {
  const [state, submitEmail, emailPending] = useActionState<LoginState, FormData>(
    continueWithEmail,
    { step: "email" },
  );
  const [pwState, submitPassword, pwPending] = useActionState<LoginState, FormData>(
    signInWithPassword,
    state,
  );

  // The email step decides the route; the password step only ever follows it.
  const current = pwState.step === "password" && state.step === "password" ? pwState : state;

  if (current.step === "password") {
    return (
      <div>
        <AuthHeader title="Enter your password" />

        {/* The identity being signed in as, and the way back out of it. One row,
            one baseline — not a sentence with a button wrapped inside it. */}
        <div className="flex items-center justify-between gap-4 border border-border bg-muted/40 px-3.5 py-2.5">
          <span className="truncate text-sm2 font-medium">{current.email}</span>
          <form action={submitEmail}>
            <Button type="submit" name="email" value="" variant="link" className="h-auto p-0 text-sm2">
              Change
            </Button>
          </form>
        </div>

        <form action={submitPassword} className="mt-5 grid gap-2">
          <input type="hidden" name="email" value={current.email} />
          <FieldLabel htmlFor="password">Password</FieldLabel>
          <Input
            id="password"
            name="password"
            type="password"
            autoComplete="current-password"
            autoFocus
            className="h-10"
            required
          />
          <FieldError>{current.error}</FieldError>
          <Button type="submit" disabled={pwPending} size="lg" className="mt-3 w-full">
            {pwPending && <Loader2 className="size-4 animate-spin" />}
            Sign in
          </Button>
        </form>
      </div>
    );
  }

  return (
    <div>
      <AuthHeader title={title}>{subtitle}</AuthHeader>

      {notice && (
        <p role="status" className="mb-5 border border-border bg-muted/40 px-3.5 py-2.5 text-sm2 text-muted-foreground">
          {notice}
        </p>
      )}

      {oauth}

      <form action={submitEmail} className="grid gap-2">
        <FieldLabel htmlFor="email">Email</FieldLabel>
        <Input
          id="email"
          name="email"
          type="email"
          autoComplete="email"
          placeholder="you@company.com"
          className="h-10"
          required
        />
        <FieldError>{current.error}</FieldError>
        <Button type="submit" disabled={emailPending} size="lg" className="mt-3 w-full">
          {emailPending && <Loader2 className="size-4 animate-spin" />}
          {submitLabel}
        </Button>
      </form>
    </div>
  );
}
```

(Check `FieldLabel`'s `aside` prop in `auth-card.tsx` is still used elsewhere — `grep -rn "aside=" src` — if not, leave it; it is not this task's to remove.)

- [ ] **Step 3: Read `?from=expired` on the login page**

`login/page.tsx` — the component signature and body become:

```tsx
export default async function LoginPage({ searchParams }: { searchParams: Promise<{ from?: string }> }) {
  const session = await getSession();
  // A signed-in person landing here means "take me in", not "sign in again" —
  // and if they have no handle yet, in means /welcome.
  if (session) redirect(session.user.username ? "/" : "/welcome");
  const { from } = await searchParams;

  return (
    <>
      <AuthCard>
        <LoginForm
          oauth={<AuthProviders verb="Sign in" />}
          notice={from === "expired" ? "Your session expired. Sign in again to continue." : undefined}
        />
      </AuthCard>
      ...
```
(Rest of the file unchanged.) Check whether `signup/page.tsx` also renders `LoginForm`; if so it needs no change — `notice` is optional.

- [ ] **Step 4: Delete `lib/sso.ts`**

```bash
git rm web/apps/web/src/lib/sso.ts
```

- [ ] **Step 5: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean. Manual: with `AUTH_SHARED_PASSWORD` unset, entering an email shows the "not available" error on the email step; `/login?from=expired` shows the notice.

```bash
git add -A web/apps/web/src/app/\(auth\) web/apps/web/src/components/auth/login-form.tsx web/apps/web/src/lib/sso.ts
git commit -m "Show only working sign-in paths and explain an expired session"
```

---

### Task 13: Destructive actions report failure (and deleting a tag asks first)

**Files:**
- Create: `web/apps/web/src/components/app/delete-form.tsx`
- Modify: `web/apps/web/src/app/(shell)/[owner]/(org)/registries/[image]/settings/actions.ts:13-21` (`removeTag`)
- Modify: `web/apps/web/src/app/(shell)/settings/actions.ts` (`removeSshKey`, `revokeToken`)
- Modify: `web/apps/web/src/app/(auth)/passkey/actions.ts:147-153` (`removePasskey`)
- Modify: `web/apps/web/src/app/(shell)/[owner]/[repo]/settings/actions.ts:60-68` (`removeRule`)
- Modify: `web/apps/web/src/app/(shell)/[owner]/[repo]/pulls/actions.ts:66-74` (`close`)
- Modify: call sites: `src/components/registry/image-settings.tsx:42-54`, `src/components/app/user-settings.tsx:105-110,144-149,180-185`, `src/components/app/passkeys-section.tsx:77-82`, `src/components/repo/repo-settings.tsx:92-98`, `src/components/repo/pull-actions.tsx:200-205`

**Context:** Six actions `await api.X(...)` and return nothing, so a failed delete is silent. `destroyImage` already shows the pattern: `(prev, formData) => Promise<{ error? } | null>` driven by `useActionState`. The forms live in both server components (`user-settings.tsx`) and client ones, so one small client `DeleteForm` holds the `useActionState` and the error line; callers keep their button. The `confirm` prop is a native `window.confirm` — used for `removeTag` only, per the review.

**Interfaces:**
- Produces: `DeleteForm({ action, fields, confirm?, className?, children })` in `@/components/app/delete-form`, with `type DeleteState = { error?: string } | null` exported from the same file. `action: (prev: DeleteState, formData: FormData) => Promise<DeleteState>`.
- The six actions change signature to `(_prev: DeleteState, formData: FormData): Promise<DeleteState>`. (`removeRule` keeps returning `SettingsState`, which is structurally compatible.)

- [ ] **Step 1: Create `components/app/delete-form.tsx`**

```tsx
"use client";

import { useActionState } from "react";

export type DeleteState = { error?: string } | null;

/** A one-button form that can fail. The six destructive actions used to return
 *  nothing, so a refused delete looked like a click that did not register. This
 *  holds the action state so a server component can still render the row.
 *
 *  `confirm` is the browser's own dialog: a delete that cannot be undone gets one
 *  question, and a custom modal would be a component for one sentence. */
export function DeleteForm({
  action,
  fields,
  confirm,
  className,
  children,
}: {
  action: (prev: DeleteState, formData: FormData) => Promise<DeleteState>;
  /** Hidden inputs: what the action is about has to travel with the request. */
  fields: Record<string, string>;
  confirm?: string;
  className?: string;
  children: React.ReactNode;
}) {
  const [state, act, pending] = useActionState<DeleteState, FormData>(action, null);
  return (
    <form
      action={act}
      onSubmit={(e) => {
        if (confirm && !window.confirm(confirm)) e.preventDefault();
      }}
      className={className}
    >
      {Object.entries(fields).map(([name, value]) => (
        <input key={name} type="hidden" name={name} value={value} />
      ))}
      {state?.error && (
        <p role="alert" className="mr-3 inline text-caption font-medium text-destructive">{state.error}</p>
      )}
      {/* `contents` so the fieldset adds no box; `disabled` so the button goes
          inert while the request is out without each caller wiring `pending`. */}
      <fieldset disabled={pending} className="contents">{children}</fieldset>
    </form>
  );
}
```

- [ ] **Step 2: Change the six actions**

`registries/[image]/settings/actions.ts` — `removeTag` becomes:
```ts
export async function removeTag(_prev: SettingsState, formData: FormData): Promise<SettingsState> {
  const owner = String(formData.get("owner") ?? "");
  const image = String(formData.get("image") ?? "");
  const tag = String(formData.get("tag") ?? "");
  if (!tag) return { error: "No tag named." };
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };
  const r = await deleteImageTag(token, owner, image, tag);
  if (!r.ok) return { error: r.message || "Could not delete the tag." };
  revalidatePath(`/${owner}/registries/${image}`, "layout");
  return null;
}
```

`(shell)/settings/actions.ts` — add `export type DeleteState = { error?: string } | null;` after `AddKeyState`, then:
```ts
export async function removeSshKey(_prev: DeleteState, formData: FormData): Promise<DeleteState> {
  const id = String(formData.get("id") ?? "");
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };
  if (!id) return { error: "No key named." };
  const r = await api.removeKey(token, id);
  if (!r.ok) return { error: r.message || "Could not remove the key." };
  revalidatePath("/settings");
  return null;
}
```
and
```ts
export async function revokeToken(_prev: DeleteState, formData: FormData): Promise<DeleteState> {
  const id = String(formData.get("id") ?? "");
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };
  if (!id) return { error: "No token named." };
  const r = await api.revokeToken(token, id);
  if (!r.ok) return { error: r.message || "Could not revoke the token." };
  revalidatePath("/settings");
  return null;
}
```

`(auth)/passkey/actions.ts` — `removePasskey` becomes (add `import type { DeleteState } from "@/components/app/delete-form";` — a type import from a client module is erased and fine in a `"use server"` file):
```ts
export async function removePasskey(_prev: DeleteState, formData: FormData): Promise<DeleteState> {
  const id = String(formData.get("id") ?? "");
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };
  if (!id) return { error: "No passkey named." };
  const r = await api.removePasskey(token, id);
  if (!r.ok) return { error: r.message || "Could not remove the passkey." };
  revalidatePath("/settings");
  return null;
}
```

`[repo]/settings/actions.ts` — `removeRule`:
```ts
export async function removeRule(_prev: SettingsState, formData: FormData): Promise<SettingsState> {
  const owner = String(formData.get("owner") ?? "");
  const repo = String(formData.get("repo") ?? "");
  const pattern = String(formData.get("pattern") ?? "");
  if (!pattern) return { error: "No rule named." };
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };
  const r = await api.setProtection(token, owner, repo, { pattern, remove: true });
  if (!r.ok) return { error: r.message || "Could not remove the rule." };
  revalidatePath(`/${owner}/${repo}/settings`);
  return null;
}
```

`pulls/actions.ts` — `close` (the file's `PullState` is `{ error?: string } | null`, already the right shape):
```ts
export async function close(_prev: PullState, formData: FormData): Promise<PullState> {
  const owner = String(formData.get("owner") ?? "");
  const repo = String(formData.get("repo") ?? "");
  const number = Number(formData.get("number"));
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };
  const r = await api.closePull(token, owner, repo, number);
  if (!r.ok) return { error: r.message || "Could not close the change." };
  revalidatePath(`/${owner}/${repo}/pulls/${number}`);
  return null;
}
```

- [ ] **Step 3: Switch the call sites to `DeleteForm`**

Each file imports `DeleteForm` from `@/components/app/delete-form`.

`image-settings.tsx` lines 42–54:
```tsx
          <DeleteForm
            action={removeTag}
            fields={{ owner, image, tag: t.tag }}
            confirm={`Delete the tag ${t.tag}? The manifest it points at is kept.`}
          >
            <Button
              type="submit"
              variant="ghost"
              size="sm"
              className="text-muted-foreground hover:text-destructive"
              aria-label={`Delete the tag ${t.tag}`}
            >
              <Trash2 />
            </Button>
          </DeleteForm>
```
(`Which` is still used by `Danger`; leave it.)

`user-settings.tsx` — both `<form action={removeSshKey}>…</form>` blocks (lines 105–110 and 144–149) become:
```tsx
                    <DeleteForm action={removeSshKey} fields={{ id: k._id }}>
                      <Button type="submit" variant="ghost" size="sm" className="text-muted-foreground hover:text-destructive" aria-label={`Remove ${k.name}`}>
                        <Trash2 />
                      </Button>
                    </DeleteForm>
```
and the token block (lines 180–185):
```tsx
                    <DeleteForm action={revokeToken} fields={{ id: t._id }}>
                      <Button type="submit" variant="outline" size="sm" className="border-edge text-muted-foreground hover:border-destructive/40 hover:text-destructive">
                        Revoke
                      </Button>
                    </DeleteForm>
```

`passkeys-section.tsx` lines 77–82:
```tsx
              <DeleteForm action={removePasskey} fields={{ id: p._id }}>
                <Button type="submit" variant="ghost" size="sm" className="text-muted-foreground hover:text-destructive" aria-label={`Remove ${p.name}`}>
                  <Trash2 />
                </Button>
              </DeleteForm>
```

`repo-settings.tsx` lines 92–98:
```tsx
              <DeleteForm action={removeRule} fields={{ owner, repo, pattern: r.pattern }}>
                <Button type="submit" variant="ghost" size="sm" className="text-muted-foreground hover:text-destructive" aria-label={`Remove the rule for ${r.pattern}`}>
                  <Trash2 />
                </Button>
              </DeleteForm>
```

`pull-actions.tsx` lines 200–205:
```tsx
      <DeleteForm action={close} fields={{ owner, repo, number: String(number) }} className="mt-3 border-t border-border pt-3">
        <Button type="submit" variant="ghost" size="sm" className="text-muted-foreground hover:text-destructive">
          Close without merging
        </Button>
      </DeleteForm>
```

- [ ] **Step 4: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean. Manual: delete a tag → browser confirm appears; with the api down, the row shows the error text instead of nothing.

```bash
git add -A web/apps/web/src
git commit -m "Report failures from destructive actions and confirm tag deletion"
```

---

### Task 14: No button inside a link in the image list

**Files:**
- Modify: `web/apps/web/src/components/app/image-list.tsx:65-85`

**Context:** `<CopyLine>` (a `<button>`) is nested inside `<Link>` — invalid HTML, and the `onClick preventDefault` wrapper is the tell. Make the row a flex `<li>` with the link and the copy control as siblings.

- [ ] **Step 1: Restructure the row**

Lines 65–85 become:
```tsx
        {shown.map((img) => (
          <li key={img.name} className="flex items-start gap-4 px-5 py-4 transition-colors hover:bg-muted/50">
            <Link
              href={`/${owner}/registries/${encodeURIComponent(img.name)}`}
              className="flex min-w-0 flex-1 items-start gap-4"
            >
              <Package className="mt-0.5 size-4 shrink-0 text-muted-foreground" aria-hidden />
              <span className="min-w-0 flex-1">
                <span className="truncate text-body font-medium">{img.name}</span>
                <span className="mt-1 block text-sm2 text-muted-foreground">
                  {img.updated_ms === null ? "Updated unknown" : `Updated ${when(img.updated_ms)}`}
                  {" · "}
                  {img.manifests} {img.manifests === 1 ? "manifest" : "manifests"}
                </span>
              </span>
            </Link>
            <span className="shrink-0">
              <CopyLine value={`docker pull ${host}/${owner}/${img.name}:latest`} compact />
            </span>
          </li>
        ))}
```

- [ ] **Step 2: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean.

```bash
git add web/apps/web/src/components/app/image-list.tsx
git commit -m "Keep the image copy control outside the row link"
```

---

### Task 15: One `guardImage` for the three image pages

**Files:**
- Create: `web/apps/web/src/app/(shell)/[owner]/(org)/registries/[image]/guard.ts`
- Create: `web/apps/web/src/app/(shell)/[owner]/(org)/registries/[image]/layout.tsx`
- Modify: `…/registries/[image]/page.tsx:1-30`, `…/[image]/tags/page.tsx:1-30`, `…/[image]/settings/page.tsx:1-28`

**Context:** Three identical 15-line preambles (session, token, `imageTags`, the three-way error). Mirror `[owner]/[repo]/guard.ts`: a `cache()`-wrapped function the layout and each page call, resolved once per request.

**Interfaces:**
- Produces: `guardImage(owner: string, image: string): Promise<{ owner: string; image: string; token: string; tags: ImageTag[] }>`.

- [ ] **Step 1: Create `guard.ts`**

```ts
import "server-only";
import { cache } from "react";
import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { imageTags, type ImageTag } from "@/lib/browse";

export type ImageContext = { owner: string; image: string; token: string; tags: ImageTag[] };

/** Every image route: signed in, and this image exists in a namespace the caller
 *  may act in — the api answers 404 otherwise, so asking is the check. Wrapped in
 *  `cache` so the layout and the page beneath it resolve the image ONCE per
 *  request, the way `guardRepo` does for a repo. */
export const guardImage = cache(async function guardImage(owner: string, image: string): Promise<ImageContext> {
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");

  const token = await apiToken();
  if (!token) redirect("/login");

  const tags = await imageTags(token, owner, image);
  if (!tags.ok) {
    if (tags.kind === "unauthorized") redirect("/login?from=expired");
    if (tags.kind === "notFound") notFound();
    throw new Error(tags.message);
  }
  return { owner, image, token, tags: tags.value };
});
```

- [ ] **Step 2: Create `layout.tsx`**

```tsx
import { guardImage } from "./guard";

/** Refusing here means no page under it has to check; the page frame itself comes
 *  from `(org)/layout.tsx`, which already wraps this. */
export default async function ImageLayout({
  params,
  children,
}: {
  params: Promise<{ owner: string; image: string }>;
  children: React.ReactNode;
}) {
  const { owner, image } = await params;
  await guardImage(owner, image);
  return <>{children}</>;
}
```

- [ ] **Step 3: Collapse the three pages' preambles**

`[image]/page.tsx` lines 1–30 become:
```tsx
import { Lock } from "lucide-react";
import { guardImage } from "./guard";
import { size, when } from "@/lib/time";
import { CopyLine } from "@/components/app/image-list";

/** One image's Details tab: … (keep the existing comment) */
export default async function ImagePage({ params }: { params: Promise<{ owner: string; image: string }> }) {
  const { owner, image } = await params;
  const { tags: list } = await guardImage(owner, image);
```
and delete the old `const list = tags.value;` at line 33. The rest is unchanged.

`[image]/tags/page.tsx` lines 1–30 become:
```tsx
import { guardImage } from "../guard";
import { size, when } from "@/lib/time";
import { CopyLine } from "@/components/app/image-list";

/** The Tags tab: … (keep the existing comment) */
export default async function ImageTagsPage({ params }: { params: Promise<{ owner: string; image: string }> }) {
  const { owner, image } = await params;
  const { tags: list } = await guardImage(owner, image);
```

`[image]/settings/page.tsx` becomes:
```tsx
import type { Metadata } from "next";
import { guardImage } from "../guard";
import { ImageSettings } from "@/components/registry/image-settings";

export const metadata: Metadata = { title: "Image settings" };

export default async function Page({ params }: { params: Promise<{ owner: string; image: string }> }) {
  const { owner, image } = await params;
  const { tags } = await guardImage(owner, image);
  return <ImageSettings owner={owner} image={image} tags={tags} />;
}
```

- [ ] **Step 4: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean. Manual: `/{owner}/registries/nope` → 404 page; a real image's three tabs still render.

```bash
git add "web/apps/web/src/app/(shell)/[owner]/(org)/registries/[image]"
git commit -m "Share one cached guard across the image pages"
```

---

### Task 16: The api-token reader uses the session cookie Auth.js is configured with

**Files:**
- Modify: `web/apps/web/src/auth.ts:84-100`
- Modify: `web/apps/web/src/lib/api-token.ts` (whole file)

**Context:** `api-token.ts` re-derives the cookie name and `secure` flag from `AUTH_URL` and hopes they match what Auth.js chose. Make the choice once in `auth.ts`, pass it to `NextAuth` (`useSecureCookies`, `cookies.sessionToken.name` — both in `@auth/core` `AuthConfig`, verified in `node_modules/.bun/@auth+core@0.41.3…/index.d.ts:443,458`), and read it back in `getToken` (`cookieName`, `secureCookie`, `salt` — verified in `@auth/core/jwt.d.ts:48-68`). The `salt` must equal the cookie name: that is how Auth.js derives the encryption key.

**Interfaces:**
- Produces from `@/auth`: `export const secureCookies: boolean`, `export const sessionCookie: string`.

- [ ] **Step 1: Export the cookie choice from `auth.ts`**

Before `export const { handlers, … } = NextAuth({`, add:
```ts
/** One decision about the session cookie, made here and read back by
 *  `lib/api-token.ts`. Auth.js would pick the same defaults from AUTH_URL, but
 *  two places deriving the same answer is how they come to differ. */
export const secureCookies = (process.env.AUTH_URL ?? "").startsWith("https");
export const sessionCookie = secureCookies ? "__Secure-authjs.session-token" : "authjs.session-token";
```
and inside the `NextAuth({ … })` options, after `session: { strategy: "jwt" },`:
```ts
  useSecureCookies: secureCookies,
  cookies: { sessionToken: { name: sessionCookie } },
```

- [ ] **Step 2: Read it in `api-token.ts`**

```ts
import "server-only";
import { headers } from "next/headers";
import { getToken } from "next-auth/jwt";
import { secureCookies, sessionCookie } from "@/auth";

/**
 * The api server's token for the signed-in person.
 *
 * Read from the encrypted session JWT rather than from `auth()`: the session
 * object is what `/api/auth/session` returns to the browser, so a bearer
 * credential placed on it would be readable by any client-side script. The JWT
 * is encrypted with AUTH_SECRET and only the server can open it.
 */
export async function apiToken(): Promise<string | undefined> {
  const token = await getToken({
    req: new Request("http://n", { headers: await headers() }),
    secret: process.env.AUTH_SECRET,
    cookieName: sessionCookie,
    // The salt is the cookie name: that is the key-derivation input Auth.js used.
    salt: sessionCookie,
    secureCookie: secureCookies,
  });
  return token?.apiToken;
}
```

- [ ] **Step 3: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean. Manual: sign in locally (http) and load `/settings` — keys/tokens still list (proves `apiToken()` still decrypts).

```bash
git add web/apps/web/src/auth.ts web/apps/web/src/lib/api-token.ts
git commit -m "Configure the session cookie once and read it back for the api token"
```

---

### Task 17: Small correctness fixes: pinned locale, focusable tooltips, validated provider

**Files:**
- Modify: `web/apps/web/src/components/repo/commit-meta.ts:23`
- Modify: `web/apps/web/src/components/repo/repo-about.tsx:103-107`
- Modify: `web/apps/web/src/components/repo/verified-badge.tsx:21-32`
- Modify: `web/apps/web/src/app/(auth)/actions.ts:5,11-14`

- [ ] **Step 1: Pin the locale in `commit-meta.ts`**

Line 23 (`lib/time.ts` explains why: server and browser must agree or React reports a hydration mismatch):
```ts
  return at.toLocaleDateString("en", { year: "numeric", month: "long", day: "numeric" });
```

- [ ] **Step 2: Make the tooltip triggers focusable**

`repo-about.tsx` lines 103–107:
```tsx
                  <TooltipTrigger asChild>
                    <button type="button" className="block" aria-label={c.name}>
                      <Initials name={c.name} size={7} />
                    </button>
                  </TooltipTrigger>
```

`verified-badge.tsx` lines 21–32 — change the `<span` to `<button type="button"` and `</span>` to `</button>`, keeping the className. A `<button>` has no border/background reset issue here: the class list sets both.

- [ ] **Step 3: Validate the provider in `(auth)/actions.ts`**

Line 5 becomes `import { enabledProviders, signIn, signOut } from "@/auth";` and `signInWithProvider` becomes:
```ts
export async function signInWithProvider(formData: FormData) {
  const provider = String(formData.get("provider"));
  // Only a provider this deployment actually registered: Auth.js would answer an
  // unknown id with an opaque error page, and the form never offers one anyway.
  if (!(provider in enabledProviders) || !enabledProviders[provider as keyof typeof enabledProviders]) return;
  await signIn(provider, { redirectTo: "/" });
}
```

- [ ] **Step 4: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean.

```bash
git add web/apps/web/src/components/repo/commit-meta.ts web/apps/web/src/components/repo/repo-about.tsx web/apps/web/src/components/repo/verified-badge.tsx "web/apps/web/src/app/(auth)/actions.ts"
git commit -m "Pin the commit date locale, focus tooltip triggers, validate the sign-in provider"
```

---

## LOW / REDUNDANCY

### Task 18: One `useCopy` hook behind the seven copy widgets

**Files:**
- Create: `web/apps/web/src/lib/use-copy.ts`
- Modify: `src/components/repo/copy-button.tsx`, `command-block.tsx`, `remote-picker.tsx`, `clone-menu.tsx:55-84`, `file-actions.tsx`, `src/components/app/new-token-dialog.tsx`, `src/components/app/image-list.tsx:92-119` (`CopyLine`)

**Context:** Seven copies of `useState(false)` + `navigator.clipboard.writeText` + an uncleaned `setTimeout` (a state update after unmount). The layouts differ, so the widgets stay; the behaviour becomes one hook with cleanup.

**Interfaces:**
- Produces: `useCopy(ms = 1600): { copied: boolean; copy: (value: string) => Promise<void> }` in `@/lib/use-copy`.

- [ ] **Step 1: Create the hook**

```ts
"use client";

import { useEffect, useRef, useState } from "react";

/** Copy a value and say so for a moment. One place for the timer, so it is
 *  cleared on unmount — seven widgets each had their own, none of them cleared. */
export function useCopy(ms = 1600) {
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);
  const copy = async (value: string) => {
    await navigator.clipboard.writeText(value);
    setCopied(true);
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setCopied(false), ms);
  };
  return { copied, copy };
}
```

- [ ] **Step 2: Replace the state in each widget**

In every file: remove `useState` from the React import if nothing else uses it, add `import { useCopy } from "@/lib/use-copy";`, replace `const [copied, setCopied] = useState(false);` with `const { copied, copy } = useCopy();` (use `useCopy(1500)` where the file used 1500 — `clone-menu.tsx`, `image-list.tsx`), and replace each
```ts
        onClick={async () => {
          await navigator.clipboard.writeText(X);
          setCopied(true);
          setTimeout(() => setCopied(false), N);
        }}
```
with `onClick={() => copy(X)}`.

`new-token-dialog.tsx` specifically: remove `const [copied, setCopied] = useState(false);`, change line 26 to `onOpenChange={setOpen}` (the hook resets itself), and line 72 to `onClick={() => copy(state!.token!)}`. `useState` is still used for `open` — keep that import.

`remote-picker.tsx`, `file-actions.tsx`, `copy-button.tsx`, `command-block.tsx`: `useState` becomes unused in `copy-button.tsx`, `command-block.tsx`, `file-actions.tsx` — drop the import there.

- [ ] **Step 3: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean. `grep -rn "setTimeout" web/apps/web/src` → only `lib/use-copy.ts`.

```bash
git add web/apps/web/src/lib/use-copy.ts web/apps/web/src/components
git commit -m "Share one copy-to-clipboard hook with a cleared timer"
```

---

### Task 19: One theme control

**Files:**
- Delete: `web/apps/web/src/components/app/theme-picker.tsx`
- Modify: `web/apps/web/src/components/app/user-settings.tsx:3,63-65`
- Modify: `web/apps/web/src/app/globals.css:52,65-72` (swatch tokens)

**Context:** `theme-picker.tsx` (three swatch cards, Settings) and `theme-toggle.tsx` (segmented control, landing + auth footers) both wrap `useTheme`. Keep the one used in two places; the Appearance section gets the segmented control. The swatch-only CSS tokens go with the picker.

- [ ] **Step 1: Swap the picker for the toggle in settings**

`user-settings.tsx` line 3: `import { ThemeToggle } from "@/components/theme-toggle";`; lines 63–65:
```tsx
          <Section title="Appearance" description="Light, dark, or whatever the operating system is doing. Applies to this browser.">
            <ThemeToggle />
          </Section>
```

- [ ] **Step 2: Delete the picker and its tokens**

```bash
git rm web/apps/web/src/components/app/theme-picker.tsx
```
In `globals.css` remove line 52 (`--grid-template-rows-swatch: …`) and lines 65–72 (the swatch comment and the six `--color-swatch-*` tokens). Confirm with `grep -rn "swatch" web/apps/web/src` → no hits.

- [ ] **Step 3: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean.

```bash
git add -A web/apps/web/src
git commit -m "Keep one theme control"
```

---

### Task 20: Delete the dev sign-in bypass

**Files:**
- Delete: `web/apps/web/src/lib/dev-auth.ts`, `web/apps/web/src/components/auth/dev-bypass.tsx`
- Modify: `web/apps/web/src/lib/session.ts`, `web/apps/web/src/app/(auth)/actions.ts`, `web/apps/web/src/app/(auth)/login/page.tsx`

**Context:** The bypass yields a session with no `username` and no `apiToken`, so every page past the landing redirects to `/welcome` or `/login` — it cannot be used. `AUTH_DEV_BYPASS` appears in no deploy yaml, README, or doc (only the review). A preview password (`AUTH_ALLOWED_EMAILS` + `AUTH_SHARED_PASSWORD`) already covers "work on the app before OAuth exists" and produces a real session. Delete rather than mint a token.

- [ ] **Step 1: Delete the two files**

```bash
git rm web/apps/web/src/lib/dev-auth.ts web/apps/web/src/components/auth/dev-bypass.tsx
```

- [ ] **Step 2: Remove the references**

`lib/session.ts`: delete line 2 (`cookies` import), line 4 (`dev-auth` import) and lines 41–46 (the `DEV_BYPASS` block); `getSession` ends with `return null;` after the real-session branch.

`(auth)/actions.ts` becomes:
```ts
"use server";

import { enabledProviders, signIn, signOut } from "@/auth";

/** Sign-in is a server action, not a client call to an endpoint. Auth.js still
 *  needs its callback route for the provider's redirect, but nothing here is
 *  reachable as an API. */
export async function signInWithProvider(formData: FormData) {
  const provider = String(formData.get("provider"));
  // Only a provider this deployment actually registered: Auth.js would answer an
  // unknown id with an opaque error page, and the form never offers one anyway.
  if (!(provider in enabledProviders) || !enabledProviders[provider as keyof typeof enabledProviders]) return;
  await signIn(provider, { redirectTo: "/" });
}

export async function signOutAction() {
  await signOut({ redirectTo: "/" });
}
```

`login/page.tsx`: delete the `DevBypass` import and the `<DevBypass />` element.

- [ ] **Step 3: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean. `grep -rn "DEV_BYPASS\|dev-auth\|devSignIn" web/apps/web/src` → no hits.

```bash
git add -A web/apps/web/src
git commit -m "Remove the dev sign-in bypass that could not reach any page"
```

---

### Task 21: Drop the `rounded-*!` overrides in the command palette

**Files:**
- Modify: `web/apps/web/src/components/ui/command.tsx:28,57,74,158`

**Context:** `--radius: 0` everywhere; `rounded-xl!` / `rounded-lg!` only resolve to square corners by accident of the token and would reappear the day a radius is set.

- [ ] **Step 1: Remove them**

- line 28: `"flex size-full flex-col overflow-hidden bg-popover p-1 text-popover-foreground"`
- line 57: `"top-1/3 translate-y-0 overflow-hidden p-0"`
- line 74: `<InputGroup className="h-8! border-input/30 bg-input/30 shadow-none! *:data-[slot=input-group-addon]:pl-2!">`
- line 158: delete `in-data-[slot=dialog-content]:rounded-lg! ` from the class string (and the leading `rounded-sm ` as well — same reason).

- [ ] **Step 2: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean. ⌘K still opens square.

```bash
git add web/apps/web/src/components/ui/command.tsx
git commit -m "Stop overriding corner radius in the command palette"
```

---

### Task 22: `shadcn` is a build-time dependency

**Files:**
- Modify: `web/apps/web/package.json`
- Modify: `web/bun.lock` (regenerated)

**Context:** `shadcn` (the CLI) is in `dependencies` only because `globals.css:3` does `@import "shadcn/tailwind.css"`. That import is resolved by Tailwind at `next build`; the standalone runtime never needs it. `web/Dockerfile` runs `bun install --frozen-lockfile` (no `--production`) before the build, so dev dependencies are present when the CSS compiles. `tw-animate-css` is the same kind of thing and moves with it.

- [ ] **Step 1: Move the two packages**

In `web/apps/web/package.json` move `"shadcn": "^4.18.0"` and `"tw-animate-css": "^1.4.0"` from `dependencies` to `devDependencies` (keep alphabetical order).

- [ ] **Step 2: Refresh the lockfile and prove the build still resolves the CSS**

From `web/`: `bun install` (updates `bun.lock` for the group move), then `bun run build`. Expected: build succeeds; no "Can't resolve 'shadcn/tailwind.css'" error. If it fails, revert the move, keep `shadcn` in `dependencies`, and add a one-line comment above the import in `globals.css` saying why it must stay a runtime dependency — then commit only that comment.

- [ ] **Step 3: Verify and commit**

From `web/`: `bun run lint` and `bunx tsc --noEmit -p apps/web/tsconfig.json` → clean.

```bash
git add web/apps/web/package.json web/bun.lock
git commit -m "Move shadcn and tw-animate-css to devDependencies"
```

---

## Final verification (after all tasks)

- [ ] From `web/apps/web`: `bun test` → 6 pass.
- [ ] From `web/`: `bun run lint`, `bunx tsc --noEmit -p apps/web/tsconfig.json`, `bun run build` → all clean.
- [ ] `grep -rn "lib/mock\|mock-repo\|dev-auth\|lib/sso\|theme-picker\|setTimeout" web/apps/web/src` → only `lib/use-copy.ts` matches.
- [ ] Re-read `docs/code-review-2026-08-23.md` sections 2–6 and check every `web/` row maps to a landed commit.
- [ ] Deploy is a separate step: the web image rebuilds on push (`web/**` changed); pin the SHA in `deploy/kloudlite-git-web.yaml` per `CLAUDE.md`.
