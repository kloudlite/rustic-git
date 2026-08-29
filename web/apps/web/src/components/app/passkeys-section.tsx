"use client";

import { useState } from "react";
import { useRouter } from "next/navigation";
import { startRegistration } from "@simplewebauthn/browser";
import { Fingerprint, Loader2, Plus, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { DeleteForm } from "@/components/app/delete-form";
import { beginPasskeyRegistration, finishPasskeyRegistration, removePasskey } from "@/app/(auth)/passkey/actions";
import type { ApiPasskey } from "@/lib/api";

/** Passkeys, listed and addable.
 *
 *  Adding one is a browser ceremony, not a form post: the authenticator has to be
 *  prompted from a user gesture, so this is a client component that calls two
 *  server actions around `startRegistration`. */
export function PasskeysSection({ passkeys }: { passkeys: ApiPasskey[] }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const router = useRouter();

  async function add() {
    setError(undefined);
    setBusy(true);
    try {
      const options = await beginPasskeyRegistration();
      if ("error" in options) {
        setError(options.error);
        return;
      }
      const response = await startRegistration({ optionsJSON: options });
      // Named after the platform doing the storing, which is the only thing the
      // browser will tell us and is usually what the person would have typed.
      const guess = response.authenticatorAttachment === "platform" ? "This device" : "Security key";
      const saved = await finishPasskeyRegistration(response, guess);
      if ("error" in saved) {
        setError(saved.error);
        return;
      }
      router.refresh();
    } catch (e) {
      const name = (e as { name?: string })?.name;
      if (name === "InvalidStateError") setError("This device already has a passkey for kloudlite.");
      else if (name !== "NotAllowedError" && name !== "AbortError") {
        setError("Could not create a passkey on this device.");
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <>
      <div className="flex items-center justify-between">
        <p className="text-sm2 text-muted-foreground">
          {passkeys.length} {passkeys.length === 1 ? "passkey" : "passkeys"}
        </p>
        <Button variant="outline" className="border-edge hover:border-edge-hover" onClick={add} disabled={busy}>
          {busy ? <Loader2 className="animate-spin" /> : <Plus />}
          Add passkey
        </Button>
      </div>
      {error && <p role="alert" className="mt-2 text-sm2 font-medium text-destructive">{error}</p>}

      {passkeys.length === 0 ? (
        <p className="mt-3 border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
          No passkeys yet. Add one and you can sign in with a fingerprint, face or PIN.
        </p>
      ) : (
        <ul className="mt-3 divide-y divide-border border border-border bg-card">
          {passkeys.map((p) => (
            <li key={p._id} className="flex items-center gap-4 px-4 py-3">
              <Fingerprint className="size-4 shrink-0 text-muted-foreground" />
              <div className="min-w-0 flex-1">
                <div className="text-sm2 font-medium">{p.name}</div>
                <div className="mt-0.5 truncate font-mono text-caption text-muted-foreground">{p._id.slice(0, 24)}…</div>
              </div>
              <DeleteForm action={removePasskey} fields={{ id: p._id }} confirm={`Remove ${p.name}? If it is your only passkey you will need another way to sign in.`}>
                <Button type="submit" variant="ghost" size="sm" className="text-muted-foreground hover:text-destructive" aria-label={`Remove ${p.name}`}>
                  <Trash2 />
                </Button>
              </DeleteForm>
            </li>
          ))}
        </ul>
      )}
    </>
  );
}
