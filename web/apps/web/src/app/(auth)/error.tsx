"use client";

import { useEffect } from "react";
import { Button } from "@/components/ui/button";

/** What sign-in shows when it threw. Someone here is mid-sign-in and has no idea
 *  what a digest is, so the wording says what to do — try again — and the layout
 *  already supplies the centred column. Client component by Next's rule, not by
 *  choice. */
export default function AuthError({ error, reset }: { error: Error & { digest?: string }; reset: () => void }) {
  // The only place the real error goes. An auth error's message can carry an
  // address, a provider response, a callback URL — none of it belongs on screen.
  useEffect(() => console.error(error), [error]);

  return (
    <div>
      <p className="text-caption font-semibold uppercase tracking-eyebrow text-muted-foreground">
        Something went wrong
      </p>
      <h1 className="mt-3 text-title font-semibold tracking-title">We could not sign you in.</h1>
      <p className="mt-2 text-sm2 leading-relaxed text-muted-foreground">
        Nothing about your account changed. Try again.
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
