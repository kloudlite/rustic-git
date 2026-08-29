import type { Metadata } from "next";
import { ownersFor } from "@/lib/owners";
import { listCliTokens, listKeys, listPasskeys, listTokens, platformKey, type ApiCredential, type ApiResult } from "@/lib/api";
import { UserSettings } from "@/components/app/user-settings";
import { listOrSignIn } from "@/lib/require-api";
import { requireToken } from "@/lib/session";

export const metadata: Metadata = { title: "Profile settings" };

export default async function Page() {
  const { session, token } = await requireToken("/settings");

  // Nothing below needs another's answer, so it is one round trip deep, not seven: this page
  // was `owners → passkeys → cli → platform → (keys → signing → tokens) × owners` in sequence.
  const [owners, passkeys, cliTokens, platform] = await Promise.all([
    ownersFor(session),
    // Passkeys are the person's, not a namespace's: one call, no owner.
    listPasskeys(token),
    // CLI logins are the person's too — the api defaults them to the caller, so no owner here either.
    listCliTokens(token),
    // The platform key is the person's own, never a team's — a team has no workspaces to carry it.
    // Reading it is what generates it, so opening this page is how an account first gets one.
    platformKey(token, session.user.owner),
  ]);
  // Credentials are per namespace, so the page asks for every namespace this
  // person can act in and shows them as one list — which namespace each belongs
  // to is a column, not a separate page to navigate between.
  const per = await Promise.all(
    owners.map(async (o) => {
      const [keys, signing, tokens] = await Promise.all([
        listKeys(token, o.slug),
        listKeys(token, o.slug, "signing"),
        listTokens(token, o.slug),
      ]);
      return { keys, signing, tokens };
    }),
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
      cliTokens={listOrSignIn(cliTokens)}
      platformKey={platform.ok ? platform.value : undefined}
    />
  );
}
