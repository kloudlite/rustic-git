import Link from "next/link";
import type { Metadata } from "next";
import { LoginForm } from "@/components/auth/login-form";
import { AuthProviders } from "@/components/auth/auth-providers";
import { DevBypass } from "@/components/auth/dev-bypass";
import { AuthCard, AuthFootnote } from "@/components/auth/auth-card";

export const metadata: Metadata = { title: "Sign in" };

export default function LoginPage() {
  return (
    <>
      <AuthCard>
        <LoginForm oauth={<AuthProviders verb="Sign in" />} />
      </AuthCard>
      <AuthFootnote>
        New to kloudlite?{" "}
        <Link href="/signup" className="font-medium text-foreground underline-offset-4 hover:underline">
          Create an account
        </Link>
      </AuthFootnote>
      <DevBypass />
    </>
  );
}
