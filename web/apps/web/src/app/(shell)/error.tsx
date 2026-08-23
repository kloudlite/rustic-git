"use client";

import { Button } from "@/components/ui/button";

/** What a page shows when it threw. Every browse page throws the api's message
 *  when a call fails for a reason that is not "sign in" or "not found", so this is
 *  mostly "the service is unavailable" — which is why there is a retry and no
 *  stack trace. Client component by Next's rule, not by choice. */
export default function ShellError({ error, reset }: { error: Error & { digest?: string }; reset: () => void }) {
  return (
    <main className="mx-auto max-w-page px-6 pt-16 pb-16">
      <div className="w-full max-w-auth">
        <p className="text-caption font-semibold uppercase tracking-eyebrow text-muted-foreground">
          Something went wrong
        </p>
        <h1 className="mt-3 text-title font-semibold tracking-title">This page could not be loaded.</h1>
        <p className="mt-2 text-sm2 leading-relaxed text-muted-foreground">
          {error.message || "The service is unavailable. Try again."}
        </p>
        <Button onClick={reset} variant="outline" className="mt-6 border-edge hover:border-edge-hover">
          Try again
        </Button>
      </div>
    </main>
  );
}
