"use server";

import { redirect } from "next/navigation";
import { AuthError } from "next-auth";
import { signIn } from "@/auth";
import { redeemSignInLink } from "@/lib/api";
import { signAssertion } from "@/lib/assertion";
import { safeNext } from "@/app/(auth)/login/destination";

/** Spends the emailed token and signs the browser in. A Server Action, and so a POST: the
 *  link itself (a GET) must not do this, or anyone who can make a browser open a URL can sign
 *  that browser into an account of their choosing. The token stays single-use — the api
 *  enforces that, this only adds the button press in front of it.
 *
 *  `signIn` writes the session cookie, which Next allows only from a Server Action or a Route
 *  Handler, and redirects by throwing — so only an AuthError is caught. */
export async function redeemLink(formData: FormData) {
  const token = String(formData.get("token") ?? "");
  // Re-validated here rather than trusted: this is a form field, so it arrives from the browser.
  const next = safeNext(String(formData.get("next") ?? "")) ?? "/";
  const r = await redeemSignInLink(token);
  if (!r.ok) redirect("/login?from=link");
  try {
    await signIn("email-link", { assertion: signAssertion(r.value.email), redirectTo: next });
  } catch (error) {
    if (!(error instanceof AuthError)) throw error;
  }
  redirect(next);
}
