"use client";

import { useActionState, useState } from "react";
import { Loader2, MailX, Trash2, TriangleAlert } from "lucide-react";
import { SettingsSection as Section } from "@/components/app/settings-section";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { FieldLabel } from "@/components/auth/auth-card";
import { Badge } from "@/components/ui/badge";
import { Initials } from "@/components/app/initials";
import { DeleteForm } from "@/components/app/delete-form";
import type { ApiInvite, ApiRole, ApiTeamDetail, ApiTeamMember } from "@/lib/api";
import { when } from "@/lib/time";
import {
  destroyTeam, invite, removeMember, revokeInvite, saveTeam, setRole, type InviteState, type TeamState,
} from "@/app/(shell)/[owner]/(org)/settings/actions";

function Saved({ state }: { state: TeamState }) {
  if (state?.error) return <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>;
  if (state?.ok) return <p className="text-sm2 text-success">Saved.</p>;
  return null;
}

/** The team's settings, on the directory. The controls a person sees follow their role —
 *  and the api decides again on every write, so a hidden control is a courtesy, not a gate.
 *
 *  The model: a member does everything in the product and may edit the name here; an admin
 *  also invites and makes admins; an owner also makes owners and deletes the team. */
export function TeamSettings({ team, me }: { team: ApiTeamDetail; me: string }) {
  const isOwner = team.yourRole === "owner";
  const canAdmin = isOwner || team.yourRole === "admin";
  return (
    <>
      <h1 className="text-title font-semibold tracking-title">Team settings</h1>
      <p className="mt-1 text-sm2 text-muted-foreground">
        How <span className="font-medium text-foreground">{team.name}</span> appears, who is in it, and
        what it can do.
      </p>

      <div className="mt-8">
        <Section
          title="Team"
          description="The name is what people see; the handle is what appears in every URL and clone address, so it is fixed once set."
        >
          <Profile team={team} disabled={false} />
        </Section>

        <Section
          title="Members"
          description="Members work in everything the team owns. Admins can also invite people and make admins. Owners can also make owners and delete the team. Invitations go by email and last seven days."
        >
          {canAdmin && <Invite slug={team.slug} isOwner={isOwner} />}
          <ul className={`divide-y divide-border border border-border bg-card ${canAdmin ? "mt-6" : ""}`}>
            {team.members.map((m) => (
              <MemberRow key={m.email} team={team} m={m} me={me} />
            ))}
          </ul>
          {canAdmin && team.invites.length > 0 && <Pending slug={team.slug} invites={team.invites} />}
        </Section>

        {isOwner && (
          <Section
            title="Danger zone"
            description="This cannot be undone. A team can only be deleted once it owns nothing."
            danger
          >
            <div className="grid max-w-xl gap-3">
              <Danger slug={team.slug} />
            </div>
          </Section>
        )}
      </div>
    </>
  );
}

function Profile({ team, disabled }: { team: ApiTeamDetail; disabled: boolean }) {
  const [state, action, pending] = useActionState<TeamState, FormData>(saveTeam, null);
  return (
    <form action={action} className="grid max-w-md gap-5">
      <input type="hidden" name="slug" value={team.slug} />
      <div className="grid gap-2">
        <FieldLabel htmlFor="name">Team name</FieldLabel>
        <Input id="name" name="name" defaultValue={team.name} disabled={disabled} className="h-9" />
      </div>
      <div className="grid gap-2">
        <FieldLabel htmlFor="handle">Handle</FieldLabel>
        <div className="flex h-9 items-center border border-input bg-muted/40 px-2.5 font-mono text-sm2 text-muted-foreground">
          kloudlite.io/<span className="text-foreground">{team.slug}</span>
        </div>
      </div>
      <div className="grid gap-2">
        <FieldLabel htmlFor="description">Description</FieldLabel>
        <Input id="description" name="description" defaultValue={team.description} disabled={disabled} placeholder="What this team works on" className="h-9" />
      </div>
      <Saved state={state} />
      {!disabled && (
        <div>
          <Button type="submit" disabled={pending}>{pending && <Loader2 className="animate-spin" />}Save changes</Button>
        </div>
      )}
    </form>
  );
}

function Invite({ slug, isOwner }: { slug: string; isOwner: boolean }) {
  const [state, action, pending] = useActionState<InviteState, FormData>(invite, null);
  return (
    <form action={action} className="grid max-w-xl gap-3">
      <input type="hidden" name="slug" value={slug} />
      <div className="flex flex-wrap items-end gap-2">
        <div className="grid min-w-56 flex-1 gap-2">
          <FieldLabel htmlFor="email">Invite by email</FieldLabel>
          <Input id="email" name="email" type="email" placeholder="name@company.com" className="h-9" required />
        </div>
        <Select name="role" defaultValue="member">
          <SelectTrigger aria-label="Role" className="h-9 w-32">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="member">Member</SelectItem>
            <SelectItem value="admin">Admin</SelectItem>
            {/* Only an owner may make an owner; the api refuses it otherwise. */}
            {isOwner && <SelectItem value="owner">Owner</SelectItem>}
          </SelectContent>
        </Select>
        <Button type="submit" variant="outline" disabled={pending} className="h-9 border-edge hover:border-edge-hover">
          {pending && <Loader2 className="animate-spin" />}Send invite
        </Button>
      </div>
      {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
      {state?.ok && state.notice && (
        <p className={`text-sm2 ${state.link ? "text-muted-foreground" : "text-success"}`}>{state.notice}</p>
      )}
      {/* Shown only when the email could not go out — the token is a credential, and the
          less it is on screen the better. */}
      {state?.link && (
        <code className="block break-all border border-border bg-muted/40 px-2.5 py-2 font-mono text-caption">{state.link}</code>
      )}
    </form>
  );
}

function Pending({ slug, invites }: { slug: string; invites: ApiInvite[] }) {
  return (
    <div className="mt-6">
      <p className="text-sm2 text-muted-foreground">
        {invites.length} pending {invites.length === 1 ? "invitation" : "invitations"}
      </p>
      <ul className="mt-2 divide-y divide-border border border-dashed border-border bg-card">
        {invites.map((i) => (
          <li key={i.id} className="flex items-center gap-4 px-4 py-3">
            <div className="min-w-0 flex-1">
              <div className="truncate text-sm2 font-medium">{i.email}</div>
              <div className="truncate text-caption text-muted-foreground">
                Invited by {i.invitedBy} · expires {when(Date.parse(i.expiresAt))}
              </div>
            </div>
            <Badge variant="outline" className="w-16 justify-center capitalize">{i.role}</Badge>
            <DeleteForm action={revokeInvite} fields={{ slug, id: i.id }} confirm={`Withdraw the invitation to ${i.email}?`}>
              <Button type="submit" variant="ghost" size="sm" className="text-muted-foreground hover:text-destructive" aria-label={`Withdraw invitation to ${i.email}`}>
                <MailX />
              </Button>
            </DeleteForm>
          </li>
        ))}
      </ul>
    </div>
  );
}

function MemberRow({ team, m, me }: { team: ApiTeamDetail; m: ApiTeamMember; me: string }) {
  const self = m.email.toLowerCase() === me.toLowerCase();
  // The api's own rule, mirrored: an admin reaches members and admins; an owner reaches
  // anyone. An owner may lower their own role, and the api refuses it only for the last one.
  const reach = (r: ApiRole) => team.yourRole === "owner" || (team.yourRole === "admin" && r !== "owner");
  const canEdit = reach(m.role);
  const canRemove = self || canEdit;
  return (
    <li className="flex items-center gap-4 px-4 py-3">
      <Initials name={m.name} size={8} tone={self ? "primary" : "muted"} className="shrink-0" />
      <div className="min-w-0 flex-1">
        <div className="truncate text-sm2 font-medium">
          {m.name}
          {m.username && <span className="ml-2 font-mono text-caption font-normal text-muted-foreground">@{m.username}</span>}
          {self && <span className="ml-2 text-caption font-normal text-muted-foreground">you</span>}
        </div>
        <div className="truncate text-caption text-muted-foreground">{m.email}</div>
      </div>
      <span className="hidden text-caption text-muted-foreground sm:inline">Joined {when(Date.parse(m.joinedAt))}</span>
      {canEdit ? (
        <RoleSelect slug={team.slug} m={m} isOwner={team.yourRole === "owner"} />
      ) : (
        <Badge variant="outline" className="w-16 justify-center capitalize">{m.role}</Badge>
      )}
      {canRemove && (
        <DeleteForm
          action={removeMember}
          fields={{ slug: team.slug, email: m.email, self: self ? "1" : "0" }}
          confirm={self ? `Leave ${team.name}?` : `Remove ${m.name} from ${team.name}?`}
        >
          <Button type="submit" variant="ghost" size="sm" className="text-muted-foreground hover:text-destructive" aria-label={self ? "Leave team" : `Remove ${m.name}`}>
            <Trash2 />
          </Button>
        </DeleteForm>
      )}
    </li>
  );
}

/** A select that submits on change — there is one field, and a Save button beside every row
 *  is a row of buttons nobody wants. */
function RoleSelect({ slug, m, isOwner }: { slug: string; m: ApiTeamMember; isOwner: boolean }) {
  const [state, action, pending] = useActionState<TeamState, FormData>(setRole, null);
  return (
    <form action={action} className="flex items-center gap-2">
      <input type="hidden" name="slug" value={slug} />
      <input type="hidden" name="email" value={m.email} />
      {state?.error && <span role="alert" className="text-caption font-medium text-destructive">{state.error}</span>}
      <Select name="role" defaultValue={m.role} disabled={pending} onValueChange={() => {
        // `requestSubmit` runs the form's action; `submit()` would bypass it.
        queueMicrotask(() => document.getElementById(`role-${m.email}`)?.closest("form")?.requestSubmit());
      }}>
        <SelectTrigger id={`role-${m.email}`} aria-label={`Role of ${m.name}`} className="h-8 w-28 capitalize">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="member">Member</SelectItem>
          <SelectItem value="admin">Admin</SelectItem>
          {isOwner && <SelectItem value="owner">Owner</SelectItem>}
        </SelectContent>
      </Select>
    </form>
  );
}

function Danger({ slug }: { slug: string }) {
  const [state, action, pending] = useActionState<TeamState, FormData>(destroyTeam, null);
  const [typed, setTyped] = useState("");
  return (
    <div className="border border-destructive/40 bg-card">
      <div className="border-b border-destructive/40 bg-destructive/5 px-4 py-2.5 text-sm2 font-medium">Delete team</div>
      <form action={action} className="grid gap-3 p-4">
        <input type="hidden" name="slug" value={slug} />
        <p className="flex items-start gap-2 text-sm2 leading-relaxed text-muted-foreground">
          <TriangleAlert className="mt-0.5 size-4 shrink-0 text-destructive" />
          Removes {slug} and frees its handle. Refused while the team still owns repositories,
          images, workspaces or environments — delete or move those first.
        </p>
        <div className="grid gap-2">
          <FieldLabel htmlFor="confirm-delete">
            Type <span className="font-mono font-semibold text-foreground">{slug}</span> to confirm
          </FieldLabel>
          <Input id="confirm-delete" name="confirm" value={typed} onChange={(e) => setTyped(e.target.value)} autoComplete="off" placeholder={slug} className="h-9 max-w-sm font-mono" />
        </div>
        <Saved state={state} />
        <div>
          <Button type="submit" variant="destructive" disabled={pending || typed !== slug}>
            {pending && <Loader2 className="animate-spin" />}Delete this team
          </Button>
        </div>
      </form>
    </div>
  );
}
