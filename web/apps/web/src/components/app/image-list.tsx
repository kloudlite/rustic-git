"use client";

import { useState } from "react";
import Link from "next/link";
import { Check, Copy, Tag } from "lucide-react";
import type { ImageSummary } from "@/lib/browse";
import { cn } from "@/lib/utils";

/** An owner's pushed images. There is no create button — an image exists because
 *  it was pushed, so the empty state is the `docker push` line that makes one,
 *  not a form. */
export function ImageList({ owner, host, images }: { owner: string; host: string; images: ImageSummary[] }) {
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
    <ul className="mt-5 divide-y divide-border border border-border bg-card">
      {images.map((img) => (
        <li key={img.name}>
          <Link
            href={`/${owner}/registries/${encodeURIComponent(img.name)}`}
            className="flex items-start gap-4 px-5 py-4 transition-colors hover:bg-muted/50"
          >
            <span className="min-w-0 flex-1">
              <span className="truncate text-body font-medium">{img.name}</span>
              <span className="mt-1 flex items-center gap-1 text-sm2 text-muted-foreground">
                <Tag className="size-3.5" />
                {img.manifests} {img.manifests === 1 ? "manifest" : "manifests"}
              </span>
            </span>
            <span className="shrink-0" onClick={(e) => e.preventDefault()}>
              <CopyLine value={`docker pull ${host}/${owner}/${img.name}:latest`} compact />
            </span>
          </Link>
        </li>
      ))}
    </ul>
  );
}

function CopyLine({ value, compact }: { value: string; compact?: boolean }) {
  const [copied, setCopied] = useState(false);
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
        onClick={async () => {
          await navigator.clipboard.writeText(value);
          setCopied(true);
          setTimeout(() => setCopied(false), 1500);
        }}
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
