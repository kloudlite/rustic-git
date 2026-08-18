"use client";

import Link from "next/link";
import { useActionState } from "react";
import { ArrowLeft, Building2, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { continueWithEmail, signInWithPassword, type LoginState } from "@/app/(auth)/login/actions";

function FieldError({ children }: { children?: string }) {
  if (!children) return null;
  return (
    <p role="alert" className="text-[13px] font-medium text-destructive">
      {children}
    </p>
  );
}

export function LoginForm() {
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
        <div className="mb-6 flex size-11 items-center justify-center border border-border bg-muted">
          <Building2 className="size-5 text-muted-foreground" />
        </div>
        <h1 className="text-[24px] font-bold leading-tight tracking-tight">
          Continue with {current.org}
        </h1>
        <p className="mt-2 text-[14.5px] leading-relaxed text-muted-foreground">
          <span className="font-medium text-foreground">{current.email}</span> uses single
          sign-on. You&rsquo;ll finish signing in with your organisation&rsquo;s identity provider.
        </p>
        <Button size="lg" className="mt-7 h-11 w-full font-semibold">
          Continue to {current.org}
        </Button>
        <form action={submitEmail}>
          <button
            type="submit"
            name="email"
            value=""
            className="mt-4 inline-flex items-center gap-1.5 text-[13.5px] font-medium text-muted-foreground hover:text-foreground"
          >
            <ArrowLeft className="size-3.5" />
            Use a different email
          </button>
        </form>
      </div>
    );
  }

  if (current.step === "password") {
    return (
      <div>
        <h1 className="text-[24px] font-bold leading-tight tracking-tight">Enter your password</h1>
        <p className="mt-2 flex flex-wrap items-center gap-x-2 text-[14px] text-muted-foreground">
          <span className="font-medium text-foreground">{current.email}</span>
          <form action={submitEmail} className="contents">
            <button
              type="submit"
              name="email"
              value=""
              className="font-medium text-primary underline-offset-4 hover:underline"
            >
              Change
            </button>
          </form>
        </p>

        <form action={submitPassword} className="mt-7 grid gap-5">
          <input type="hidden" name="email" value={current.email} />
          <div className="grid gap-2">
            <div className="flex items-baseline justify-between">
              <Label htmlFor="password" className="text-[13.5px] font-semibold">Password</Label>
              <Link href="/reset" className="text-[13px] font-medium text-primary underline-offset-4 hover:underline">
                Forgot?
              </Link>
            </div>
            <Input id="password" name="password" type="password" autoComplete="current-password" autoFocus className="h-11" required />
            <FieldError>{current.error}</FieldError>
          </div>
          <Button type="submit" size="lg" disabled={pwPending} className="h-11 w-full font-semibold">
            {pwPending && <Loader2 className="size-4 animate-spin" />}
            Sign in
          </Button>
        </form>
      </div>
    );
  }

  return (
    <form action={submitEmail} className="grid gap-2">
      <Label htmlFor="email" className="text-[13.5px] font-semibold">Email</Label>
      <Input
        id="email"
        name="email"
        type="email"
        autoComplete="email"
        placeholder="you@company.com"
        className="h-11"
        required
      />
      <FieldError>{current.error}</FieldError>
      <Button type="submit" size="lg" disabled={emailPending} className="mt-2 h-11 w-full font-semibold">
        {emailPending && <Loader2 className="size-4 animate-spin" />}
        Continue
      </Button>
      <p className="mt-1 text-[12.5px] leading-relaxed text-muted-foreground">
        If your organisation uses single sign-on, we&rsquo;ll take you there.
      </p>
    </form>
  );
}
