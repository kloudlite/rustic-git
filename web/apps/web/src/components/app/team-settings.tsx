import { AppShell } from "@/components/app/app-shell";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { FieldLabel } from "@/components/auth/auth-card";
import { MEMBERS } from "@/lib/mock";
import type { Session } from "@/lib/session";
import { inviteMember, updateTeam } from "@/app/[owner]/settings/actions";

/** A settings section: a heading and its explanation on the left, the controls on
 *  the right. Every section on the page shares the shape, so scanning the left
 *  column is a table of contents. */
function Section({
  title,
  description,
  children,
  danger,
}: {
  title: string;
  description: string;
  children: React.ReactNode;
  danger?: boolean;
}) {
  return (
    <section className="grid gap-6 border-t border-border py-10 first:border-t-0 first:pt-0 md:grid-cols-settings md:gap-12">
      <div>
        <h2 className={`text-body font-semibold ${danger ? "text-destructive" : ""}`}>{title}</h2>
        <p className="mt-1.5 text-sm2 leading-relaxed text-muted-foreground">{description}</p>
      </div>
      <div className="min-w-0">{children}</div>
    </section>
  );
}

function initials(name: string) {
  return name.split(" ").map((p) => p[0]).slice(0, 2).join("");
}

export function TeamSettings({ session }: { session: NonNullable<Session> }) {
  const owner = session.user.owner;

  return (
    <AppShell session={session} active="Settings">
      <main className="mx-auto max-w-page px-6 pt-8 pb-16">
        <h1 className="text-title font-semibold tracking-title">Team settings</h1>
        <p className="mt-1 text-sm2 text-muted-foreground">
          How <span className="font-medium text-foreground">{owner}</span> appears, who is in it, and
          what it can do.
        </p>

        <div className="mt-8">
          <Section
            title="Team"
            description="The name is what people see; the handle is what appears in every URL and clone address, so it is fixed once set."
          >
            <form action={updateTeam} className="grid max-w-md gap-5">
              <div className="grid gap-2">
                <FieldLabel htmlFor="name">Team name</FieldLabel>
                <Input id="name" name="name" defaultValue={owner} className="h-9" />
              </div>
              <div className="grid gap-2">
                <FieldLabel htmlFor="handle">Handle</FieldLabel>
                <div className="flex h-9 items-center border border-input bg-muted/40 px-2.5 font-mono text-sm2 text-muted-foreground">
                  kloudlite.io/<span className="text-foreground">{owner}</span>
                </div>
              </div>
              <div className="grid gap-2">
                <FieldLabel htmlFor="description">Description</FieldLabel>
                <Input id="description" name="description" placeholder="What this team works on" className="h-9" />
              </div>
              <div>
                <Button type="submit">Save changes</Button>
              </div>
            </form>
          </Section>

          <Section
            title="Members"
            description="Owners manage the team, admins manage repos and environments, members work in them. Invitations go to a work email."
          >
            <form action={inviteMember} className="flex max-w-md flex-wrap items-end gap-2">
              <div className="grid min-w-56 flex-1 gap-2">
                <FieldLabel htmlFor="invite">Invite by email</FieldLabel>
                <Input id="invite" name="email" type="email" placeholder="name@company.com" className="h-9" required />
              </div>
              <select
                name="role"
                defaultValue="member"
                aria-label="Role"
                className="h-9 border border-input bg-background px-2 text-sm2 outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50"
              >
                <option value="member">Member</option>
                <option value="admin">Admin</option>
              </select>
              <Button type="submit" variant="outline" className="h-9 border-edge hover:border-edge-hover">
                Send invite
              </Button>
            </form>

            <ul className="mt-6 divide-y divide-border border border-border">
              {MEMBERS.map((m) => (
                <li key={m.login} className="flex items-center gap-4 px-4 py-3">
                  <span className="flex size-8 shrink-0 items-center justify-center bg-muted text-micro font-semibold text-muted-foreground">
                    {initials(m.name)}
                  </span>
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-sm2 font-medium">
                      {m.name}
                      {m.login === session.user.owner && (
                        <span className="ml-2 text-caption font-normal text-muted-foreground">you</span>
                      )}
                    </div>
                    <div className="truncate text-caption text-muted-foreground">{m.email}</div>
                  </div>
                  <span className="hidden text-caption text-muted-foreground sm:inline">Joined {m.joined}</span>
                  <span className="w-16 border border-border px-1.5 py-px text-center text-micro font-medium capitalize text-muted-foreground">
                    {m.role}
                  </span>
                </li>
              ))}
            </ul>
          </Section>

          <Section
            title="Danger zone"
            description="These cannot be undone. Deleting the team removes every repo, registry, workspace and environment in it."
            danger
          >
            <div className="grid max-w-md gap-3">
              <div className="flex items-center justify-between gap-4 border border-border px-4 py-3">
                <div>
                  <div className="text-sm2 font-medium">Transfer ownership</div>
                  <div className="text-caption text-muted-foreground">Hand the team to another owner.</div>
                </div>
                <Button variant="outline" className="border-edge hover:border-edge-hover">Transfer</Button>
              </div>
              <div className="flex items-center justify-between gap-4 border border-destructive/30 px-4 py-3">
                <div>
                  <div className="text-sm2 font-medium">Delete team</div>
                  <div className="text-caption text-muted-foreground">Removes {owner} and everything in it.</div>
                </div>
                <Button variant="destructive">Delete</Button>
              </div>
            </div>
          </Section>
        </div>
      </main>
    </AppShell>
  );
}
