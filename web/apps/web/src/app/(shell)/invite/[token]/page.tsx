import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { previewInvite } from "@/lib/api";
import { AcceptInvite } from "@/components/app/accept-invite";

export const metadata: Metadata = { title: "Invitation" };

/** The landing for an invitation link. Signed out, the person is sent to sign in and opens
 *  the link again afterwards — sign-in does not carry a return address today, and the email
 *  still has the link. The api shows nothing for a token without a session, and nothing for
 *  one that is spent, expired or made up.
 *  ponytail: no return-to after sign-in; add one to /login when a second page wants it. */
export default async function InvitePage({ params }: { params: Promise<{ token: string }> }) {
  const { token } = await params;
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");
  const api = await apiToken();
  if (!api) redirect("/login");

  const preview = await previewInvite(api, token);
  return (
    <main className="mx-auto max-w-page px-6 pt-8 pb-16">
      <AcceptInvite token={token} preview={preview.ok ? preview.value : null} me={session.user.email} />
    </main>
  );
}
