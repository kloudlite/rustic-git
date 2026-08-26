# Team Settings on Real Data — Implementation Plan

**Goal:** restore the team settings page deleted in `d8e3925` ("Serve team pages for any owner and
replace mock team data with empty states") with the same design, backed by the directory instead of
`lib/mock.ts`.

The page was never broken. It rendered a convincing UI over fake data with no-op actions, and
deleting it was the honest move at the time. What changed is that the backing exists now: teams,
members and roles are real documents in Mongo. This plan connects the two.

## What already exists

`crates/pulls/src/directory/teams.rs`:

```rust
Team   { slug, name, created_by, created_at, members: Vec<Member> }
Member { user, role, joined_at }
Role   { Owner, Admin, Member }
```

`Directory::create` / `get` / `for_user`, plus `Directory::user(email)` for display names, and
`POST|GET /v1/teams`. The Team form and Members list are therefore already backed — they lack a
read route and any mutation.

`Member.user` is the person's EMAIL, not their handle. The old component keyed off `m.login` and
compared against `session.user.owner`; both are wrong against real documents and are corrected here.

## What is missing

| UI element | Backing today | Work |
|---|---|---|
| Team name | `Team.name` | read route + rename |
| Handle | `Team.slug` | read-only, as designed — it is in every URL and clone address |
| Description | — | new field on `Team` |
| Members list | `Team.members` | read route + resolve emails to names |
| Role badge | `Member.role` | free once the list reads |
| Invite by email | — | becomes "Add member" (see Decisions) |
| Transfer ownership | — | role flip, owner only |
| Delete team | — | gated on the team being empty (see Decisions) |

## Decisions

**Invitations become direct adds.** There is no pending-invite state and no email transport anywhere
in this tree. Building one is a project with a mailer in it. Adding an existing user by email is
small, honest and works today, so the control says "Add member" rather than "Send invite". A pending
flow can replace it later without changing the storage shape.

**Delete refuses while the team owns anything.** The mock promised "removes every repo, registry,
workspace and environment in it" — a cascading delete across SlateDB repo databases, registry blobs
and cluster-scoped CRDs, each with its own ownership rules and none of them transactional. That is
its own project, and a settings-page button is the wrong place to start it. Delete therefore
refuses while the team owns repos, images, workspaces or environments, and says what is left.
This is a deliberate divergence from the mock: the mock promised something the system cannot do
safely.

**Authorization reads `Team.members`, never the URL or a label.** Same rule as `may_act_on` in the
workspaces API. The slug in the path says which team; it never says whether the caller may touch it.

**The last owner is protected.** A team with no owner can never be administered again, so the last
owner cannot be removed, demoted, or leave. Enforced in the directory, not in the handler, so every
future caller inherits it.

## Tasks

### 1. Read path

- `Directory::describe(slug) -> Option<(Team, Vec<Person>)>` resolving member emails to display
  names in one query, not N.
- `GET /v1/teams/{slug}`, authorized on membership; 404 (not 403) for a non-member, so the route
  cannot be used to probe which teams exist.
- Web: `getTeam` in `lib/api.ts`, page restored from `d8e3925^` rendering real name, slug, members,
  roles and join dates.

**Verify:** page shows the real member list; a non-member gets 404.

### 2. Rename and description

- `description` on `Team`, defaulted for documents written before it existed.
- `Directory::update_team(slug, name, description)`.
- `PATCH /v1/teams/{slug}`, owner or admin.
- Web: server action on the existing form.

**Verify:** rename persists and shows in the switcher; a plain member is refused.

### 3. Members

- `Directory::add_member(slug, email, role)` — refuses an unknown user and a duplicate.
- `Directory::remove_member(slug, email)` and `set_role(slug, email, role)`, both refusing to
  strand the team without an owner.
- `POST /v1/teams/{slug}/members`, `DELETE .../members/{email}`, `PATCH .../members/{email}`.
- Web: add form, remove button, role select.

**Verify:** add, promote, demote, remove all round-trip; removing the last owner is refused with a
message that says why.

### 4. Danger zone

- `Directory::transfer(slug, to)` — owner only, promotes the target and demotes the caller in one
  update so there is never a moment with two owners or none.
- `Directory::delete_team(slug)` refusing while anything is owned.
- `POST /v1/teams/{slug}/transfer`, `DELETE /v1/teams/{slug}`.
- Web: both behind a typed confirmation, matching the repo settings pattern.

**Verify:** delete refuses on a team with a repo and names it; transfer moves ownership exactly once.

## Out of scope

Pending invitations, email delivery, cascading team deletion, per-repo permissions. Each is named
in the UI where a user would otherwise expect it.
