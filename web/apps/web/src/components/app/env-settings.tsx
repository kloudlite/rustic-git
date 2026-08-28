import { SettingsSection as Section } from "@/components/app/settings-section";
import { DeleteEnvDialog, DeleteSnapshotsDialog } from "@/components/app/env-actions";

/** Everything about an environment that is a setting rather than a fact — which today is the way
 *  out, and the name it cannot change.
 *
 *  An ARCHIVED environment (`archived`) has no object left to configure or delete: the one thing
 *  still deletable is its snapshots, so that is the only section it gets. The tab row stays the
 *  same either way — a row that loses a tab reads as a page that failed to load. */
export function EnvSettings({
  owner,
  id,
  name,
  archived,
}: {
  owner: string;
  id: string;
  name: string;
  archived: boolean;
}) {
  return (
    <div className="grid gap-8">
      <h1 className="text-title font-semibold tracking-title">Environment settings</h1>

      {!archived && (
        <Section title="General" description="How this environment names itself, wherever it is listed.">
          <div className="grid max-w-xl gap-2">
            <div className="border border-border bg-card px-3 py-2 text-sm2">{name}</div>
            <p className="text-caption text-muted-foreground">
              Renaming is not supported yet — the api has no rename, and the name is what its
              namespace and its DNS are built from. Clone it under the name you want instead.
            </p>
          </div>
        </Section>
      )}

      <Section title="Danger zone" description="These cannot be undone." danger>
        <div className="grid max-w-xl gap-3 border border-destructive/40 bg-card p-4">
          <p className="text-sm2 leading-relaxed text-muted-foreground">
            {archived ? (
              <>
                The environment is already gone. Its snapshots are the last copy of that data —
                nothing else references them once they are deleted.
              </>
            ) : (
              <>
                Deleting stops its services, pushes one final snapshot, and removes it from the
                node. Its snapshots survive as an archived row unless you say otherwise in the
                dialog.
              </>
            )}
          </p>
          <div>
            {archived ? (
              <DeleteSnapshotsDialog owner={owner} id={id} name={name} />
            ) : (
              <DeleteEnvDialog owner={owner} id={id} name={name} />
            )}
          </div>
        </div>
      </Section>
    </div>
  );
}
