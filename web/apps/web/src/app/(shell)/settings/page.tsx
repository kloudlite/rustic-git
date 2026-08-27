import type { Metadata } from "next";
import { redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { ownersFor } from "@/lib/owners";
import { apiToken } from "@/lib/api-token";
import { listKeys, listPasskeys, listTokens, platformKey, type ApiCredential, type ApiResult } from "@/lib/api";
import { UserSettings } from "@/components/app/user-settings";
import { listOrSignIn } from "@/lib/require-api";

export const metadata: Metadata = { title: "Profile settings" };

export default async function Page() {
  const session = await getSession();
  if (!session) redirect("/login");
  if (!session.user.username) redirect("/welcome");

  const token = await apiToken();
  if (!token) redirect("/login");

  const owners = await ownersFor(session);
  // Passkeys are the person's, not a namespace's: one call, no owner.
  const passkeys = await listPasskeys(token);
  // The platform key is the person's own, never a team's — a team has no workspaces to carry it.
  // Reading it is what generates it, so opening this page is how an account first gets one.
  const platform = await platformKey(token, session.user.owner);
  // Credentials are per namespace, so the page asks for every namespace this
  // person can act in and shows them as one list — which namespace each belongs
  // to is a column, not a separate page to navigate between.
  const per = await Promise.all(
    owners.map(async (o) => ({
      keys: await listKeys(token, o.slug),
      signing: await listKeys(token, o.slug, "signing"),
      tokens: await listTokens(token, o.slug),
    })),
  );
  // `listOrSignIn`, not `?? []`: an expired token must send the person to sign in
  // rather than render their credentials as gone.
  const gather = (pick: (p: (typeof per)[number]) => ApiResult<ApiCredential[]>) =>
    per.flatMap((p) => listOrSignIn(pick(p)));

  return (
    <UserSettings
      session={session}
      owners={owners}
      keys={gather((p) => p.keys)}
      signingKeys={gather((p) => p.signing)}
      tokens={gather((p) => p.tokens)}
      passkeys={listOrSignIn(passkeys)}
      platformKey={platform.ok ? platform.value : undefined}
    />
  );
}
