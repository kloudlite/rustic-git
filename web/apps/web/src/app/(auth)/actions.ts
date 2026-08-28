"use server";

import { enabledProviders, signIn, signOut } from "@/auth";
import { safeNext } from "@/app/(auth)/login/destination";

/** Sign-in is a server action, not a client call to an endpoint. Auth.js still
 *  needs its callback route for the provider's redirect, but nothing here is
 *  reachable as an API. */
export async function signInWithProvider(formData: FormData) {
  const provider = String(formData.get("provider"));
  // Only a provider this deployment actually registered: Auth.js would answer an
  // unknown id with an opaque error page, and the form never offers one anyway.
  if (!(provider in enabledProviders) || !enabledProviders[provider as keyof typeof enabledProviders]) return;
  // Re-validated here rather than trusted: this is a form field, so it arrives from the
  // browser and an absolute URL in it would be an open redirect through our own sign-in.
  await signIn(provider, { redirectTo: safeNext(String(formData.get("next") ?? "")) ?? "/" });
}

export async function signOutAction() {
  await signOut({ redirectTo: "/" });
}
