"use server";

import { enabledProviders, signIn, signOut } from "@/auth";

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
  await signOut({ redirectTo: "/" });
}
