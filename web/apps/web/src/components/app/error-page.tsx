"use client";

import { useEffect } from "react";
import { log, reason } from "@/lib/log";
import { Button } from "@/components/ui/button";

/** The body every route-group error.tsx renders: eyebrow, headline, one reassuring
 *  sentence, the digest, retry. Only the words and the wrapper class differ per
 *  group, so they pass those and nothing else. Client component by Next's rule. */
const logger = log("web::error-page");

export function ErrorPage({
  error,
  reset,
  title,
  body,
  className,
}: {
  error: Error & { digest?: string };
  reset: () => void;
  title: string;
  body: string;
  className?: string;
}) {
  // The only place the real error goes -- a message can carry a handle, a query, a
  // path, a token, a provider response. None of it belongs on screen.
  useEffect(
    () => logger.error("page.render.failed", { boundary: title, digest: error.digest, error: reason(error) }),
    [error, title],
  );

  return (
    <div className={className}>
      <p className="text-caption font-semibold uppercase tracking-eyebrow text-muted-foreground">
        Something went wrong
      </p>
      <h1 className="mt-3 text-title font-semibold tracking-title">{title}</h1>
      <p className="mt-2 text-sm2 leading-relaxed text-muted-foreground">{body}</p>
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
