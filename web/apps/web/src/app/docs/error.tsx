"use client";

import { ErrorPage } from "@/components/app/error-page";

/** What a documentation page shows when it threw. The markdown is read from disk and rendered
 *  at request time, so a missing content tree or an unreadable file is a throw, not a 404 —
 *  without this boundary it took the root error page and the docs chrome with it. */
export default function DocsError(props: { error: Error & { digest?: string }; reset: () => void }) {
  return (
    <main className="docs-main">
      <ErrorPage
        {...props}
        className="w-full max-w-auth"
        title="This page could not be loaded."
        body="The documentation is unavailable. Try again."
      />
    </main>
  );
}
