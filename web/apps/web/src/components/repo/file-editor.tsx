"use client";

import Link from "next/link";
import { useActionState, useState } from "react";
import { GitBranch, GitCommitHorizontal, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { FieldLabel } from "@/components/auth/auth-card";
import { commitFile, type EditState } from "@/app/(shell)/[owner]/[repo]/edit/actions";
import { pathHref } from "@/lib/utils";

/** A file, and somewhere to put it.
 *
 *  Deliberately a textarea rather than a code editor: this is for a typo, a
 *  version bump, a line in a README. Anything larger is a checkout, and pretending
 *  otherwise would mean shipping a syntax engine to edit one line.
 *
 *  The two places an edit can go are the whole point of the second half of this
 *  form. Committing straight to the branch is the fast path; a new branch is what
 *  makes an edit to a protected branch possible at all, since the base never
 *  moves and the change goes to review instead. */
export function FileEditor({
  owner,
  repo,
  path,
  branch,
  expect,
  initial,
}: {
  owner: string;
  repo: string;
  path: string;
  branch: string;
  expect: string;
  initial: string;
}) {
  const [state, action, pending] = useActionState<EditState, FormData>(commitFile, null);
  const [target, setTarget] = useState<"here" | "branch">("here");
  const [text, setText] = useState(initial);
  const filename = path.split("/").pop() ?? path;
  const untouched = text === initial;
  const suggestion = `patch-${filename.replace(/[^a-zA-Z0-9._-]/g, "-")}`;

  return (
    <section className="min-w-0">
      <div className="flex flex-wrap items-baseline gap-2">
        <h1 className="font-mono text-body font-medium">{path}</h1>
        <span className="text-caption text-muted-foreground">on {branch}</span>
      </div>

      <form action={action} className="mt-5 grid gap-5">
        <input type="hidden" name="owner" value={owner} />
        <input type="hidden" name="repo" value={repo} />
        <input type="hidden" name="path" value={path} />
        <input type="hidden" name="branch" value={branch} />
        <input type="hidden" name="expect" value={expect} />

        <Textarea
          name="content"
          value={text}
          onChange={(e) => setText(e.target.value)}
          spellCheck={false}
          rows={24}
          aria-label={`Contents of ${path}`}
          className="resize-y bg-card font-mono text-caption leading-relaxed"
        />

        <div className="grid gap-4 border border-border bg-card p-4">
          <div className="grid gap-2">
            <FieldLabel htmlFor="message">Commit message</FieldLabel>
            <Input
              id="message"
              name="message"
              placeholder={`Update ${filename}`}
              className="h-9"
            />
          </div>

          <fieldset className="grid gap-2.5">
            <legend className="sr-only">Where this commit goes</legend>
            <label className="flex items-start gap-2.5 text-sm2">
              <input
                type="radio"
                name="target"
                value="here"
                checked={target === "here"}
                onChange={() => setTarget("here")}
                className="mt-1 accent-primary"
              />
              <span>
                Commit directly to <code className="font-mono">{branch}</code>
              </span>
            </label>
            <label className="flex items-start gap-2.5 text-sm2">
              <input
                type="radio"
                name="target"
                value="branch"
                checked={target === "branch"}
                onChange={() => setTarget("branch")}
                className="mt-1 accent-primary"
              />
              <span className="min-w-0">
                <span className="flex items-center gap-1.5">
                  <GitBranch className="size-3.5 text-muted-foreground" />
                  Start a new branch and open a pull request
                </span>
                {target === "branch" && (
                  <Input
                    name="newBranch"
                    defaultValue={suggestion}
                    aria-label="New branch name"
                    className="mt-2 h-8 font-mono text-caption"
                  />
                )}
              </span>
            </label>
          </fieldset>

          {state?.error && (
            <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>
          )}

          <div className="flex items-center gap-3">
            <Button type="submit" disabled={pending || untouched}>
              {pending ? <Loader2 className="animate-spin" /> : <GitCommitHorizontal />}
              Commit changes
            </Button>
            <Button asChild variant="ghost">
              <Link href={`/${owner}/${repo}/blob/${pathHref(path)}?ref=${encodeURIComponent(branch)}`}>Cancel</Link>
            </Button>
            {untouched && <span className="text-caption text-muted-foreground">Nothing changed yet.</span>}
          </div>
        </div>
      </form>
    </section>
  );
}
