"use client";

import { useState } from "react";
import Link from "next/link";
import { Lock, Globe } from "lucide-react";
import { CommandBlock } from "@/components/repo/command-block";
import { RemotePicker } from "@/components/repo/remote-picker";
import type { CloneUrls } from "@/lib/clone";

/** A repo with no refs.
 *
 *  Not an error and not an empty box: it is the state every repo starts in, and
 *  the only useful thing to show is how to leave it. The protocol is picked once,
 *  at the top, and every command below follows it — instructions that mix an ssh
 *  remote with an https note are how someone ends up pasting a URL that cannot
 *  reach the server they just authenticated to.
 *
 *  Client-side because that one choice drives everything under it. */
export function EmptyRepo({
  owner,
  repo,
  urls,
  isPrivate,
  branch = "main",
}: {
  owner: string;
  repo: string;
  urls: CloneUrls;
  isPrivate: boolean;
  branch?: string;
}) {
  const [kind, setKind] = useState<"ssh" | "https">("ssh");
  const remote = urls[kind];

  return (
    <div className="mx-auto max-w-2xl">
      <header className="border-b border-border pb-6">
        <div className="flex items-center gap-2.5">
          <h1 className="font-mono text-lead font-medium">
            <Link href={`/${owner}`} className="text-muted-foreground underline-offset-4 hover:text-foreground hover:underline">
              {owner}
            </Link>
            <span className="text-muted-foreground/50"> / </span>
            {repo}
          </h1>
          <span className="flex items-center gap-1 border border-border px-1.5 py-0.5 text-micro font-medium uppercase tracking-label text-muted-foreground">
            {isPrivate ? <Lock className="size-3" /> : <Globe className="size-3" />}
            {isPrivate ? "Private" : "Public"}
          </span>
        </div>
        <p className="mt-2 text-sm2 text-muted-foreground">
          Nothing has been pushed yet. Once it has, this page becomes the code.
        </p>
      </header>

      <section className="mt-6">
        <div className="flex items-baseline justify-between">
          <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
            Remote
          </h2>
          <p className="text-caption text-muted-foreground">
            {kind === "ssh" ? (
              <>Uses a key from <Link href="/settings" className="text-primary underline-offset-4 hover:underline">Settings</Link></>
            ) : (
              <>Uses an access token from <Link href="/settings" className="text-primary underline-offset-4 hover:underline">Settings</Link></>
            )}
          </p>
        </div>
        <div className="mt-2">
          <RemotePicker urls={urls} onChange={setKind} />
        </div>
      </section>

      <section className="mt-8 grid gap-3">
        <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
          Start something new
        </h2>
        <CommandBlock
          command={`git init
git add .
git commit -m "Initial commit"
git branch -M ${branch}
git remote add origin ${remote}
git push -u origin ${branch}`}
        />
      </section>

      <section className="mt-8 grid gap-3">
        <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">
          Push an existing repo
        </h2>
        <CommandBlock
          command={`git remote add origin ${remote}
git push -u origin ${branch}`}
        />
      </section>

      {kind === "https" && (
        <p className="mt-6 border-l-2 border-border pl-4 text-caption leading-relaxed text-muted-foreground">
          Git will ask for a username and password. The username is ignored — use
          anything — and the password is an access token, not your account password.
        </p>
      )}
    </div>
  );
}
