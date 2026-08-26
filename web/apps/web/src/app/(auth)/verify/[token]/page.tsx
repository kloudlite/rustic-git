import type { Metadata } from "next";
import { redirect } from "next/navigation";
import Link from "next/link";
import { AuthError } from "next-auth";
import { signIn } from "@/auth";
import { redeemSignInLink } from "@/lib/api";
import { signAssertion } from "@/lib/assertion";
import { AuthCard, AuthFootnote, AuthHeader } from "@/components/auth/auth-card";

export const metadata: Metadata = { title: "Signing you in" };

/** Where the emailed link lands. The token is spent against the api on this render — which
 *  is a GET, and a GET with a side effect is normally wrong. It is right here: the link is
 *  single-use by construction, mail clients prefetch nothing that requires a session, and a
 *  confirm button would be one more click between someone and the thing they just asked for.
 *
 *  `signIn` redirects by throwing; only an AuthError is caught. */
export default async function VerifyPage({ params }: { params: Promise<{ token: string }> }) {
  const { token } = await params;
  const r = await redeemSignInLink(token);
  if (r.ok) {
    try {
      await signIn("email-link", { assertion: signAssertion(r.value.email), redirectTo: "/" });
    } catch (error) {
      if (!(error instanceof AuthError)) throw error;
    }
  }
  if (r.ok) redirect("/");
  return (
    <>
      <AuthCard>
        <AuthHeader title="That link is no longer valid">
          Sign-in links work once and expire after 15 minutes. Ask for a new one and use the
          most recent email.
        </AuthHeader>
      </AuthCard>
      <AuthFootnote>
        <Link href="/login" className="font-medium text-foreground underline-offset-4 hover:underline">
          Back to sign in
        </Link>
      </AuthFootnote>
    </>
  );
}
