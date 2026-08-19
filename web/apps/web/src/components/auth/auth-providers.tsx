import { OAuthButtons, OrDivider } from "@/components/auth/oauth-buttons";
import { PasskeyButton } from "@/components/auth/passkey-button";
import { enabledProviders } from "@/auth";

/** Buttons and their divider travel together: with nothing above it, a lone "or"
 *  rule would sit above the email field dividing nothing.
 *
 *  Passkeys appear on SIGN IN only. A passkey proves you are an account that
 *  already exists — it carries no email, so it cannot create one — and a
 *  "continue with a passkey" button on the signup page would work for exactly the
 *  people who do not need it and fail for everyone it is aimed at. Adding one is
 *  in Settings, once the account exists. */
export function AuthProviders({ verb }: { verb: "Sign in" | "Sign up" }) {
  return (
    <>
      <div className="grid gap-2">
        {verb === "Sign in" && <PasskeyButton />}
        {Object.values(enabledProviders).some(Boolean) && <OAuthButtons verb={verb} />}
      </div>
      <div className="my-6">
        <OrDivider />
      </div>
    </>
  );
}
