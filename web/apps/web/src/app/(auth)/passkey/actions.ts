"use server";

import {
  generateAuthenticationOptions,
  generateRegistrationOptions,
  verifyAuthenticationResponse,
  verifyRegistrationResponse,
} from "@simplewebauthn/server";
import type {
  AuthenticationResponseJSON,
  PublicKeyCredentialCreationOptionsJSON,
  PublicKeyCredentialRequestOptionsJSON,
  RegistrationResponseJSON,
} from "@simplewebauthn/server";
import { revalidatePath } from "next/cache";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import * as api from "@/lib/api";
import type { DeleteState } from "@/components/app/delete-form";
import { relyingParty, rememberChallenge, takeChallenge } from "@/lib/passkey";
import { signAssertion } from "@/lib/assertion";

/* ── signing in ─────────────────────────────────────────────────────────── */

/** Options for a sign-in. `allowCredentials` is deliberately empty: the browser
 *  offers whatever discoverable credential it holds for this site, so the person
 *  never types an address — which is the whole appeal. */
export async function beginPasskeyLogin(): Promise<PublicKeyCredentialRequestOptionsJSON> {
  const { rpID } = await relyingParty();
  const options = await generateAuthenticationOptions({ rpID, userVerification: "preferred" });
  await rememberChallenge(options.challenge);
  return options;
}

export type PasskeyLoginResult = { assertion: string } | { error: string };

/** Verify the signature and, only then, mint the assertion Auth.js will accept.
 *  Nothing here signs anyone in — that happens on the client, which calls
 *  `signIn("passkey", { assertion })` with what this returns. */
export async function finishPasskeyLogin(
  response: AuthenticationResponseJSON,
): Promise<PasskeyLoginResult> {
  const expectedChallenge = await takeChallenge();
  if (!expectedChallenge) return { error: "That took too long. Try again." };

  const stored = await api.lookupPasskey(response.id);
  // An unknown credential and a failed signature give the same answer: a stranger
  // must not learn which passkeys exist here.
  if (!stored.ok) return { error: "That passkey was not recognised." };

  const { rpID, origin } = await relyingParty();
  let verification;
  try {
    verification = await verifyAuthenticationResponse({
      response,
      expectedChallenge,
      expectedOrigin: origin,
      expectedRPID: rpID,
      credential: {
        id: stored.value._id,
        publicKey: Buffer.from(stored.value.publicKey, "base64url"),
        counter: stored.value.counter,
        transports: stored.value.transports as never,
      },
    });
  } catch {
    return { error: "That passkey was not recognised." };
  }
  if (!verification.verified) return { error: "That passkey was not recognised." };

  // A counter that does not advance is the documented signal of a cloned
  // authenticator. Recorded on every success so the next attempt can be judged.
  await api.passkeyUsed(stored.value._id, verification.authenticationInfo.newCounter);

  return { assertion: signAssertion(stored.value.user) };
}

/* ── adding one ─────────────────────────────────────────────────────────── */

export async function beginPasskeyRegistration(): Promise<
  PublicKeyCredentialCreationOptionsJSON | { error: string }
> {
  const session = await getSession();
  if (!session) return { error: "Sign in first." };
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const existing = await api.listPasskeys(token);
  const { rpID, rpName } = await relyingParty();
  const options = await generateRegistrationOptions({
    rpID,
    rpName,
    userName: session.user.email,
    userDisplayName: session.user.name,
    attestationType: "none",
    // So the browser says "you already have one of these" instead of silently
    // enrolling the same authenticator twice.
    excludeCredentials: existing.ok ? existing.value.map((p) => ({ id: p._id })) : [],
    authenticatorSelection: { residentKey: "preferred", userVerification: "preferred" },
  });
  await rememberChallenge(options.challenge);
  return options;
}

export type AddPasskeyResult = { ok: true } | { error: string };

export async function finishPasskeyRegistration(
  response: RegistrationResponseJSON,
  name: string,
): Promise<AddPasskeyResult> {
  const session = await getSession();
  if (!session) return { error: "Sign in first." };
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };

  const expectedChallenge = await takeChallenge();
  if (!expectedChallenge) return { error: "That took too long. Try again." };

  const { rpID, origin } = await relyingParty();
  let verification;
  try {
    verification = await verifyRegistrationResponse({
      response,
      expectedChallenge,
      expectedOrigin: origin,
      expectedRPID: rpID,
    });
  } catch {
    return { error: "That passkey could not be verified." };
  }
  const info = verification.registrationInfo;
  if (!verification.verified || !info) return { error: "That passkey could not be verified." };

  const r = await api.addPasskey(token, {
    id: info.credential.id,
    publicKey: Buffer.from(info.credential.publicKey).toString("base64url"),
    counter: info.credential.counter,
    transports: response.response.transports ?? [],
    name: name.trim() || "Passkey",
  });
  if (!r.ok) {
    if (r.kind === "conflict") return { error: "That passkey is already registered." };
    return { error: r.message || "Could not save the passkey." };
  }
  revalidatePath("/settings");
  return { ok: true };
}

export async function removePasskey(_prev: DeleteState, formData: FormData): Promise<DeleteState> {
  const id = String(formData.get("id") ?? "");
  const token = await apiToken();
  if (!token) return { error: "Your session has expired. Sign in again." };
  if (!id) return { error: "No passkey named." };
  const r = await api.removePasskey(token, id);
  if (!r.ok) return { error: r.message || "Could not remove the passkey." };
  revalidatePath("/settings");
  return null;
}
