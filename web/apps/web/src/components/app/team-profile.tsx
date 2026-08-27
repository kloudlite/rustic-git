import Link from "next/link";
import { Globe, Link2, Mail, MapPin, SquareCode, Users } from "lucide-react";
import { Initials } from "@/components/app/initials";
import { RepoList } from "@/components/app/repo-list";
import { Markdown } from "@/components/repo/code";
import type { ApiPublicRepo, ApiTeamProfile } from "@/lib/api";
import type { ImageSummary } from "@/lib/browse";
import type { LanguageShare } from "@/lib/languages";
import { when } from "@/lib/time";

/** Who is reading. A member sees the same page a stranger does, plus the notes
 *  that only make sense to someone who can change it. */
export type ProfileViewer = "anonymous" | "member" | "member-preview-private";

function Heading({ children }: { children: React.ReactNode }) {
  return <h2 className="text-caption font-semibold uppercase tracking-label text-muted-foreground">{children}</h2>;
}

function Meta({ icon: Icon, children }: { icon: typeof Users; children: React.ReactNode }) {
  return (
    <span className="flex items-center gap-1.5">
      <Icon className="size-3.5" aria-hidden />
      {children}
    </span>
  );
}

/**
 * A team as the world sees it: the README, what it chose to pin, and the repos
 * and images anyone can pull. Everything is passed in — the page does the reads,
 * so this stays a pure render and the private half of the app cannot leak in
 * through a fetch nobody noticed.
 */
export function TeamProfile({
  profile,
  readme,
  images,
  languages,
  viewer,
}: {
  profile: ApiTeamProfile;
  readme: string | null;
  images: ImageSummary[];
  languages: LanguageShare[];
  viewer: ProfileViewer;
}) {
  const member = viewer !== "anonymous";
  const byName = new Map(profile.repos.map((r) => [r.name, r]));
  const pinned = profile.pins.map((n) => byName.get(n)).filter((r): r is ApiPublicRepo => !!r);

  // RepoList speaks ApiRepo. A public list has no author to show and no id of its
  // own, so both are synthesised rather than fetched.
  const repos = profile.repos.map((r) => ({
    _id: `${profile.slug}/${r.name}`,
    owner: profile.slug,
    name: r.name,
    public: r.public,
    description: r.description,
    createdBy: "",
    createdAt: r.createdAt,
  }));

  return (
    <>
      <div className="flex items-start gap-5">
        <div className="flex size-24 shrink-0 items-center justify-center border border-border bg-card">
          <Initials name={profile.name || profile.slug} size={8} />
        </div>
        <div className="min-w-0">
          <h1 className="text-title font-semibold tracking-title">{profile.name || profile.slug}</h1>
          {profile.tagline && <p className="mt-1 text-body text-muted-foreground">{profile.tagline}</p>}
          <div className="mt-3 flex flex-wrap items-center gap-x-5 gap-y-1.5 text-sm2 text-muted-foreground">
            <Meta icon={Users}>{profile.memberCount === 1 ? "1 member" : `${profile.memberCount} members`}</Meta>
            {profile.location && <Meta icon={MapPin}>{profile.location}</Meta>}
            {profile.website && (
              <Meta icon={Link2}>
                <a href={profile.website} className="underline-offset-4 transition-colors hover:text-foreground hover:underline">
                  {profile.website.replace(/^https?:\/\//, "")}
                </a>
              </Meta>
            )}
            {profile.email && (
              <Meta icon={Mail}>
                <a href={`mailto:${profile.email}`} className="underline-offset-4 transition-colors hover:text-foreground hover:underline">
                  {profile.email}
                </a>
              </Meta>
            )}
          </div>
        </div>
      </div>

      <div className="mt-10 grid gap-10 xl:grid-cols-overview">
        <div className="min-w-0">
          {readme && (
            <section className="border border-border bg-card p-6">
              <p className="mb-4 font-mono text-caption text-muted-foreground">README.md</p>
              <Markdown source={readme} />
            </section>
          )}

          {pinned.length > 0 && (
            <section className="mt-10">
              <Heading>Pinned</Heading>
              <div className="mt-3 grid gap-4 sm:grid-cols-2">
                {pinned.map((r) => (
                  <div key={r.name} className="border border-border bg-card p-4">
                    <div className="flex items-center gap-2.5">
                      <SquareCode className="size-4 shrink-0 text-muted-foreground" aria-hidden />
                      <Link
                        href={`/${profile.slug}/${r.name}`}
                        className="truncate text-body font-medium underline-offset-4 hover:underline"
                      >
                        {r.name}
                      </Link>
                      {r.public && (
                        <span className="flex shrink-0 items-center gap-1 border border-border px-1.5 py-0.5 text-micro font-medium text-muted-foreground">
                          <Globe className="size-3" />
                          Public
                        </span>
                      )}
                    </div>
                    <p className={`mt-2 line-clamp-2 text-sm2 ${r.description ? "text-muted-foreground" : "text-muted-foreground/50 italic"}`}>
                      {r.description || "No description"}
                    </p>
                  </div>
                ))}
              </div>
            </section>
          )}

          <section className="mt-10">
            <Heading>Repositories</Heading>
            <div className="mt-3">
              <RepoList owner={profile.slug} repos={repos} readOnly />
            </div>
          </section>
        </div>

        <aside className="grid content-start gap-5">
          {member && (
            <section className="border-t border-border pt-5">
              <p className="text-sm2 text-muted-foreground">
                You are viewing the README and pinned repositories as a public user. Workspaces,
                environments and private repositories are hidden.
              </p>
              {viewer === "member-preview-private" && (
                <p className="mt-3 border border-border bg-muted/40 p-3 text-sm2">
                  This team is not public yet. Only members can see this page. Change that in{" "}
                  <Link href={`/${profile.slug}/settings`} className="font-medium underline underline-offset-4">
                    Team settings → Visibility
                  </Link>
                  .
                </p>
              )}
            </section>
          )}

          <section className="border-t border-border pt-5">
            <Heading>People</Heading>
            {/* Members are not listable anonymously, so the count is the whole truth
                this page has about them. */}
            <p className="mt-2.5 text-sm2 text-muted-foreground">
              {profile.memberCount === 1 ? "1 member" : `${profile.memberCount} members`}
            </p>
            {member && (
              <Link
                href={`/${profile.slug}/settings`}
                className="mt-2 inline-block text-sm2 underline-offset-4 hover:underline"
              >
                Team settings
              </Link>
            )}
          </section>

          {languages.length > 0 && (
            <section className="border-t border-border pt-5">
              <Heading>Top languages</Heading>
              <div
                className="mt-2.5 flex h-2 w-full gap-px overflow-hidden"
                role="img"
                aria-label={languages.map((l) => `${l.name} ${l.pct}%`).join(", ")}
              >
                {languages.map((l) => (
                  <span key={l.name} style={{ width: `${l.pct}%`, background: l.color }} />
                ))}
              </div>
              <ul className="mt-2.5 grid grid-cols-2 gap-x-4 gap-y-1">
                {languages.map((l) => (
                  <li key={l.name} className="flex items-center gap-2 text-caption">
                    <span className="size-2 shrink-0" style={{ background: l.color }} aria-hidden />
                    <span className="truncate font-medium">{l.name}</span>
                    <span className="text-muted-foreground">{l.pct}%</span>
                  </li>
                ))}
              </ul>
            </section>
          )}

          {images.length > 0 && (
            <section className="border-t border-border pt-5">
              <Heading>Public images</Heading>
              <ul className="mt-2.5 grid gap-2">
                {images.map((img) => (
                  <li key={img.name} className="flex items-center justify-between gap-3">
                    <Link
                      href={`/${profile.slug}/registries/${img.name}`}
                      className="truncate font-mono text-sm2 underline-offset-4 hover:underline"
                    >
                      {img.name}
                    </Link>
                    {img.updated_ms !== null && (
                      <span className="shrink-0 text-caption text-muted-foreground">{when(img.updated_ms)}</span>
                    )}
                  </li>
                ))}
              </ul>
            </section>
          )}
        </aside>
      </div>
    </>
  );
}
