import { redirect } from "next/navigation";
import { AuthError } from "next-auth";
import { signIn } from "@/auth";
import { redeemSignInLink } from "@/lib/api";
import { signAssertion } from "@/lib/assertion";
import { safeNext } from "@/app/(auth)/login/destination";

/** Where the emailed link lands. The token is spent against the api here — a GET with a side
 *  effect, which is normally wrong and right here: the link is single-use by construction, mail
 *  clients prefetch nothing that requires a session, and a confirm button would be one more
 *  click between someone and the thing they just asked for.
 *
 *  A Route Handler, not a page: `signIn` writes the session cookie, and Next only allows a
 *  cookie write from a Server Action or a Route Handler — as a page this threw on every link
 *  ("Cookies can only be modified in a Server Action or Route Handler") and nobody could sign in.
 *  `signIn` redirects by throwing; only an AuthError is caught. */
export async function GET(req: Request, { params }: { params: Promise<{ token: string }> }) {
  const { token } = await params;
  // Carried in the emailed link: the browser opening it may not be the one that asked.
  const next = safeNext(new URL(req.url).searchParams.get("next") ?? undefined) ?? "/";
  const r = await redeemSignInLink(token);
  if (!r.ok) redirect("/login?from=link");
  try {
    await signIn("email-link", { assertion: signAssertion(r.value.email), redirectTo: next });
  } catch (error) {
    if (!(error instanceof AuthError)) throw error;
  }
  redirect(next);
}
