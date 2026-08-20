import { KeyRound, ShieldCheck, Trash2 } from "lucide-react";
import { AppShell } from "@/components/app/app-shell";
import { SettingsSection as Section } from "@/components/app/settings-section";
import { ThemePicker } from "@/components/app/theme-picker";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { FieldLabel } from "@/components/auth/auth-card";
import type { Session } from "@/lib/session";
import type { ApiCredential, ApiPasskey } from "@/lib/api";
import type { SwitcherOwner } from "@/components/app/team-switcher";
import { removeSshKey, revokeToken, updateProfile } from "@/app/settings/actions";
import { AddKeyDialog } from "@/components/app/add-key-dialog";
import { NewTokenDialog } from "@/components/app/new-token-dialog";
import { Badge } from "@/components/ui/badge";
import { PasskeysSection } from "@/components/app/passkeys-section";

/** A user's own settings — the person, not the team. Team settings are under the
 *  team; this is reached from the avatar menu and is the same page in every team. */
export function UserSettings({
  session,
  owners,
  keys,
  signingKeys,
  tokens,
  passkeys,
}: {
  session: NonNullable<Session>;
  owners: SwitcherOwner[];
  keys: ApiCredential[];
  signingKeys: ApiCredential[];
  tokens: ApiCredential[];
  passkeys: ApiPasskey[];
}) {
  const many = owners.length > 1;
  return (
    <AppShell session={session}>
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <h1 className="text-title font-semibold tracking-title">Your settings</h1>
        <p className="mt-1 text-sm2 text-muted-foreground">
          Signed in as <span className="font-medium text-foreground">{session.user.email}</span>.
          These follow you across every team.
        </p>

        <div className="mt-8">
          <Section title="Profile" description="How you appear to your teams. Your email comes from the identity you signed in with and is not editable here.">
            <form action={updateProfile} className="grid max-w-md gap-5">
              <div className="grid gap-2">
                <FieldLabel htmlFor="name">Name</FieldLabel>
                <Input id="name" name="name" defaultValue={session.user.name} className="h-9" />
              </div>
              <div className="grid gap-2">
                <FieldLabel htmlFor="email">Email</FieldLabel>
                <div className="flex h-9 items-center border border-input bg-muted/40 px-2.5 text-sm2 text-muted-foreground">{session.user.email}</div>
              </div>
              <div className="grid gap-2">
                <FieldLabel htmlFor="handle">Handle</FieldLabel>
                <div className="flex h-9 items-center border border-input bg-muted/40 px-2.5 font-mono text-sm2 text-muted-foreground">
                  @<span className="text-foreground">{session.user.owner}</span>
                </div>
              </div>
              <div><Button type="submit">Save changes</Button></div>
            </form>
          </Section>

          <Section title="Appearance" description="Light, dark, or whatever the operating system is doing. Applies to this browser.">
            <ThemePicker />
          </Section>

          <Section
            title="Passkeys"
            description="Sign in with a fingerprint, face or device PIN instead of a password. The key stays on the device; this site only ever sees a signature."
          >
            <PasskeysSection passkeys={passkeys} />
          </Section>

          <Section
            title="SSH keys"
            description={
              many
                ? "Keys you push and pull with over SSH. A key works in the one namespace it was added for, so a key for a team is a separate entry."
                : "Keys you push and pull with over SSH. Add a public key from each machine; the private half never leaves it."
            }
          >
            <div className="flex items-center justify-between">
              <p className="text-sm2 text-muted-foreground">
                {keys.length} {keys.length === 1 ? "key" : "keys"}
              </p>
              <AddKeyDialog owners={owners} defaultOwner={session.user.owner} />
            </div>
            {keys.length === 0 ? (
              <p className="mt-3 border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
                No keys yet. Add one and you can clone over SSH.
              </p>
            ) : (
              <ul className="mt-3 divide-y divide-border border border-border bg-card">
                {keys.map((k) => (
                  <li key={k._id} className="flex items-center gap-4 px-4 py-3">
                    <KeyRound className="size-4 shrink-0 text-muted-foreground" />
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-2 text-sm2 font-medium">
                        {k.name}
                        {many && <Badge variant="outline" className="font-mono">{k.owner}</Badge>}
                      </div>
                      {/* The fingerprint IS the id — it is what the fleet stores the key under. */}
                      <div className="mt-0.5 truncate font-mono text-caption text-muted-foreground">{k._id}</div>
                    </div>
                    <form action={removeSshKey}>
                      <input type="hidden" name="id" value={k._id} />
                      <Button type="submit" variant="ghost" size="sm" className="text-muted-foreground hover:text-destructive" aria-label={`Remove ${k.name}`}>
                        <Trash2 />
                      </Button>
                    </form>
                  </li>
                ))}
              </ul>
            )}
          </Section>

          <Section
            title="Signing keys"
            description="Keys that prove you wrote a commit. A commit signed with one of these shows as verified; the key grants no access on its own."
          >
            <div className="flex items-center justify-between">
              <p className="text-sm2 text-muted-foreground">
                {signingKeys.length} {signingKeys.length === 1 ? "key" : "keys"}
              </p>
              <AddKeyDialog owners={owners} defaultOwner={session.user.owner} signing />
            </div>
            {signingKeys.length === 0 ? (
              <p className="mt-3 border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
                No signing keys. Sign commits with{" "}
                <code className="font-mono text-caption">git config gpg.format ssh</code> and add the
                key here to have them verified.
              </p>
            ) : (
              <ul className="mt-3 divide-y divide-border border border-border bg-card">
                {signingKeys.map((k) => (
                  <li key={k._id} className="flex items-center gap-4 px-4 py-3">
                    <ShieldCheck className="size-4 shrink-0 text-muted-foreground" />
                    <div className="min-w-0 flex-1">
                      <div className="text-sm2 font-medium">{k.name}</div>
                      <div className="mt-0.5 truncate font-mono text-caption text-muted-foreground">
                        {k._id.replace(/^sign:/, "")}
                      </div>
                    </div>
                    <form action={removeSshKey}>
                      <input type="hidden" name="id" value={k._id} />
                      <Button type="submit" variant="ghost" size="sm" className="text-muted-foreground hover:text-destructive" aria-label={`Remove ${k.name}`}>
                        <Trash2 />
                      </Button>
                    </form>
                  </li>
                ))}
              </ul>
            )}
          </Section>

          <Section
            title="Personal access tokens"
            description="For tools and scripts that clone and push over HTTPS. A token acts in one namespace, and is shown once, on creation."
          >
            <div className="flex items-center justify-between">
              <p className="text-sm2 text-muted-foreground">
                {tokens.length} {tokens.length === 1 ? "token" : "tokens"}
              </p>
              <NewTokenDialog owners={owners} defaultOwner={session.user.owner} />
            </div>
            {tokens.length === 0 ? (
              <p className="mt-3 border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
                No tokens yet. Generate one to clone over HTTPS.
              </p>
            ) : (
              <ul className="mt-3 divide-y divide-border border border-border bg-card">
                {tokens.map((t) => (
                  <li key={t._id} className="flex items-center gap-4 px-4 py-3">
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-2 text-sm2 font-medium">
                        {t.name}
                        {many && <Badge variant="outline" className="font-mono">{t.owner}</Badge>}
                      </div>
                    </div>
                    <form action={revokeToken}>
                      <input type="hidden" name="id" value={t._id} />
                      <Button type="submit" variant="outline" size="sm" className="border-edge text-muted-foreground hover:border-destructive/40 hover:text-destructive">
                        Revoke
                      </Button>
                    </form>
                  </li>
                ))}
              </ul>
            )}
          </Section>
        </div>
      </main>
    </AppShell>
  );
}
