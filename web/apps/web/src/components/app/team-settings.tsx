"use client";

import { useActionState, useState } from "react";
import { Loader2, Trash2, TriangleAlert } from "lucide-react";
import { SettingsSection as Section } from "@/components/app/settings-section";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { FieldLabel } from "@/components/auth/auth-card";
import { Badge } from "@/components/ui/badge";
import { Initials } from "@/components/app/initials";
import { DeleteForm } from "@/components/app/delete-form";
import type { ApiTeamDetail, ApiTeamMember } from "@/lib/api";
import { when } from "@/lib/time";
import {
  addMember, destroyTeam, removeMember, saveTeam, setRole, transferTeam, type TeamState,
} from "@/app/(shell)/[owner]/(org)/settings/actions";

function Saved({ state }: { state: TeamState }) {
  if (state?.error) return <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>;
  if (state?.ok) return <p className="text-sm2 text-success">Saved.</p>;
  return null;
}

/** The team's settings, on the directory. The controls a person sees follow their role —
 *  and the api decides again on every write, so a hidden control is a courtesy, not a gate. */
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
          <Profile team={team} disabled={!canAdmin} />
        </Section>

        <Section
          title="Members"
          description={
            canAdmin
              ? "Owners manage the team, admins manage repos and environments, members work in them. Someone has to have signed in here before they can be added."
              : "Owners manage the team, admins manage repos and environments, members work in them."
          }
        >
          {canAdmin && <AddMember slug={team.slug} isOwner={isOwner} />}
          <ul className={`divide-y divide-border border border-border bg-card ${canAdmin ? "mt-6" : ""}`}>
            {team.members.map((m) => (
              <MemberRow key={m.email} team={team} m={m} me={me} />
            ))}
          </ul>
        </Section>

        {isOwner && (
          <Section
            title="Danger zone"
            description="These cannot be undone. A team can only be deleted once it owns nothing."
            danger
          >
            <div className="grid max-w-xl gap-3">
              <Transfer team={team} me={me} />
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

function AddMember({ slug, isOwner }: { slug: string; isOwner: boolean }) {
  const [state, action, pending] = useActionState<TeamState, FormData>(addMember, null);
  return (
    <form action={action} className="grid max-w-md gap-3">
      <input type="hidden" name="slug" value={slug} />
      <div className="flex flex-wrap items-end gap-2">
        <div className="grid min-w-56 flex-1 gap-2">
          <FieldLabel htmlFor="email">Add by email</FieldLabel>
          <Input id="email" name="email" type="email" placeholder="name@company.com" className="h-9" required />
        </div>
        <Select name="role" defaultValue="member">
          <SelectTrigger aria-label="Role" className="h-9 w-32">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="member">Member</SelectItem>
            {/* Only an owner may create an admin; the api refuses it otherwise. */}
            {isOwner && <SelectItem value="admin">Admin</SelectItem>}
          </SelectContent>
        </Select>
        <Button type="submit" variant="outline" disabled={pending} className="h-9 border-edge hover:border-edge-hover">
          {pending && <Loader2 className="animate-spin" />}Add member
        </Button>
      </div>
      <Saved state={state} />
    </form>
  );
}

function MemberRow({ team, m, me }: { team: ApiTeamDetail; m: ApiTeamMember; me: string }) {
  const self = m.email.toLowerCase() === me.toLowerCase();
  const isOwner = team.yourRole === "owner";
  const canAdmin = isOwner || team.yourRole === "admin";
  // Who may change this row: an owner can change anyone but themselves (that is a transfer);
  // an admin can move members and admins, never an owner. Mirrors the api's own rule.
  const canEdit = !self && canAdmin && (isOwner || m.role !== "owner");
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
      {canEdit && m.role !== "owner" ? (
        <RoleSelect slug={team.slug} m={m} isOwner={isOwner} />
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
          {/* An admin cannot promote to admin; only an owner can. */}
          {(isOwner || m.role === "admin") && <SelectItem value="admin">Admin</SelectItem>}
        </SelectContent>
      </Select>
    </form>
  );
}

function Transfer({ team, me }: { team: ApiTeamDetail; me: string }) {
  const [state, action, pending] = useActionState<TeamState, FormData>(transferTeam, null);
  const [typed, setTyped] = useState("");
  const others = team.members.filter((m) => m.email.toLowerCase() !== me.toLowerCase());
  return (
    <div className="border border-border bg-card">
      <div className="border-b border-border px-4 py-2.5 text-sm2 font-medium">Transfer ownership</div>
      <form action={action} className="grid gap-3 p-4">
        <input type="hidden" name="slug" value={team.slug} />
        <p className="text-sm2 leading-relaxed text-muted-foreground">
          Hand the team to another member. You stay on as an admin; they become the owner.
        </p>
        {others.length === 0 ? (
          <p className="text-sm2 text-muted-foreground">Add another member first.</p>
        ) : (
          <>
            <div className="grid gap-2">
              <FieldLabel htmlFor="to">New owner</FieldLabel>
              <Select name="to">
                <SelectTrigger id="to" className="h-9 w-full max-w-sm"><SelectValue placeholder="Pick a member" /></SelectTrigger>
                <SelectContent>
                  {others.map((m) => <SelectItem key={m.email} value={m.email}>{m.name} · {m.email}</SelectItem>)}
                </SelectContent>
              </Select>
            </div>
            <div className="grid gap-2">
              <FieldLabel htmlFor="confirm-transfer">
                Type <span className="font-mono font-semibold text-foreground">{team.slug}</span> to confirm
              </FieldLabel>
              <Input id="confirm-transfer" name="confirm" value={typed} onChange={(e) => setTyped(e.target.value)} autoComplete="off" placeholder={team.slug} className="h-9 max-w-sm font-mono" />
            </div>
            <Saved state={state} />
            <div>
              <Button type="submit" variant="outline" disabled={pending || typed !== team.slug} className="border-edge hover:border-edge-hover">
                {pending && <Loader2 className="animate-spin" />}Transfer
              </Button>
            </div>
          </>
        )}
      </form>
    </div>
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
