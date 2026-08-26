"use server";

import { AuthError } from "next-auth";
import { signIn, signOut, passwordSignIn, emailLinkSignIn } from "@/auth";
import { requestSignInLink } from "@/lib/api";
import { sendSignInLink } from "@/lib/mail";

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

  if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email)) {
    return { step: "email", error: "Enter a valid email address." };
  }
  if (passwordSignIn) return { step: "password", email };
  if (!emailLinkSignIn) {
    return { step: "email", error: "Email sign-in is not available here. Use a provider or a passkey above." };
  }
  const r = await requestSignInLink(email);
  if (!r.ok) return { step: "email", error: "Could not send a sign-in link. Try again." };
  const base = (process.env.AUTH_URL ?? "").replace(/\/$/, "");
  const mail = await sendSignInLink(r.value.email, `${base}/verify/${r.value.token}`);
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
    await signIn("credentials", { email, password, redirectTo: "/" });
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
