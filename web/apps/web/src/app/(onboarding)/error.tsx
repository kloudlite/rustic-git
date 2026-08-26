"use client";

import { useEffect } from "react";
import { Button } from "@/components/ui/button";

/** What onboarding shows when it threw. The person is signed in but has no handle
 *  yet, so the reassurance that matters is that they are still signed in and
 *  nothing was half-created. Client component by Next's rule, not by choice. */
export default function OnboardingError({ error, reset }: { error: Error & { digest?: string }; reset: () => void }) {
  // The only place the real error goes; the message can carry the handle that was
  // being claimed and the api's reason for refusing it.
  useEffect(() => console.error(error), [error]);

  return (
    <div className="w-full max-w-auth">
      <p className="text-caption font-semibold uppercase tracking-eyebrow text-muted-foreground">
        Something went wrong
      </p>
      <h1 className="mt-3 text-title font-semibold tracking-title">We could not finish setting you up.</h1>
      <p className="mt-2 text-sm2 leading-relaxed text-muted-foreground">
        You are still signed in and nothing was created. Try again.
      </p>
      {/* Enough to find this exact failure in the logs, and nothing more. */}
      {error.digest && (
        <p className="mt-2 font-mono text-caption text-muted-foreground">Reference {error.digest}</p>
      )}
      <Button onClick={reset} variant="outline" className="mt-6 border-edge hover:border-edge-hover">
        Try again
      </Button>
    </div>
  );
}
