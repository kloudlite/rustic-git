"use client";

import { ErrorPage } from "@/components/app/error-page";

/** What sign-in shows when it threw. Someone here is mid-sign-in and has no idea
 *  what a digest is, so the wording says what to do — try again — and the layout
 *  already supplies the centred column. */
export default function AuthError(props: { error: Error & { digest?: string }; reset: () => void }) {
  return (
    <ErrorPage
      {...props}
      title="We could not sign you in."
      body="Nothing about your account changed. Try again."
    />
  );
}
