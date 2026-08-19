import { OAuthButtons, OrDivider } from "@/components/auth/oauth-buttons";
import { enabledProviders } from "@/auth";

/** Buttons and their divider travel together: with no provider configured, a lone
 *  "or" rule would sit above the email field dividing nothing. */
export function AuthProviders({ verb }: { verb: "Sign in" | "Sign up" }) {
  if (!Object.values(enabledProviders).some(Boolean)) return null;
  return (
    <>
      <OAuthButtons verb={verb} />
      <div className="my-6">
        <OrDivider />
      </div>
    </>
  );
}
