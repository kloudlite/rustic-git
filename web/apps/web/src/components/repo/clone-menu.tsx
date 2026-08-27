"use client";

import { Check, ChevronDown, Copy, Download } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import { cn } from "@/lib/utils";
import { useCopy } from "@/lib/use-copy";
import { Input } from "@/components/ui/input";
import type { CloneUrls } from "@/lib/clone";
import { OpenInWorkspace } from "@/components/repo/open-in-workspace";

/** Every way to get the code, in one menu: the three addresses with a copy
 *  button each, and the kloudlite way — a workspace that already has it. */
export function CloneMenu({
  urls, owner, repo, branch,
}: {
  urls: CloneUrls;
  owner: string;
  repo: string;
  /** Absent when the view is on a tag or a bare commit: there is no branch to open. */
  branch?: string;
}) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button><Download />Clone<ChevronDown className="opacity-70" /></Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="w-96 p-0">
        <div className="border-b border-border px-4 py-3">
          <div className="text-sm2 font-medium">Clone</div>
          <div className="text-caption text-muted-foreground">Pick a protocol, copy, and go.</div>
        </div>
        <Tabs defaultValue="https" className="gap-0 px-4 pt-3">
          <TabsList>
            <TabsTrigger value="https">HTTPS</TabsTrigger>
            <TabsTrigger value="ssh">SSH</TabsTrigger>
            <TabsTrigger value="cli">CLI</TabsTrigger>
          </TabsList>
          {(["https", "ssh", "cli"] as const).map((k) => (
            <TabsContent key={k} value={k} className="pt-3 pb-4">
              <CopyRow value={urls[k]} />
              <p className="mt-2 text-caption text-muted-foreground">
                {k === "https" && "Works everywhere. Sign in with a personal access token when prompted."}
                {k === "ssh" && "Uses the SSH keys in your settings. No password prompts."}
                {k === "cli" && "The kloudlite CLI: clones, and sets up the workspace and environment too."}
              </p>
            </TabsContent>
          ))}
        </Tabs>
        {branch && (
          <div className="border-t border-border bg-muted/40 px-4 py-3">
            <OpenInWorkspace
              owner={owner}
              repo={repo}
              branch={branch}
              className="w-full border-edge hover:border-edge-hover"
            />
            <p className="mt-2 text-caption text-muted-foreground">A ready environment with this repo checked out — nothing to install.</p>
          </div>
        )}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function CopyRow({ value }: { value: string }) {
  const { copied, copy } = useCopy(1500);
  return (
    <div className="flex h-9 items-stretch border border-input bg-muted/30">
      <Input
        readOnly
        value={value}
        aria-label="Clone address"
        onFocus={(e) => e.currentTarget.select()}
        className="h-full min-w-0 flex-1 rounded-none border-0 bg-transparent px-3 font-mono text-caption focus-visible:ring-0"
      />
      <Button
        type="button"
        variant="ghost"
        size="icon"
        aria-label={copied ? "Copied" : "Copy"}
        onClick={() => copy(value)}
        className={cn(
          "h-full w-10 shrink-0 rounded-none border-l border-input bg-background",
          copied ? "text-success hover:text-success" : "text-muted-foreground",
        )}
      >
        {copied ? <Check /> : <Copy />}
      </Button>
    </div>
  );
}
