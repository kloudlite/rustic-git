"use client";

import { useActionState, useState } from "react";
import { ArrowRight, GitPullRequest, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { FieldLabel } from "@/components/auth/auth-card";
import { openPull, type PullState } from "@/app/(shell)/[owner]/[repo]/pulls/actions";

function Branch({ name, value, branches, onChange, label }: {
  name: string;
  value: string;
  branches: string[];
  onChange: (v: string) => void;
  label: string;
}) {
  return (
    <label className="grid gap-1.5">
      <span className="text-caption font-medium uppercase tracking-label text-muted-foreground">{label}</span>
      <select
        name={name}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="h-9 min-w-40 border border-input bg-card px-2.5 font-mono text-sm2 outline-none focus-visible:border-ring"
      >
        {branches.map((b) => <option key={b} value={b}>{b}</option>)}
      </select>
    </label>
  );
}

/** Propose putting one branch onto another. The two are picked first, because
 *  which branches are involved is what the change IS — the title describes it. */
export function NewPullForm({
  owner,
  repo,
  branches,
  defaultBase,
  defaultHead,
}: {
  owner: string;
  repo: string;
  branches: string[];
  defaultBase: string;
  defaultHead: string;
}) {
  const [state, action, pending] = useActionState<PullState, FormData>(openPull, null);
  const [base, setBase] = useState(defaultBase);
  const [head, setHead] = useState(defaultHead);
  const same = base === head;

  return (
    <div className="max-w-2xl">
      <h1 className="text-title font-semibold tracking-title">New pull request</h1>

      <form action={action} className="mt-6 grid gap-5">
        <input type="hidden" name="owner" value={owner} />
        <input type="hidden" name="repo" value={repo} />

        <div className="flex flex-wrap items-end gap-3 border border-border bg-card p-4">
          <Branch label="Base" name="base" value={base} branches={branches} onChange={setBase} />
          <ArrowRight className="mb-2.5 size-4 shrink-0 text-muted-foreground" />
          <Branch label="Compare" name="head" value={head} branches={branches} onChange={setHead} />
          <p className="mb-1 text-caption text-muted-foreground">
            {same ? "Pick two different branches." : <>What is on <code className="font-mono">{head}</code> goes onto <code className="font-mono">{base}</code>.</>}
          </p>
        </div>

        <div className="grid gap-2">
          <FieldLabel htmlFor="title">Title</FieldLabel>
          <Input id="title" name="title" placeholder="What this changes" autoFocus className="h-9" />
        </div>

        <div className="grid gap-2">
          <FieldLabel htmlFor="body">Description <span className="font-normal text-muted-foreground">(optional)</span></FieldLabel>
          <Textarea id="body" name="body" rows={6} placeholder="Why it changes, and anything a reviewer should know." className="resize-y text-sm2" />
        </div>

        {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}

        <div>
          <Button type="submit" size="lg" disabled={pending || same}>
            {pending ? <Loader2 className="animate-spin" /> : <GitPullRequest />}
            Create pull request
          </Button>
        </div>
      </form>
    </div>
  );
}
