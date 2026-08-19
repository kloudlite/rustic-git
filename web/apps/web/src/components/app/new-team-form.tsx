"use client";

import { useActionState, useState } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { FieldLabel } from "@/components/auth/auth-card";
import { create, type NewTeamState } from "@/app/(app)/new-team/actions";

/** A team's handle shares one namespace with every username, so it is offered
 *  rather than derived silently — the person should see what they are claiming. */
function toHandle(name: string) {
  return name
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 39);
}

export function NewTeamForm() {
  const [state, action, pending] = useActionState<NewTeamState, FormData>(create, null);
  const [name, setName] = useState("");
  const [slug, setSlug] = useState("");
  const [touched, setTouched] = useState(false);

  return (
    <div className="max-w-xl">
      <h1 className="text-title font-semibold tracking-title">Create a team</h1>
      <p className="mt-1 text-sm2 leading-relaxed text-muted-foreground">
        A team owns repos, workspaces and environments, and its handle is the
        namespace they all sit under.
      </p>

      <form action={action} className="mt-8 grid gap-5">
        <div className="grid gap-2">
          <FieldLabel htmlFor="name">Team name</FieldLabel>
          <Input
            id="name"
            name="name"
            value={name}
            onChange={(e) => {
              setName(e.target.value);
              if (!touched) setSlug(toHandle(e.target.value));
            }}
            placeholder="Platform Engineering"
            autoFocus
            className="h-10"
          />
        </div>

        <div className="grid gap-2">
          <FieldLabel htmlFor="slug">Handle</FieldLabel>
          <div className="flex h-10 items-stretch border border-input bg-card focus-within:border-ring">
            <span className="flex shrink-0 items-center pl-3 font-mono text-caption text-muted-foreground">
              kloudlite.io/
            </span>
            <Input
              id="slug"
              name="slug"
              value={slug}
              onChange={(e) => {
                setTouched(true);
                setSlug(e.target.value);
              }}
              autoComplete="off"
              spellCheck={false}
              className="h-full min-w-0 flex-1 rounded-none border-0 bg-transparent pl-0 font-mono text-sm2 focus-visible:ring-0"
            />
          </div>
          <p className="text-caption text-muted-foreground">
            3–39 characters, lowercase. Shared with everyone&rsquo;s handles, so it
            has to be unused. It cannot be changed later.
          </p>
        </div>

        {state?.error && (
          <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>
        )}

        <div>
          <Button type="submit" size="lg" disabled={pending}>
            {pending && <Loader2 className="animate-spin" />}
            Create team
          </Button>
        </div>
      </form>
    </div>
  );
}
