"use server";

import { cookies } from "next/headers";
import { redirect } from "next/navigation";
import { enabledProviders, signIn, signOut } from "@/auth";
import { DEV_BYPASS, DEV_SIGNED_OUT_COOKIE } from "@/lib/dev-auth";

/** Sign-in is a server action, not a client call to an endpoint. Auth.js still
 *  needs its callback route for the provider's redirect, but nothing here is
 *  reachable as an API. */
export async function signInWithProvider(formData: FormData) {
  const provider = String(formData.get("provider"));
  // Only a provider this deployment actually registered: Auth.js would answer an
  // unknown id with an opaque error page, and the form never offers one anyway.
  if (!(provider in enabledProviders) || !enabledProviders[provider as keyof typeof enabledProviders]) return;
  await signIn(provider, { redirectTo: "/" });
}

export async function signOutAction() {
  /* Under the bypass there is no real session to end, so sign-out has to be
     recorded somewhere the next request will see. */
  if (DEV_BYPASS) {
    (await cookies()).set(DEV_SIGNED_OUT_COOKIE, "1", { path: "/", httpOnly: true, sameSite: "lax" });
    redirect("/");
  }
  await signOut({ redirectTo: "/" });
}

/** Development only: clears the opt-out set by signOutAction. */
export async function devSignIn() {
  if (!DEV_BYPASS) return;
  (await cookies()).delete(DEV_SIGNED_OUT_COOKIE);
  redirect("/");
}
