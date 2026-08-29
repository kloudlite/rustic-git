"use client";

import { useActionState, useState } from "react";
import { GitBranch, Globe, Loader2, Lock, Trash2, TriangleAlert } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Checkbox } from "@/components/ui/checkbox";
import { FieldLabel } from "@/components/auth/auth-card";
import { Saved, SettingsSection as Section } from "@/components/app/settings-section";
import { DeleteForm } from "@/components/app/delete-form";
import type { ApiProtection, ApiRepo } from "@/lib/api";
import {
  addRule, destroyRepo, removeRule, saveDescription, setVisibility, type SettingsState,
} from "@/app/(shell)/[owner]/[repo]/settings/actions";

/** Hidden inputs rather than closures: these forms post to server actions, and the
 *  repo they are about has to travel with the request. */
function Which({ owner, repo }: { owner: string; repo: string }) {
  return (
    <>
      <input type="hidden" name="owner" value={owner} />
      <input type="hidden" name="repo" value={repo} />
    </>
  );
}

function Description({ owner, repo, description }: { owner: string; repo: string; description: string }) {
  const [state, action, pending] = useActionState<SettingsState, FormData>(saveDescription, null);
  return (
    <form action={action} className="grid max-w-xl gap-3">
      <Which owner={owner} repo={repo} />
      <div className="grid gap-2">
        <FieldLabel htmlFor="description">Description</FieldLabel>
        <Input id="description" name="description" defaultValue={description} placeholder="What this is for." className="h-9" />
      </div>
      <Saved state={state} />
      <div><Button type="submit" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Save</Button></div>
    </form>
  );
}

function Visibility({ owner, repo, isPublic }: { owner: string; repo: string; isPublic: boolean }) {
  const [state, action, pending] = useActionState<SettingsState, FormData>(setVisibility, null);
  return (
    <form action={action} className="grid max-w-xl gap-3">
      <Which owner={owner} repo={repo} />
      <fieldset className="grid gap-2">
        <legend className="sr-only">Visibility</legend>
        {[
          { value: "private", Icon: Lock, title: "Private", detail: "Only people you give access to can see it." },
          { value: "public", Icon: Globe, title: "Public", detail: "Anyone can read it. Only members can push." },
        ].map(({ value, Icon, title, detail }) => (
          <label key={value} className="flex cursor-pointer items-start gap-3 border border-border bg-card p-3 transition-colors has-checked:border-primary hover:bg-muted/50">
            <input type="radio" name="visibility" value={value} defaultChecked={isPublic === (value === "public")} className="mt-0.5 accent-primary" />
            <Icon className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
            <span>
              <span className="block text-sm2 font-medium">{title}</span>
              <span className="block text-caption text-muted-foreground">{detail}</span>
            </span>
          </label>
        ))}
      </fieldset>
      <Saved state={state} />
      <div><Button type="submit" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Update visibility</Button></div>
    </form>
  );
}

function Protection({ owner, repo, rules }: { owner: string; repo: string; rules: ApiProtection[] }) {
  const [state, action, pending] = useActionState<SettingsState, FormData>(addRule, null);
  return (
    <>
      {rules.length === 0 ? (
        <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
          No protected branches. Anyone who can push can rewrite or delete any branch.
        </p>
      ) : (
        <ul className="divide-y divide-border border border-border bg-card">
          {rules.map((r) => (
            <li key={r.pattern} className="flex items-center gap-4 px-4 py-3">
              <GitBranch className="size-4 shrink-0 text-muted-foreground" />
              <div className="min-w-0 flex-1">
                <div className="truncate font-mono text-sm2 font-medium">{r.pattern}</div>
                <div className="mt-0.5 text-caption text-muted-foreground">
                  {[r.no_force && "no force pushes", r.no_delete && "no deletion"].filter(Boolean).join(" · ") || "nothing enforced"}
                </div>
              </div>
              <DeleteForm action={removeRule} fields={{ owner, repo, pattern: r.pattern }} confirm={`Remove the rule for ${r.pattern}?`}>
                <Button type="submit" variant="ghost" size="sm" className="text-muted-foreground hover:text-destructive" aria-label={`Remove the rule for ${r.pattern}`}>
                  <Trash2 />
                </Button>
              </DeleteForm>
            </li>
          ))}
        </ul>
      )}

      <form action={action} className="mt-4 grid max-w-xl gap-3 border border-border bg-card p-4">
        <Which owner={owner} repo={repo} />
        <div className="grid gap-2">
          <FieldLabel htmlFor="pattern">Branch or pattern</FieldLabel>
          <Input id="pattern" name="pattern" placeholder="main" className="h-9 font-mono" />
          <p className="text-caption text-muted-foreground">
            An exact name, or a trailing star — <code className="font-mono">release/*</code>.
          </p>
        </div>
        <div className="grid gap-2">
          <label className="flex items-center gap-2.5 text-sm2">
            <Checkbox name="no_force" defaultChecked /> Block force pushes
          </label>
          <label className="flex items-center gap-2.5 text-sm2">
            <Checkbox name="no_delete" defaultChecked /> Block deletion
          </label>
        </div>
        <Saved state={state} />
        <div><Button type="submit" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Add rule</Button></div>
      </form>
    </>
  );
}

function Danger({ owner, repo }: { owner: string; repo: string }) {
  const [state, action, pending] = useActionState<SettingsState, FormData>(destroyRepo, null);
  const [typed, setTyped] = useState("");
  return (
    <div className="border border-destructive/40 bg-card">
      <div className="border-b border-destructive/40 bg-destructive/5 px-4 py-2.5 text-sm2 font-medium">
        Delete this repository
      </div>
      <form action={action} className="grid max-w-xl gap-3 p-4">
        <Which owner={owner} repo={repo} />
        <p className="flex items-start gap-2 text-sm2 leading-relaxed text-muted-foreground">
          <TriangleAlert className="mt-0.5 size-4 shrink-0 text-destructive" />
          Its objects, refs and history are removed. Clones people already have keep
          working; nothing here can be recovered.
        </p>
        <div className="grid gap-2">
          <FieldLabel htmlFor="confirm">
            Type <span className="font-mono font-semibold text-foreground">{owner}/{repo}</span> to confirm
          </FieldLabel>
          <Input id="confirm" name="confirm" value={typed} onChange={(e) => setTyped(e.target.value)} autoComplete="off" placeholder={`${owner}/${repo}`} className="h-9 font-mono" />
        </div>
        <Saved state={state} />
        <div>
          <Button type="submit" variant="destructive" disabled={pending || typed !== `${owner}/${repo}`}>
            {pending && <Loader2 className="animate-spin" />}Delete this repository
          </Button>
        </div>
      </form>
    </div>
  );
}

/** Everything about the repo that is a setting rather than a fact: what it says it
 *  is, who may read it, which branches are defended, and the way out. */
export function RepoSettings({ meta, rules }: { meta: ApiRepo; rules: ApiProtection[] }) {
  const { owner, name } = meta;
  return (
    <div className="grid gap-8">
      <h1 className="text-title font-semibold tracking-title">Repository settings</h1>

      <Section title="General" description="How this repo describes itself, wherever it is listed.">
        <Description owner={owner} repo={name} description={meta.description} />
      </Section>

      <Section title="Visibility" description="Who may read the code. Pushing always requires access, whichever this is.">
        <Visibility owner={owner} repo={name} isPublic={meta.public} />
      </Section>

      <Section
        title="Branch protection"
        description="Rules the server enforces when someone pushes — not a convention, a refusal. They apply to everyone, including you."
      >
        <Protection owner={owner} repo={name} rules={rules} />
      </Section>

      <Section title="Danger zone" description="These cannot be undone.">
        <Danger owner={owner} repo={name} />
      </Section>
    </div>
  );
}
