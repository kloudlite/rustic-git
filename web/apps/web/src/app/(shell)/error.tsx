"use client";

import { ErrorPage } from "@/components/app/error-page";

/** What a page shows when it threw. Every browse page throws the api's message
 *  when a call fails for a reason that is not "sign in" or "not found", so this is
 *  mostly "the service is unavailable" — which is why there is a retry and no
 *  stack trace. */
export default function ShellError(props: { error: Error & { digest?: string }; reset: () => void }) {
  return (
    <main className="mx-auto max-w-page px-6 pt-16 pb-16">
      <ErrorPage
        {...props}
        className="w-full max-w-auth"
        title="This page could not be loaded."
        body="The service is unavailable. Try again."
      />
    </main>
  );
}
