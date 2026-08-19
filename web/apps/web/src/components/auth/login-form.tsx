"use client";

import Link from "next/link";
import { useActionState } from "react";
import { ArrowLeft, Building2, Loader2 } from "lucide-react";
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
export function LoginForm({ oauth }: { oauth?: React.ReactNode }) {
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

  if (current.step === "sso") {
    return (
      <div>
        <div className="mx-auto mb-5 flex size-10 items-center justify-center border border-edge bg-muted/50">
          <Building2 className="size-4.5 text-muted-foreground" />
        </div>
        <AuthHeader title={`Continue with ${current.org}`}>
          <span className="font-medium text-foreground">{current.email}</span> uses single
          sign-on. You&rsquo;ll finish signing in with your organisation&rsquo;s identity provider.
        </AuthHeader>
        <Button size="lg" className="w-full">
          Continue to {current.org}
        </Button>
        <form action={submitEmail} className="mt-4 text-center">
          <Button type="submit" name="email" value="" variant="link" className="h-auto p-0 text-sm2 text-muted-foreground hover:text-foreground">
            <ArrowLeft />
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
          <FieldLabel
            htmlFor="password"
            aside={
              <Link
                href="/reset"
                className="text-sm2 font-medium text-muted-foreground underline-offset-4 transition-colors hover:text-foreground hover:underline"
              >
                Forgot password?
              </Link>
            }
          >
            Password
          </FieldLabel>
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
      <AuthHeader title="Sign in to kloudlite">Continue to your workspaces and repos.</AuthHeader>

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
          Continue
        </Button>
      </form>

      <p className="mt-3 text-caption leading-relaxed text-muted-foreground">
        If your organisation uses single sign-on, we&rsquo;ll take you there.
      </p>
    </div>
  );
}
