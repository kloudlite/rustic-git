import { KeyRound, Plus, Trash2 } from "lucide-react";
import { AppShell } from "@/components/app/app-shell";
import { SettingsSection as Section } from "@/components/app/settings-section";
import { ThemePicker } from "@/components/app/theme-picker";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { FieldLabel } from "@/components/auth/auth-card";
import { SSH_KEYS, TOKENS } from "@/lib/mock";
import type { Session } from "@/lib/session";
import { addSshKey, createToken, removeSshKey, revokeToken, updateProfile } from "@/app/settings/actions";
import { Badge } from "@/components/ui/badge";
import { Textarea } from "@/components/ui/textarea";
import { Checkbox } from "@/components/ui/checkbox";

/** Token scopes as a matrix: what a token may touch, and whether it may only look
 *  or also change. Read is implied by write on the server; the form lets both be
 *  ticked so the intent is explicit. */
const SCOPES = [
  { id: "repo", label: "Code repos", read: "Clone and browse", write: "Push, branches, tags" },
  { id: "packages", label: "Package registries", read: "Pull", write: "Publish" },
  { id: "workspaces", label: "Workspaces", read: "View", write: "Open and manage" },
  { id: "environments", label: "Environments", read: "View", write: "Fork, switch, snapshot" },
];

/** A user's own settings — the person, not the team. Team settings are under the
 *  team; this is reached from the avatar menu and is the same page in every team. */
export function UserSettings({ session }: { session: NonNullable<Session> }) {
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

          <Section title="SSH keys" description="Keys you push and pull with over SSH. Add a public key from each machine; the private half never leaves it.">
            <form action={addSshKey} className="grid max-w-xl gap-4">
              <div className="grid gap-2">
                <FieldLabel htmlFor="key-title">Title</FieldLabel>
                <Input id="key-title" name="title" placeholder="Work laptop" className="h-9" required />
              </div>
              <div className="grid gap-2">
                <FieldLabel htmlFor="key">Public key</FieldLabel>
                <Textarea
                  id="key" name="key" rows={3} required spellCheck={false}
                  placeholder="ssh-ed25519 AAAA… you@machine" className="resize-y font-mono text-caption"
                />
                <p className="text-caption text-muted-foreground">Starts with <code className="font-mono">ssh-ed25519</code> or <code className="font-mono">ssh-rsa</code>. Usually in <code className="font-mono">~/.ssh/id_ed25519.pub</code>.</p>
              </div>
              <div><Button type="submit" variant="outline" className="border-edge hover:border-edge-hover"><Plus />Add key</Button></div>
            </form>

            <ul className="mt-6 divide-y divide-border border border-border">
              {SSH_KEYS.map((k) => (
                <li key={k.id} className="flex items-center gap-4 px-4 py-3">
                  <KeyRound className="size-4 shrink-0 text-muted-foreground" />
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2 text-sm2 font-medium">
                      {k.title}
                      <Badge variant="outline" className="uppercase">{k.type}</Badge>
                    </div>
                    <div className="mt-0.5 truncate font-mono text-caption text-muted-foreground">{k.fingerprint}</div>
                    <div className="mt-0.5 text-caption text-muted-foreground">Added {k.added} · Last used {k.lastUsed}</div>
                  </div>
                  <form action={removeSshKey}>
                    <input type="hidden" name="id" value={k.id} />
                    <Button type="submit" variant="ghost" size="sm" className="text-muted-foreground hover:text-destructive" aria-label={`Remove ${k.title}`}>
                      <Trash2 />
                    </Button>
                  </form>
                </li>
              ))}
            </ul>
          </Section>

          <Section title="Personal access tokens" description="For tools and scripts that act as you over HTTPS. Give each one only the scopes it needs and an expiry; the token is shown once, on creation.">
            <form action={createToken} className="grid max-w-xl gap-5">
              <div className="grid gap-4 sm:grid-cols-field-pair">
                <div className="grid gap-2">
                  <FieldLabel htmlFor="tok-name">Name</FieldLabel>
                  <Input id="tok-name" name="name" placeholder="ci-runner" className="h-9" required />
                </div>
                <div className="grid gap-2">
                  <FieldLabel htmlFor="tok-exp">Expires</FieldLabel>
                  <Select name="expires" defaultValue="90">
                    <SelectTrigger id="tok-exp" className="h-9 w-full">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="30">30 days</SelectItem>
                      <SelectItem value="90">90 days</SelectItem>
                      <SelectItem value="365">1 year</SelectItem>
                      <SelectItem value="never">Never</SelectItem>
                    </SelectContent>
                  </Select>
                </div>
              </div>
              <fieldset>
                <legend className="text-sm2 font-medium leading-none">Scopes</legend>
                <div className="mt-2 border border-border">
                  <div className="grid grid-cols-scopes items-center border-b border-border bg-muted/40 px-3 py-1.5 text-micro font-semibold uppercase tracking-label text-muted-foreground">
                    <span>Resource</span>
                    <span className="text-center">Read</span>
                    <span className="text-center">Write</span>
                  </div>
                  <ul className="divide-y divide-border">
                    {SCOPES.map((s) => (
                      <li key={s.id} className="grid grid-cols-scopes items-center px-3 py-2">
                        <span className="text-sm2 font-medium">{s.label}</span>
                        <label className="flex cursor-pointer flex-col items-center gap-1 py-0.5" title={s.read}>
                          <Checkbox name="scope" value={`${s.id}:read`} aria-label={`${s.label}: read`} />
                          <span className="text-micro text-muted-foreground">{s.read}</span>
                        </label>
                        <label className="flex cursor-pointer flex-col items-center gap-1 py-0.5" title={s.write}>
                          <Checkbox name="scope" value={`${s.id}:write`} aria-label={`${s.label}: write`} />
                          <span className="text-micro text-muted-foreground">{s.write}</span>
                        </label>
                      </li>
                    ))}
                  </ul>
                </div>
              </fieldset>
              <div><Button type="submit"><Plus />Generate token</Button></div>
            </form>

            <ul className="mt-6 divide-y divide-border border border-border">
              {TOKENS.map((t) => (
                <li key={t.id} className="flex items-center gap-4 px-4 py-3">
                  <div className="min-w-0 flex-1">
                    <div className="text-sm2 font-medium">{t.name}</div>
                    <div className="mt-1 flex flex-wrap gap-1">
                      {t.scopes.map((s) => <Badge key={s} variant="outline" className="font-mono">{s}</Badge>)}
                    </div>
                    <div className="mt-1 text-caption text-muted-foreground">Created {t.created} · Last used {t.lastUsed} · Expires {t.expires}</div>
                  </div>
                  <form action={revokeToken}>
                    <input type="hidden" name="id" value={t.id} />
                    <Button type="submit" variant="outline" size="sm" className="border-edge text-muted-foreground hover:border-destructive/40 hover:text-destructive">Revoke</Button>
                  </form>
                </li>
              ))}
            </ul>
          </Section>
        </div>
      </main>
    </AppShell>
  );
}
