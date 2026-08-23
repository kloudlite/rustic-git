"use client";

import { useMemo, useState } from "react";
import Link from "next/link";
import { Check, Copy, Package, Search } from "lucide-react";
import type { ImageSummary } from "@/lib/browse";
import { cn } from "@/lib/utils";
import { useCopy } from "@/lib/use-copy";
import { when } from "@/lib/time";
import { Input } from "@/components/ui/input";

/** An owner's pushed images, filtered the same way repo-list filters repos: locally,
 *  live, by name — the whole list is already here, so a round trip per keystroke
 *  would be slower and no more correct. There is no create button — an image exists
 *  because it was pushed, so the empty state is the `docker push` line that makes
 *  one, not a form. */
export function ImageList({ owner, host, images }: { owner: string; host: string; images: ImageSummary[] }) {
  const [q, setQ] = useState("");

  const shown = useMemo(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return images;
    return images.filter((img) => img.name.toLowerCase().includes(needle));
  }, [images, q]);

  if (images.length === 0) {
    return (
      <div className="mt-5 border border-border bg-card px-5 py-14 text-center">
        <p className="text-sm2 font-medium">No images yet</p>
        <p className="mx-auto mt-1 max-w-md text-sm2 text-muted-foreground">
          Images show up here once you push one. From wherever you build:
        </p>
        <div className="mx-auto mt-5 max-w-md space-y-2 text-left">
          <CopyLine value={`docker login ${host} -u ${owner}`} />
          <CopyLine value={`docker tag <image> ${host}/${owner}/<name>:latest`} />
          <CopyLine value={`docker push ${host}/${owner}/<name>:latest`} />
        </div>
      </div>
    );
  }

  return (
    <>
      <div className="flex flex-wrap items-center gap-3">
        <div className="relative w-full max-w-xs">
          <Search className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="Filter images"
            aria-label="Filter images"
            className="h-8 pl-8 text-sm2"
          />
        </div>
        <span className="text-sm2 text-muted-foreground">
          {images.length} {images.length === 1 ? "image" : "images"}
        </span>
      </div>

      {shown.length === 0 ? (
        <p className="mt-5 border border-border bg-card px-5 py-12 text-center text-sm2 text-muted-foreground">
          Nothing matches that.
        </p>
      ) : (
      <ul className="mt-5 divide-y divide-border border border-border bg-card">
        {shown.map((img) => (
          <li key={img.name} className="flex items-start gap-4 px-5 py-4 transition-colors hover:bg-muted/50">
            <Link
              href={`/${owner}/registries/${encodeURIComponent(img.name)}`}
              className="flex min-w-0 flex-1 items-start gap-4"
            >
              <Package className="mt-0.5 size-4 shrink-0 text-muted-foreground" aria-hidden />
              <span className="min-w-0 flex-1">
                <span className="truncate text-body font-medium">{img.name}</span>
                <span className="mt-1 block text-sm2 text-muted-foreground">
                  {img.updated_ms === null ? "Updated unknown" : `Updated ${when(img.updated_ms)}`}
                  {" · "}
                  {img.manifests} {img.manifests === 1 ? "manifest" : "manifests"}
                </span>
              </span>
            </Link>
            <span className="shrink-0">
              <CopyLine value={`docker pull ${host}/${owner}/${img.name}:latest`} compact />
            </span>
          </li>
        ))}
      </ul>
      )}
    </>
  );
}

export function CopyLine({ value, compact }: { value: string; compact?: boolean }) {
  const { copied, copy } = useCopy(1500);
  return (
    <div
      className={cn(
        "flex items-stretch border border-input bg-muted/30",
        compact ? "h-7 w-72" : "h-9",
      )}
    >
      <code className="min-w-0 flex-1 truncate px-3 py-1.5 font-mono text-caption leading-tight">{value}</code>
      <button
        type="button"
        aria-label={copied ? "Copied" : "Copy"}
        onClick={() => copy(value)}
        className={cn(
          "flex w-8 shrink-0 items-center justify-center border-l border-input bg-background",
          copied ? "text-success" : "text-muted-foreground hover:text-foreground",
        )}
      >
        {copied ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
      </button>
    </div>
  );
}
