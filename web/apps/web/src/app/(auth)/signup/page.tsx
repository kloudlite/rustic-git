import Link from "next/link";
import type { Metadata } from "next";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { AuthCard, AuthFootnote, AuthHeader, FieldLabel } from "@/components/auth/auth-card";
import { AuthProviders } from "@/components/auth/auth-providers";

export const metadata: Metadata = { title: "Create an account" };

export default function SignupPage() {
  return (
    <>
      <AuthCard>
        <AuthHeader title="Create your account">
          Use your work email to be placed with your organisation.
        </AuthHeader>

        <AuthProviders verb="Sign up" />

        <form className="grid gap-4">
          <div className="grid gap-2">
            <FieldLabel htmlFor="name">Full name</FieldLabel>
            <Input id="name" name="name" autoComplete="name" placeholder="Ada Lovelace" className="h-10" required />
          </div>

          <div className="grid gap-2">
            <FieldLabel htmlFor="email">Work email</FieldLabel>
            <Input id="email" name="email" type="email" autoComplete="email" placeholder="you@company.com" className="h-10" required />
          </div>

          <div className="grid gap-2">
            <FieldLabel htmlFor="password">Password</FieldLabel>
            <Input id="password" name="password" type="password" autoComplete="new-password" className="h-10" required />
            <p className="text-caption leading-relaxed text-muted-foreground">
              At least 12 characters, with a number or symbol.
            </p>
          </div>

          <Button type="submit" size="lg" className="mt-2 w-full">
            Create account
          </Button>
        </form>

        <p className="mt-4 text-center text-caption leading-relaxed text-muted-foreground">
          By continuing you agree to the{" "}
          <a href="https://kloudlite.io/terms" className="text-foreground underline underline-offset-2">Terms</a> and{" "}
          <a href="https://kloudlite.io/privacy" className="text-foreground underline underline-offset-2">Privacy Policy</a>.
        </p>
      </AuthCard>

      <AuthFootnote>
        Already have an account?{" "}
        <Link href="/login" className="font-medium text-foreground underline-offset-4 hover:underline">
          Sign in
        </Link>
      </AuthFootnote>
    </>
  );
}
