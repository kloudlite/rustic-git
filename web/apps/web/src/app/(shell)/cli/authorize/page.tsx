import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import { Approve } from "./approve";

export const metadata: Metadata = { title: "Authorize the CLI" };

/** Where `kl login` sends the browser.
 *
 *  The device name is not in the URL — only the code is, and the api will not hand it back
 *  before approval — so the page names the code and nothing else. That is the whole check the
 *  person can make: the code on screen matches the code in their terminal. */
export default async function CliAuthorizePage({
  searchParams,
}: {
  searchParams: Promise<{ code?: string }>;
}) {
  const { code } = await searchParams;
  const session = await getSession();
  // Signed in as the person the token will belong to is the entire point, so a signed-out
  // caller comes back HERE after signing in rather than to the home page.
  if (!session) redirect(`/login?next=${encodeURIComponent(`/cli/authorize?code=${code ?? ""}`)}`);
  if (!session.user.username) redirect("/welcome");
  const token = await apiToken();
  if (!token) redirect("/login?from=expired");

  // The person types it; case and the dash are theirs to get wrong, and the api uppercases too.
  const clean = (code ?? "").trim().toUpperCase();

  return (
    <main className="mx-auto max-w-2xl px-6 pt-12 pb-16">
      <h1 className="text-title font-semibold tracking-title">
        {clean ? `Approve CLI login for code ${clean}?` : "Approve CLI login"}
      </h1>
      {clean ? (
        <Approve code={clean} />
      ) : (
        <p className="mt-6 border border-border bg-card px-5 py-8 text-center text-sm2 text-muted-foreground">
          This link is missing its code. Run <code className="font-mono text-caption">kl login</code>{" "}
          again and open the link it prints.
        </p>
      )}
    </main>
  );
}
