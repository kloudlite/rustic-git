import Link from "next/link";
import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { AuthCard, AuthFootnote, AuthHeader } from "@/components/auth/auth-card";
import { AuthProviders } from "@/components/auth/auth-providers";
import { enabledProviders } from "@/auth";

export const metadata: Metadata = { title: "Create an account" };

/** There is no separate registration: signing in with a provider for the first
 *  time IS the registration. The api server records the person on that first
 *  sign-in, and the handle is chosen straight after at /welcome.
 *
 *  So this page offers exactly what /login does. It exists because people look
 *  for it — not because the flow differs. */
export default async function SignupPage() {
  const session = await getSession();
  if (session) redirect(session.user.username ? "/" : "/welcome");

  const anyProvider = Object.values(enabledProviders).some(Boolean);

  return (
    <>
      <AuthCard>
        <AuthHeader title="Create your account">
          Continue with a provider. Your first sign-in creates the account.
        </AuthHeader>

        <AuthProviders verb="Sign up" />

        {!anyProvider && (
          <p className="text-sm2 leading-relaxed text-muted-foreground">
            No sign-in provider is configured on this deployment yet. Ask an
            administrator to add one, then come back.
          </p>
        )}

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
