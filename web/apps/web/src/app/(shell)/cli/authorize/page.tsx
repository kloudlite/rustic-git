import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import * as api from "@/lib/api";
import { Approve } from "./approve";

export const metadata: Metadata = { title: "Authorize the CLI" };

/** Where `kl login` sends the browser.
 *
 *  The code arrives prefilled in the URL, so "the code matches my terminal" is not a check the
 *  person makes — the link made it for them. The DEVICE is: it is read here, server-side, from
 *  the code itself, and a code with no device behind it (unknown, expired, already approved)
 *  gets an explanation and NO Approve button rather than a one-click approval of something
 *  unnamed. */
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
  const pending = clean ? await api.pendingCliCode(token, clean) : null;
  const device = pending?.ok ? pending.value.device : null;

  return (
    <main className="mx-auto max-w-2xl px-6 pt-12 pb-16">
      <h1 className="text-title font-semibold tracking-title">
        {device ? <>Approve CLI login from <strong>{device}</strong>?</> : "Approve CLI login"}
      </h1>
      {device ? (
        <Approve code={clean} device={device} />
      ) : (
        <p className="mt-6 border border-border bg-card px-5 py-8 text-center text-sm2 text-muted-foreground">
          {clean ? (
            <>
              This login is no longer waiting — it expired, was already approved, or never
              existed. Run <code className="font-mono text-caption">kl login</code> again and open
              the link it prints.
            </>
          ) : (
            <>
              This link is missing its code. Run{" "}
              <code className="font-mono text-caption">kl login</code> again and open the link it
              prints.
            </>
          )}
        </p>
      )}
    </main>
  );
}
