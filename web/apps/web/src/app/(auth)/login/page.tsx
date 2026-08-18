import Link from "next/link";
import type { Metadata } from "next";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { OAuthButtons, OrDivider } from "@/components/auth/oauth-buttons";

export const metadata: Metadata = { title: "Sign in" };

export default function LoginPage() {
  return (
    <div>
      <div className="mb-8">
        <h1 className="text-[26px] font-bold leading-tight tracking-tight text-foreground">
          Sign in
        </h1>
        <p className="mt-2 text-[14.5px] text-muted-foreground">
          Continue to your repositories and environments.
        </p>
      </div>

      <OAuthButtons verb="Sign in" />

      <div className="my-6">
        <OrDivider />
      </div>

      <form className="grid gap-5">
        <div className="grid gap-2">
          <Label htmlFor="email" className="text-[13.5px] font-semibold">
            Work email
          </Label>
          <Input
            id="email"
            name="email"
            type="email"
            autoComplete="email"
            placeholder="you@company.com"
            className="h-11"
            required
          />
        </div>

        <div className="grid gap-2">
          <div className="flex items-baseline justify-between">
            <Label htmlFor="password" className="text-[13.5px] font-semibold">
              Password
            </Label>
            <Link
              href="/reset"
              className="text-[13px] font-medium text-primary underline-offset-4 hover:underline"
            >
              Forgot?
            </Link>
          </div>
          <Input
            id="password"
            name="password"
            type="password"
            autoComplete="current-password"
            className="h-11"
            required
          />
        </div>

        <Button type="submit" size="lg" className="mt-1 h-11 w-full font-semibold">
          Sign in
        </Button>
      </form>

      <p className="mt-7 text-[14px] text-muted-foreground">
        New to kloudlite?{" "}
        <Link href="/signup" className="font-semibold text-primary underline-offset-4 hover:underline">
          Create an account
        </Link>
      </p>
    </div>
  );
}
