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

**The role model, decided 26 Aug:**

| role | may |
|---|---|
| member | everything in the product, and the team's name and description |
| admin | member, plus invite people and make admins |
| owner | admin, plus make owners and delete the team |

Owners are additive — a team may have several, and an owner promotes another rather than handing
over — so there is no transfer step. The one rule binding an owner is that the last one cannot
step down or be removed; that lives in the directory so every caller inherits it. `may_grant` in
`crates/api/src/teams.rs` is the table above as code, and the test beside it is the table as a
check.

**Invitations are real, by email, through Resend.** An invitation is a row keyed by the SHA-256
of a one-time token; the raw token exists in the email and the accept URL, nowhere else, so the
collection cannot be used to join a team. Accept requires the signed-in email to match the
invited one — otherwise every forwarded link is a bearer credential for the team. The web app
sends the mail (it already knows its own URL for the link, and holds the Resend key in its
Secret); unconfigured, it shows the inviter the link to pass on rather than pretending an email
went out. Seven-day expiry, filtered on read rather than by TTL index (Cosmos expires on `_ts`
only).

**Delete refuses while the team owns anything.** The mock promised "removes every repo, registry,
workspace and environment in it" — a cascading delete across SlateDB repo databases, registry
blobs and cluster-scoped CRDs, each with its own ownership rules and none of them transactional.
Delete refuses while the team owns repositories (what the directory can see) and says how many.
ponytail: images, workspaces and environments are not counted; extend when one place can.

**Authorization reads `Team.members`, never the URL or a label.** Same rule as `may_act_on` in the
workspaces API. A non-member gets 404, not 403, so the routes cannot be used to probe which
teams exist.

**A person's own namespace is not a team.** It gets no Settings tab; their settings are at
`/settings`. Showing one was what made a fresh account look as if it came with a team.

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

### 3. Members and invitations

- `Directory::create_invite` / `invites_for` / `revoke_invite` / `invite` / `accept_invite`;
  `add_member` is reached only through accept.
- `Directory::remove_member(slug, email)` and `set_role(slug, email, role)`, both refusing to
  strand the team without an owner, each re-asserting the precondition in the update filter.
- `POST /v1/teams/{slug}/invites`, `DELETE .../invites/{id}`, `GET /v1/invites/{token}`,
  `POST /v1/invites/{token}/accept`, `PATCH|DELETE .../members/{email}`.
- Web: invite form, pending list with withdraw, role select, `/invite/{token}` accept page,
  `lib/mail.ts` (Resend), `RESEND_API_KEY`/`RESEND_FROM` from the `rustic-git-mail` Secret.

**Verify:** invite, accept as the invited email, promote, demote, remove all round-trip; accept as
another email is refused; the last owner cannot be demoted or removed.

### 4. Danger zone

- `Directory::delete_team(slug)` refusing while repositories are owned, releasing the handle.
- `DELETE /v1/teams/{slug}`, owner only; typed confirmation in the page.

**Verify:** delete refuses on a team with a repo and says how many.

## Out of scope

Cascading team deletion, per-repo permissions, a return-to after sign-in for invite links (the
person opens the link again). Each is named in the UI where a user would otherwise expect it.
