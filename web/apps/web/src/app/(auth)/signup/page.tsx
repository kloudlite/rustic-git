import Link from "next/link";
import type { Metadata } from "next";
import { Check } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { OAuthButtons, OrDivider } from "@/components/auth/oauth-buttons";

export const metadata: Metadata = { title: "Create an account" };

export default function SignupPage() {
  return (
    <div>
      <div className="mb-8">
        <h1 className="text-[26px] font-bold leading-tight tracking-tight text-foreground">
          Create an account
        </h1>
        <p className="mt-2 text-[14.5px] text-muted-foreground">
          Free for personal projects. No card required.
        </p>
      </div>

      <OAuthButtons verb="Sign up" />

      <div className="my-6">
        <OrDivider />
      </div>

      <form className="grid gap-5">
        <div className="grid gap-2">
          <Label htmlFor="name" className="text-[13.5px] font-semibold">Full name</Label>
          <Input id="name" name="name" autoComplete="name" placeholder="Ada Lovelace" className="h-11" required />
        </div>

        <div className="grid gap-2">
          <Label htmlFor="email" className="text-[13.5px] font-semibold">Work email</Label>
          <Input id="email" name="email" type="email" autoComplete="email" placeholder="you@company.com" className="h-11" required />
        </div>

        <div className="grid gap-2">
          <Label htmlFor="password" className="text-[13.5px] font-semibold">Password</Label>
          <Input id="password" name="password" type="password" autoComplete="new-password" className="h-11" required />
          <p className="text-[12.5px] text-muted-foreground">
            At least 12 characters, with a number or symbol.
          </p>
        </div>

        <Button type="submit" size="lg" className="mt-1 h-11 w-full font-semibold">
          Create account
        </Button>

        <p className="text-[12.5px] leading-relaxed text-muted-foreground">
          By creating an account you agree to our{" "}
          <Link href="/terms" className="text-foreground underline underline-offset-2">Terms</Link> and{" "}
          <Link href="/privacy" className="text-foreground underline underline-offset-2">Privacy Policy</Link>.
        </p>
      </form>

      <p className="mt-7 text-[14px] text-muted-foreground">
        Already have an account?{" "}
        <Link href="/login" className="font-semibold text-primary underline-offset-4 hover:underline">
          Sign in
        </Link>
      </p>
    </div>
  );
}
