"use server";

import { AuthError } from "next-auth";
import { headers } from "next/headers";
import { signIn, signOut, passwordSignIn, emailLinkSignIn } from "@/auth";
import { requestSignInLink } from "@/lib/api";
import { sendSignInLink } from "@/lib/mail";
import { safeNext } from "./destination";

export type LoginState =
  | { step: "email"; error?: string }
  | { step: "password"; email: string; error?: string }
  /** A link went out; the page now only tells them where to look. */
  | { step: "sent"; email: string };

/** Step one: we have an email and nothing else. The preview password, when a deployment has
 *  one, takes precedence — it exists for environments with no mail. Otherwise a sign-in link
 *  goes out, and the first click on it IS the sign-up: the api records the person then. */
export async function continueWithEmail(
  _prev: LoginState,
  formData: FormData,
): Promise<LoginState> {
  const email = String(formData.get("email") ?? "").trim().toLowerCase();
  // "Change email" submits an empty address on purpose: it wants the first step back, not a
  // complaint about the address it just cleared.
  if (email === "") return { step: "email" };

  if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email)) {
    return { step: "email", error: "Enter a valid email address." };
  }
  if (passwordSignIn) return { step: "password", email };
  if (!emailLinkSignIn) {
    return { step: "email", error: "Email sign-in is not available here. Use a provider or a passkey above." };
  }
  // The ingress's view of the browser (deploy/ingress-nginx-config.yaml), handed on so the
  // api's per-address bucket keys on the person and not on this pod.
  const ip = (await headers()).get("x-real-ip") ?? undefined;
  const r = await requestSignInLink(email, ip);
  if (!r.ok) return { step: "email", error: "Could not send a sign-in link. Try again." };
  const base = (process.env.AUTH_URL ?? "").replace(/\/$/, "");
  // The link leaves this browser, so `next` rides in the URL rather than in any state here —
  // that is what makes the mail work when it is opened on the phone instead.
  const next = safeNext(String(formData.get("next") ?? ""));
  const onward = next ? `?next=${encodeURIComponent(next)}` : "";
  const mail = await sendSignInLink(r.value.email, `${base}/verify/${r.value.token}${onward}`);
  // Deliberately the same answer whether or not the address exists anywhere: this page must
  // not become a way to find out who has an account.
  if (!mail.sent) return { step: "email", error: "Could not send a sign-in link. Try again." };
  return { step: "sent", email };
}

/** Step two, password path. On success `signIn` redirects, which it does by
 *  throwing — so only an AuthError is caught here, never the redirect. */
export async function signInWithPassword(
  _prev: LoginState,
  formData: FormData,
): Promise<LoginState> {
  const email = String(formData.get("email") ?? "");
  const password = String(formData.get("password") ?? "");
  if (password.length < 1) {
    return { step: "password", email, error: "Enter your password." };
  }
  if (!passwordSignIn) {
    return { step: "email", error: "Password sign-in is not available here. Use a provider or a passkey above." };
  }
  try {
    await signIn("credentials", { email, password, redirectTo: safeNext(String(formData.get("next") ?? "")) ?? "/" });
  } catch (error) {
    if (error instanceof AuthError) {
      // Deliberately does not say which half was wrong.
      return { step: "password", email, error: "Incorrect email or password." };
    }
    throw error;
  }
  return { step: "password", email };
}

/** The way out of a session whose api token is dead. Deliberately an action and
 *  not something the page does while rendering: signing out is a side effect, and
 *  a GET that destroys the session fires on every prefetch and every refresh. */
export async function signOutExpired() {
  await signOut({ redirectTo: "/login" });
}
