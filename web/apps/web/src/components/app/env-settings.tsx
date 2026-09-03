import { SettingsSection as Section } from "@/components/app/settings-section";
import { DeleteEnvDialog, DeleteSnapshotsDialog } from "@/components/app/env-actions";

/** Everything about an environment that is a setting rather than a fact — which today is the way
 *  out, and the name it cannot change.
 *
 *  A DELETED environment (`archived`) has no object left to configure or delete: the one thing
 *  still deletable is its snapshots, so that is the only section it gets. The tab row stays the
 *  same either way — a row that loses a tab reads as a page that failed to load. */
export function EnvSettings({
  owner,
  id,
  name,
  archived,
  snapshots,
}: {
  owner: string;
  id: string;
  name: string;
  archived: boolean;
  /** The volume's snapshot count, which both dialogs below name — one to promise they survive
   *  the delete, the other to say how much the delete destroys. */
  snapshots: number;
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
                The environment is already gone. Its snapshots are the last copy of that data, and
                the only thing keeping its volume.
              </>
            ) : (
              <>
                Deleting stops its services, pushes one final snapshot, and removes it from the
                node. Its snapshots survive it — the environment appears under Snapshots on the
                environments page, and deleting them for good lives there.
              </>
            )}
          </p>
          <div>
            {archived ? (
              <DeleteSnapshotsDialog owner={owner} id={id} name={name} snapshots={snapshots} />
            ) : (
              <DeleteEnvDialog owner={owner} id={id} name={name} snapshots={snapshots} />
            )}
          </div>
        </div>
      </Section>
    </div>
  );
}
