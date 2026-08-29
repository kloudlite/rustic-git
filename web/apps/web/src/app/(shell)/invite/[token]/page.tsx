import type { Metadata } from "next";
import { previewInvite } from "@/lib/api";
import { AcceptInvite } from "@/components/app/accept-invite";
import { requireToken } from "@/lib/session";

export const metadata: Metadata = { title: "Invitation" };

/** The landing for an invitation link. Signed out, the person is sent to sign in and comes
 *  straight back here. The api shows nothing for a token without a session, and nothing for
 *  one that is spent, expired or made up. */
export default async function InvitePage({ params }: { params: Promise<{ token: string }> }) {
  const { token } = await params;
  const { session, token: api } = await requireToken(`/invite/${token}`);

  const preview = await previewInvite(api, token);
  return (
    <main className="mx-auto max-w-page px-6 pt-8 pb-16">
      <AcceptInvite token={token} preview={preview.ok ? preview.value : null} me={session.user.email} />
    </main>
  );
}
