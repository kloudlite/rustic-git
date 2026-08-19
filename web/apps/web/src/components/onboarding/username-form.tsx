"use client";

import { useActionState } from "react";
import { Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { AuthCard, AuthHeader, FieldLabel } from "@/components/auth/auth-card";
import { claim, type ClaimState } from "@/app/(onboarding)/welcome/actions";

export function UsernameForm({ name, suggestion }: { name: string; suggestion: string }) {
  const [state, action, pending] = useActionState<ClaimState, FormData>(claim, null);
  const first = name.split(" ")[0];

  return (
    <AuthCard>
      <AuthHeader title={`Welcome, ${first}`}>
        Pick a handle. It becomes your namespace — every repo, workspace and clone
        address you own sits under it, so it cannot be changed later.
      </AuthHeader>

      <form action={action} className="grid gap-2">
        <FieldLabel htmlFor="username">Handle</FieldLabel>
        <div className="flex h-10 items-stretch border border-input bg-card focus-within:border-ring">
          <span className="flex shrink-0 items-center pl-3 font-mono text-caption text-muted-foreground">
            kloudlite.io/
          </span>
          <Input
            id="username"
            name="username"
            defaultValue={state?.suggestion ?? suggestion}
            autoFocus
            autoComplete="off"
            spellCheck={false}
            aria-describedby="handle-rules"
            className="h-full min-w-0 flex-1 rounded-none border-0 bg-transparent pl-0 font-mono text-sm2 focus-visible:ring-0"
          />
        </div>
        <p id="handle-rules" className="text-caption text-muted-foreground">
          3–39 characters. Lowercase letters, digits and dashes.
        </p>
        {state?.error && (
          <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>
        )}
        <Button type="submit" size="lg" disabled={pending} className="mt-3 w-full">
          {pending && <Loader2 className="animate-spin" />}
          Continue
        </Button>
      </form>
    </AuthCard>
  );
}
