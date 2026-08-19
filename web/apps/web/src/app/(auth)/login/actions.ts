"use server";

import { AuthError } from "next-auth";
import { signIn, passwordSignIn } from "@/auth";
import { routeForEmail } from "@/lib/sso";

export type LoginState =
  | { step: "email"; error?: string }
  | { step: "password"; email: string; error?: string }
  | { step: "sso"; email: string; org: string; provider: string };

/** Step one: we have an email and nothing else. Decide where it goes. */
export async function continueWithEmail(
  _prev: LoginState,
  formData: FormData,
): Promise<LoginState> {
  const email = String(formData.get("email") ?? "").trim();

  if (!/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(email)) {
    return { step: "email", error: "Enter a valid email address." };
  }

  const route = routeForEmail(email);
  if (route.kind === "sso") {
    // Real implementation redirects to the IdP here.
    return { step: "sso", email, org: route.org, provider: route.provider };
  }
  return { step: "password", email };
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
    return { step: "password", email, error: "Password sign-in is not available here. Use a provider above." };
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
