import Link from "next/link";
import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { LoginForm } from "@/components/auth/login-form";
import { AuthProviders } from "@/components/auth/auth-providers";
import { AuthCard, AuthFootnote } from "@/components/auth/auth-card";
import { Button } from "@/components/ui/button";
import { loginDestination, safeNext } from "./destination";
import { signOutExpired } from "./actions";

export const metadata: Metadata = { title: "Sign in" };

export default async function LoginPage({ searchParams }: { searchParams: Promise<{ from?: string; next?: string }> }) {
  // Read BEFORE any redirect decision: `from` is what says this caller was sent
  // here by a refused token, and sending them onward is the loop.
  const { from, next: raw } = await searchParams;
  // Validated once, here: every place below hands it to a redirect.
  const next = safeNext(raw);
  const session = await getSession();
  const token = session ? await apiToken() : undefined;

  const to = loginDestination({
    hasSession: Boolean(session),
    hasToken: Boolean(token),
    username: session?.user.username,
    from,
    next,
  });
  if (to) redirect(to);

  const notice =
    from === "expired"
      ? "Your session expired. Sign in again to continue."
      : from === "link"
        ? "That sign-in link is no longer valid — links work once and expire after 15 minutes. Ask for a new one and use the most recent email."
        : undefined;

  return (
    <>
      <AuthCard>
        <LoginForm
          oauth={<AuthProviders verb="Sign in" next={next} />}
          notice={notice}
          next={next}
        />
        {session && (
          /* Still holding a session cookie, but the api will not take its token.
             Signing back in on top of it re-mints one; this is the explicit way
             to clear it first, and the only thing that ends the session. */
          <form action={signOutExpired} className="mt-5 text-center">
            <Button type="submit" variant="link" className="h-auto p-0 text-sm2">
              Sign out of {session.user.email}
            </Button>
          </form>
        )}
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
