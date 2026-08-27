"use client";

import { useActionState } from "react";
import { Loader2, Lock, Globe } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { FieldLabel } from "@/components/auth/auth-card";
import type { SwitcherOwner } from "@/components/app/team-switcher";
import { create, type NewRepoState } from "@/app/(shell)/new-repo/actions";

/** Visibility is two radios rather than a switch: the difference is not a degree
 *  of one thing, and each option says what it means in its own words. Private is
 *  first and pre-selected — the safe answer is the default one. */
function Visibility() {
  const options = [
    { value: "private", Icon: Lock, title: "Private", detail: "Only people you give access to can see it." },
    { value: "public", Icon: Globe, title: "Public", detail: "Anyone can read it. Only members can push." },
  ];
  return (
    <fieldset className="grid gap-2">
      <legend className="mb-2 text-sm2 font-medium">Visibility</legend>
      {options.map(({ value, Icon, title, detail }, i) => (
        <label
          key={value}
          className="flex cursor-pointer items-start gap-3 border border-border bg-card p-3 transition-colors has-checked:border-primary hover:bg-muted/50"
        >
          <input
            type="radio"
            name="visibility"
            value={value}
            defaultChecked={i === 0}
            className="mt-0.5 accent-primary"
          />
          <Icon className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
          <span>
            <span className="block text-sm2 font-medium">{title}</span>
            <span className="block text-caption text-muted-foreground">{detail}</span>
          </span>
        </label>
      ))}
    </fieldset>
  );
}

export function NewRepoForm({ owners, defaultOwner }: { owners: SwitcherOwner[]; defaultOwner: string }) {
  const [state, action, pending] = useActionState<NewRepoState, FormData>(create, null);

  return (
    <div className="max-w-xl">
      <h1 className="text-title font-semibold tracking-title">Create a repo</h1>
      <p className="mt-1 text-sm2 leading-relaxed text-muted-foreground">
        An empty repo, ready to push to. Clone it, or add a remote to something you
        already have.
      </p>

      <form action={action} className="mt-8 grid gap-5">
        <div className="grid gap-2">
          <FieldLabel htmlFor="owner">Owner</FieldLabel>
          <div className="flex h-10 items-stretch border border-input bg-card focus-within:border-ring">
            {/* Borderless: the frame around owner / name is the one control's border. */}
            <Select name="owner" defaultValue={defaultOwner}>
              <SelectTrigger id="owner" className="h-full shrink-0 border-0 bg-transparent pl-3 font-mono text-sm2 shadow-none focus-visible:ring-0">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {owners.map((o) => (
                  <SelectItem key={o.slug} value={o.slug} className="font-mono">{o.slug}</SelectItem>
                ))}
              </SelectContent>
            </Select>
            <span className="flex shrink-0 items-center px-1 font-mono text-sm2 text-muted-foreground">/</span>
            <Input
              id="name"
              name="name"
              placeholder="my-service"
              autoFocus
              autoComplete="off"
              spellCheck={false}
              aria-label="Repo name"
              className="h-full min-w-0 flex-1 rounded-none border-0 bg-transparent pl-0 font-mono text-sm2 focus-visible:ring-0"
            />
          </div>
          <p className="text-caption text-muted-foreground">
            Letters, digits, dots, dashes and underscores.
          </p>
        </div>

        <div className="grid gap-2">
          <FieldLabel htmlFor="description">Description <span className="font-normal text-muted-foreground">(optional)</span></FieldLabel>
          <Input id="description" name="description" placeholder="What this is for." className="h-10" />
        </div>

        <Visibility />

        {state?.error && (
          <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>
        )}

        <div>
          <Button type="submit" size="lg" disabled={pending}>
            {pending && <Loader2 className="animate-spin" />}
            Create repo
          </Button>
        </div>
      </form>
    </div>
  );
}
