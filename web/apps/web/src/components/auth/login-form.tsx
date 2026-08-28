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
  next,
  title = "Sign in to kloudlite",
  subtitle = "Continue to your workspaces and repos.",
  submitLabel = "Continue",
}: {
  oauth?: React.ReactNode;
  /** Why the person is here, when it was not their idea — an expired session. */
  notice?: string;
  /** Where to land after signing in. Already validated by the page; every form below
   *  carries it so the destination survives whichever provider they pick. */
  next?: string;
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

  if (current.step === "sent") {
    return (
      <div>
        <AuthHeader title="Check your email">
          A sign-in link is on its way to <span className="font-medium text-foreground">{current.email}</span>.
          It works once and expires in 15 minutes.
        </AuthHeader>
        <form action={submitEmail} className="text-center">
          <Button type="submit" name="email" value="" variant="link" className="h-auto p-0 text-sm2">
            Use a different email
          </Button>
        </form>
      </div>
    );
  }

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
          {next && <input type="hidden" name="next" value={next} />}
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
        {/* The emailed link is built server-side from this, so the magic-link path lands
            in the same place as every other provider. */}
        {next && <input type="hidden" name="next" value={next} />}
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
