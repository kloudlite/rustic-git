import { Button } from "@/components/ui/button";
import { DEV_BYPASS, devUser } from "@/lib/dev-auth";
import { devSignIn } from "@/app/(auth)/actions";

/** Renders nothing unless the bypass is active, and the bypass cannot be active
 *  in a production build — so this cannot appear in one. */
export function DevBypass() {
  if (!DEV_BYPASS) return null;
  const user = devUser();

  return (
    <div className="mt-6 border border-dashed border-edge p-4">
      <p className="text-micro font-semibold uppercase tracking-eyebrow text-muted-foreground">
        Development only
      </p>
      <p className="mt-2 text-sm2 text-muted-foreground">
        Sign in as <span className="font-medium text-foreground">{user.email}</span> without a
        provider.
      </p>
      <form action={devSignIn}>
        <Button type="submit" variant="outline" size="lg" className="mt-3 w-full">
          Continue as {user.name}
        </Button>
      </form>
    </div>
  );
}
