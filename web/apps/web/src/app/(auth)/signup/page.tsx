import Link from "next/link";
import type { Metadata } from "next";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { AuthHeader, FieldLabel } from "@/components/auth/auth-card";
import { OAuthButtons, OrDivider } from "@/components/auth/oauth-buttons";

export const metadata: Metadata = { title: "Create an account" };

export default function SignupPage() {
  return (
    <>
      <AuthHeader title="Create your account">Free for personal projects. No card required.</AuthHeader>

      <OAuthButtons verb="Sign up" />

      <div className="my-6">
        <OrDivider />
      </div>

      <form className="grid gap-4">
        <div className="grid gap-2">
          <FieldLabel htmlFor="name">Full name</FieldLabel>
          <Input id="name" name="name" autoComplete="name" placeholder="Ada Lovelace" className="h-11 text-[14px]" required />
        </div>

        <div className="grid gap-2">
          <FieldLabel htmlFor="email">Work email</FieldLabel>
          <Input id="email" name="email" type="email" autoComplete="email" placeholder="you@company.com" className="h-11 text-[14px]" required />
        </div>

        <div className="grid gap-2">
          <FieldLabel htmlFor="password">Password</FieldLabel>
          <Input id="password" name="password" type="password" autoComplete="new-password" className="h-11 text-[14px]" required />
          <p className="text-[12.5px] leading-relaxed text-muted-foreground">
            At least 12 characters, with a number or symbol.
          </p>
        </div>

        <Button type="submit" className="mt-2 h-11 w-full text-[14px] font-semibold">
          Create account
        </Button>
      </form>

      <p className="mt-4 text-[12.5px] leading-relaxed text-muted-foreground">
        By creating an account you agree to our{" "}
        <Link href="/terms" className="text-foreground underline underline-offset-2">Terms</Link> and{" "}
        <Link href="/privacy" className="text-foreground underline underline-offset-2">Privacy Policy</Link>.
      </p>

      <p className="mt-8 border-t border-border pt-5 text-[13.5px] text-muted-foreground">
        Already have an account?{" "}
        <Link href="/login" className="font-semibold text-foreground underline-offset-4 hover:underline">
          Sign in
        </Link>
      </p>
    </>
  );
}
