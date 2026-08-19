import Link from "next/link";
import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { AuthCard, AuthFootnote } from "@/components/auth/auth-card";
import { AuthProviders } from "@/components/auth/auth-providers";
import { LoginForm } from "@/components/auth/login-form";

export const metadata: Metadata = { title: "Create an account" };

/** There is no separate registration: the first sign-in IS the registration — the
 *  api server records the person then, and the handle is chosen straight after at
 *  /welcome. So this page runs the same form as /login and only says it
 *  differently. It exists because people look for it, not because the flow
 *  differs, and it must not pretend to collect anything the flow does not use. */
export default async function SignupPage() {
  const session = await getSession();
  if (session) redirect(session.user.username ? "/" : "/welcome");

  return (
    <>
      <AuthCard>
        <LoginForm
          title="Create your account"
          subtitle="Use a provider, or your work email."
          submitLabel="Continue"
          oauth={<AuthProviders verb="Sign up" />}
        />
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
