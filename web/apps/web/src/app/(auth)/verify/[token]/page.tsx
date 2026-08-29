import type { Metadata } from "next";
import { AuthCard, AuthHeader } from "@/components/auth/auth-card";
import { Button } from "@/components/ui/button";
import { safeNext } from "@/app/(auth)/login/destination";
import { redeemLink } from "./actions";

export const metadata: Metadata = { title: "Sign in" };

/** Where the emailed link lands. Deliberately inert: opening it spends nothing and sets
 *  nothing, so a mail client's prefetch cannot burn the token and a link planted by someone
 *  else cannot sign this browser into their account. The button is the gesture; the POST
 *  behind it does the work. */
export default async function VerifyPage({
  params,
  searchParams,
}: {
  params: Promise<{ token: string }>;
  searchParams: Promise<{ next?: string }>;
}) {
  const { token } = await params;
  // Carried in the emailed link: the browser opening it may not be the one that asked.
  const next = safeNext((await searchParams).next) ?? "/";
  return (
    <AuthCard>
      <AuthHeader title="Finish signing in">
        Continue only if you asked for this link. It works once and expires after 15 minutes.
      </AuthHeader>
      <form action={redeemLink}>
        <input type="hidden" name="token" value={token} />
        <input type="hidden" name="next" value={next} />
        <Button type="submit" className="w-full">Continue</Button>
      </form>
    </AuthCard>
  );
}
