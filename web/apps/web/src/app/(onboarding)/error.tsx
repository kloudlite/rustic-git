"use client";

import { ErrorPage } from "@/components/app/error-page";

/** What onboarding shows when it threw. The person is signed in but has no handle
 *  yet, so the reassurance that matters is that they are still signed in and
 *  nothing was half-created. */
export default function OnboardingError(props: { error: Error & { digest?: string }; reset: () => void }) {
  return (
    <ErrorPage
      {...props}
      className="w-full max-w-auth"
      title="We could not finish setting you up."
      body="You are still signed in and nothing was created. Try again."
    />
  );
}
