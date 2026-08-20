"use client";

import { useActionState, useState } from "react";
import { Loader2, Tag, Trash2, TriangleAlert } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { FieldLabel } from "@/components/auth/auth-card";
import { SettingsSection as Section } from "@/components/app/settings-section";
import type { ImageTag } from "@/lib/browse";
import {
  destroyImage, removeTag, type SettingsState,
} from "@/app/(shell)/[owner]/(org)/registries/[image]/settings/actions";

/** Hidden inputs rather than closures: these forms post to server actions, and the
 *  image they are about has to travel with the request — same idiom `RepoSettings`
 *  uses for a repo. */
function Which({ owner, image }: { owner: string; image: string }) {
  return (
    <>
      <input type="hidden" name="owner" value={owner} />
      <input type="hidden" name="image" value={image} />
    </>
  );
}

function DeleteTags({ owner, image, tags }: { owner: string; image: string; tags: ImageTag[] }) {
  if (tags.length === 0) {
    return (
      <p className="border border-border bg-card px-4 py-8 text-center text-sm2 text-muted-foreground">
        No tags to delete.
      </p>
    );
  }
  return (
    <ul className="divide-y divide-border border border-border bg-card">
      {tags.map((t) => (
        <li key={t.tag} className="flex items-center gap-4 px-4 py-3">
          <Tag className="size-4 shrink-0 text-muted-foreground" />
          <div className="min-w-0 flex-1">
            <div className="truncate font-mono text-sm2 font-medium">{t.tag}</div>
          </div>
          <form action={removeTag}>
            <Which owner={owner} image={image} />
            <input type="hidden" name="tag" value={t.tag} />
            <Button
              type="submit"
              variant="ghost"
              size="sm"
              className="text-muted-foreground hover:text-destructive"
              aria-label={`Delete the tag ${t.tag}`}
            >
              <Trash2 />
            </Button>
          </form>
        </li>
      ))}
    </ul>
  );
}

function Danger({ owner, image }: { owner: string; image: string }) {
  const [state, action, pending] = useActionState<SettingsState, FormData>(destroyImage, null);
  const [typed, setTyped] = useState("");
  return (
    <div className="border border-destructive/40 bg-card">
      <div className="border-b border-destructive/40 bg-destructive/5 px-4 py-2.5 text-sm2 font-medium">
        Delete this image
      </div>
      <form action={action} className="grid max-w-xl gap-3 p-4">
        <Which owner={owner} image={image} />
        <p className="flex items-start gap-2 text-sm2 leading-relaxed text-muted-foreground">
          <TriangleAlert className="mt-0.5 size-4 shrink-0 text-destructive" />
          Every tag and every manifest are removed; nothing here can be recovered. The layers
          themselves are not deleted immediately — they are reclaimed later by the sweeper, once
          nothing else references them. Because an image exists only by being pushed, it
          disappears from the Container Images list the moment this finishes.
        </p>
        <div className="grid gap-2">
          <FieldLabel htmlFor="confirm">
            Type <span className="font-mono font-semibold text-foreground">{image}</span> to confirm
          </FieldLabel>
          <Input id="confirm" name="confirm" value={typed} onChange={(e) => setTyped(e.target.value)} autoComplete="off" className="h-9 font-mono" />
        </div>
        {state?.error && <p role="alert" className="text-sm2 font-medium text-destructive">{state.error}</p>}
        <div>
          <Button type="submit" variant="destructive" disabled={pending || typed !== image}>
            {pending && <Loader2 className="animate-spin" />}Delete this image
          </Button>
        </div>
      </form>
    </div>
  );
}

/** Everything about the image that is a setting rather than a fact: which tags it
 *  carries, and the way out. Modelled on `RepoSettings` — the same section shape,
 *  the same danger-zone styling, so the two settings pages read as one product. */
export function ImageSettings({ owner, image, tags }: { owner: string; image: string; tags: ImageTag[] }) {
  return (
    <div className="grid gap-8">
      <h1 className="text-title font-semibold tracking-title">Settings</h1>

      <Section title="Delete a tag" description="Removes the tag alone. Other tags on the same manifest, if any, are untouched.">
        <DeleteTags owner={owner} image={image} tags={tags} />
      </Section>

      <Section title="Danger zone" description="These cannot be undone.">
        <Danger owner={owner} image={image} />
      </Section>
    </div>
  );
}
