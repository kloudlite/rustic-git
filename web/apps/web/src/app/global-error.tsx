"use client";

import { useEffect } from "react";
import { log, reason } from "@/lib/log";
import "./globals.css";

/** The boundary UNDER the root layout: what renders when the layout itself threw (a font, the
 *  theme provider), which no route group's `error.tsx` can catch. It replaces the whole document,
 *  so it carries its own `html`/`body` and its own stylesheet, and stays plain — the shell's
 *  Button and tokens come from the tree that just failed. */
const logger = log("web::global-error");

export default function GlobalError({ error, reset }: { error: Error & { digest?: string }; reset: () => void }) {
  // The only place the real error goes; see `(shell)/error.tsx` for why it is not rendered.
  useEffect(() => logger.error("page.render.failed", { boundary: "global", digest: error.digest, error: reason(error) }), [error]);

  return (
    <html lang="en">
      <body className="bg-background text-foreground">
        <main className="mx-auto max-w-page px-6 pt-16 pb-16">
          <div className="w-full max-w-auth">
            <p className="text-caption font-semibold uppercase tracking-eyebrow text-muted-foreground">
              Something went wrong
            </p>
            <h1 className="mt-3 text-title font-semibold tracking-title">This page could not be loaded.</h1>
            <p className="mt-2 text-sm2 leading-relaxed text-muted-foreground">The service is unavailable. Try again.</p>
            {error.digest && (
              <p className="mt-2 font-mono text-caption text-muted-foreground">Reference {error.digest}</p>
            )}
            <button
              type="button"
              onClick={reset}
              className="mt-6 inline-flex h-9 items-center border border-edge px-4 text-sm2 font-medium hover:border-edge-hover"
            >
              Try again
            </button>
          </div>
        </main>
      </body>
    </html>
  );
}
