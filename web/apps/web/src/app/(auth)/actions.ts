"use server";

import { signIn, signOut } from "@/auth";

/** Sign-in is a server action, not a client call to an endpoint. Auth.js still
 *  needs its callback route for the provider's redirect, but nothing here is
 *  reachable as an API. */
export async function signInWithProvider(formData: FormData) {
  const provider = String(formData.get("provider"));
  await signIn(provider, { redirectTo: "/" });
}

export async function signOutAction() {
  await signOut({ redirectTo: "/" });
}
