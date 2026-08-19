import Link from "next/link";
import type { Metadata } from "next";
import { LoginForm } from "@/components/auth/login-form";
import { AuthProviders } from "@/components/auth/auth-providers";
import { DevBypass } from "@/components/auth/dev-bypass";

export const metadata: Metadata = { title: "Sign in" };

export default function LoginPage() {
  return (
    <>
      <LoginForm oauth={<AuthProviders verb="Sign in" />} />
      <DevBypass />
      <p className="mt-8 border-t border-border pt-5 text-sm2 text-muted-foreground">
        New to kloudlite?{" "}
        <Link href="/signup" className="font-medium text-foreground underline-offset-4 hover:underline">
          Create an account
        </Link>
      </p>
    </>
  );
}
