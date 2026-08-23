import Link from "next/link";
import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { LoginForm } from "@/components/auth/login-form";
import { AuthProviders } from "@/components/auth/auth-providers";
import { AuthCard, AuthFootnote } from "@/components/auth/auth-card";

export const metadata: Metadata = { title: "Sign in" };

export default async function LoginPage({ searchParams }: { searchParams: Promise<{ from?: string }> }) {
  const session = await getSession();
  // A signed-in person landing here means "take me in", not "sign in again" —
  // and if they have no handle yet, in means /welcome.
  if (session) redirect(session.user.username ? "/" : "/welcome");
  const { from } = await searchParams;

  return (
    <>
      <AuthCard>
        <LoginForm
          oauth={<AuthProviders verb="Sign in" />}
          notice={from === "expired" ? "Your session expired. Sign in again to continue." : undefined}
        />
      </AuthCard>
      <AuthFootnote>
        New to kloudlite?{" "}
        <Link href="/signup" className="font-medium text-foreground underline-offset-4 hover:underline">
          Create an account
        </Link>
      </AuthFootnote>
    </>
  );
}
