import { notFound, redirect } from "next/navigation";
import { getSession } from "@/lib/session";
import { apiToken } from "@/lib/api-token";
import {
  activity,
  getQuota,
  getTeam,
  getTeamProfile,
  listEnvironments,
  listRepos,
  listWorkspaces,
  type ApiTeamProfile,
} from "@/lib/api";
import { blob, decodeBlob, defaultBranch, publicImages, refs } from "@/lib/browse";
import { pinnedLanguages } from "@/lib/team-languages";
import { Home } from "@/components/app/home";
import { AutoRefresh } from "@/components/app/auto-refresh";
import { TeamProfile, type ProfileViewer } from "@/components/app/team-profile";
import { ViewAs } from "@/components/app/view-as";

/** An owner's home — their own handle or a team's, the same page either way.
 *
 *  Membership is not checked here: the api answers 404 for a namespace the caller
 *  may not act in, so asking it IS the check. Deciding locally would mean two
 *  places that know what a member is, and the browser-facing one would be guessing. */

/** A team's README is a file in a real repo (`.profile/README.md` at its default
 *  branch), read the way a stranger reads it — no token, ever. Nothing there, or a
 *  private `.profile`, is simply no README. */
async function profileReadme(owner: string): Promise<string | null> {
  const all = await refs(undefined, owner, ".profile");
  if (!all.ok) return null;
  const head = defaultBranch(all.value);
  if (!head) return null;
  const file = await blob(undefined, owner, ".profile", head.oid, "README.md");
  if (!file.ok) return null;
  const decoded = decodeBlob(file.value);
  return decoded.binary ? null : decoded.text;
}

/** The public page, whoever is reading it. Its three extra reads are independent of
 *  each other and none of them can fail the page: a missing README, an unreadable
 *  image list and an empty language tally are all just less to show. */
async function publicView(profile: ApiTeamProfile, viewer: ProfileViewer) {
  const [readme, images, languages] = await Promise.all([
    profileReadme(profile.slug),
    publicImages(profile.slug),
    pinnedLanguages(profile.slug, profile.pins),
  ]);
  return (
    <>
      {/* Without this row a member who switched to the public view has no way back
          but editing the URL — the page they are on no longer draws the switch. */}
      {viewer !== "anonymous" && (
        <div className="mb-4 flex h-8 items-center justify-end">
          <ViewAs slug={profile.slug} view="public" />
        </div>
      )}
      <TeamProfile
        profile={profile}
        readme={readme}
        images={images ?? []}
        languages={languages}
        viewer={viewer}
      />
    </>
  );
}

export default async function OwnerPage({
  params,
  searchParams,
}: {
  params: Promise<{ owner: string }>;
  searchParams: Promise<{ view?: string }>;
}) {
  const { owner } = await params;
  const { view } = await searchParams;
  const session = await getSession();
  if (session && !session.user.username) redirect("/welcome");
  const token = session ? await apiToken() : null;

  // Nobody signed in: the public profile is all there is, and there is nothing to
  // preview or switch to. A team with no public profile is a sign-in prompt rather
  // than a 404, which would tell a stranger the team exists.
  if (!session) {
    const profile = await getTeamProfile(owner);
    if (profile.ok) return await publicView(profile.value, "anonymous");
    redirect(`/login?next=${encodeURIComponent(`/${owner}`)}`);
  }
  if (!token) redirect(`/login?next=${encodeURIComponent(`/${owner}`)}`);

  // `getTeam` 404s for a personal namespace as well as for a team you are not in,
  // so a person's own handle is answered locally — and has no public half at all,
  // so `?view=public` on it is meaningless and ignored rather than 404ing.
  const ownHandle = owner === session.user.owner;
  const member = ownHandle ? null : await getTeam(token, owner);

  // Signed in but not a member: `memberView` would 404 on a team whose public profile
  // this caller is perfectly entitled to read. They see exactly what a stranger sees —
  // no switch back to a member view they do not have.
  if (member && !member.ok && !ownHandle) {
    const profile = await getTeamProfile(owner);
    if (!profile.ok) notFound();
    return await publicView(profile.value, "anonymous");
  }

  if (view === "public" && !ownHandle) {
    const profile = await getTeamProfile(owner);
    if (profile.ok) return await publicView(profile.value, member?.ok ? "member" : "anonymous");
    if (member?.ok) {
      // A private team has no public profile to read, so a member previewing one is
      // shown what publishing it WOULD publish, assembled from what they can see.
      // Any OTHER failure is the profile route being down, and a member who cannot
      // preview still gets their own team — never a 404.
      if (profile.kind !== "notFound") return memberView(token, owner, ownHandle, session.user.name, member.value);
      const repos = await listRepos(token, owner);
      const open = (repos.ok ? repos.value : []).filter((r) => r.public);
      const names = new Set(open.map((r) => r.name));
      const t = member.value;
      return await publicView(
        {
          slug: t.slug,
          name: t.name,
          description: t.description,
          tagline: t.tagline,
          location: t.location,
          website: t.website,
          email: t.email,
          memberCount: t.members.length,
          pins: t.pins.filter((p) => names.has(p)),
          repos: open.map((r) => ({
            name: r.name,
            description: r.description,
            public: r.public,
            createdAt: r.createdAt,
          })),
        },
        "member-preview-private",
      );
    }
    notFound();
  }

  return memberView(token, owner, ownHandle, session.user.name, member?.ok ? member.value : null);
}

/** The signed-in member view: home for this namespace. Together: nothing here needs
 *  another's answer, and every strip is decoration — only the repo list can fail the
 *  page, because a namespace whose repos cannot be listed is not one we can show. */
async function memberView(
  token: string,
  owner: string,
  ownHandle: boolean,
  self: string,
  team: { name: string; members: unknown[] } | null,
) {
  const [repos, events, workspaces, environments, quota] = await Promise.all([
    listRepos(token, owner),
    activity(token, owner, 30),
    // No owner filter on the caller's own page — the api then aggregates personal work
    // plus every team they belong to, the same as the list pages do.
    listWorkspaces(token, ownHandle ? undefined : owner),
    listEnvironments(token, ownHandle ? undefined : owner),
    getQuota(owner, token),
  ]);
  if (!repos.ok) {
    // An expired token is a session problem, not a missing namespace.
    if (repos.kind === "unauthorized") redirect("/login?from=expired");
    if (repos.kind === "notFound") notFound();
    throw new Error(repos.message);
  }

  const title = team ? team.name : self;
  return (
    <>
      {/* The workspace and environment strips change on their own; the rest rides along. */}
      <AutoRefresh />
      <Home
        owner={owner}
        title={title}
        subtitle={
          team
            ? `What is running and what happened in ${title}`
            : "Your workspaces, environments and activity"
        }
        canSwitch={!ownHandle}
        members={team ? team.members.length : undefined}
        repos={repos.value}
        workspaces={workspaces.ok ? workspaces.value : []}
        environments={environments.ok ? environments.value : []}
        events={events.ok ? events.value : []}
        // A read failure here is one lost section, not a broken page — see the "renders the
        // section absent" note on `<QuotaBar>`'s call site.
        quota={quota.ok ? quota.value : null}
      />
    </>
  );
}
