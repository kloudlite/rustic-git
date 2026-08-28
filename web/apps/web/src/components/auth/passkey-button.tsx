"use client";

import { useState } from "react";
import { signIn } from "next-auth/react";
import { startAuthentication } from "@simplewebauthn/browser";
import { Fingerprint, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { beginPasskeyLogin, finishPasskeyLogin } from "@/app/(auth)/passkey/actions";

/** Sign in with whatever passkey the browser holds for this site.
 *
 *  No email field: the credential is discoverable, so the authenticator names the
 *  account. The server verifies the signature and returns a short-lived assertion;
 *  only that is handed to Auth.js, because the provider's callback URL is public
 *  and would otherwise accept any address anyone cared to post. */
export function PasskeyButton({ next }: { next?: string }) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();

  async function go() {
    setError(undefined);
    setBusy(true);
    try {
      const options = await beginPasskeyLogin();
      const response = await startAuthentication({ optionsJSON: options });
      const result = await finishPasskeyLogin(response);
      if ("error" in result) {
        setError(result.error);
        return;
      }
      await signIn("passkey", { assertion: result.assertion, redirectTo: next ?? "/" });
    } catch (e) {
      // A cancelled prompt throws, and is not an error worth shouting about.
      const name = (e as { name?: string })?.name;
      if (name !== "NotAllowedError" && name !== "AbortError") {
        setError("Could not use a passkey on this device.");
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="grid gap-2">
      <Button
        type="button"
        variant="outline"
        size="lg"
        onClick={go}
        disabled={busy}
        className="w-full justify-start gap-3 px-4"
      >
        <span className="flex w-4.5 shrink-0 justify-center">
          {busy ? <Loader2 className="size-4.5 animate-spin" /> : <Fingerprint className="size-4.5" />}
        </span>
        <span>Continue with a passkey</span>
      </Button>
      {error && <p role="alert" className="text-caption font-medium text-destructive">{error}</p>}
    </div>
  );
}
