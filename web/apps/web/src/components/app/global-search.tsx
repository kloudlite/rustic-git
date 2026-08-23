"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import dynamic from "next/dynamic";
import { Search } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Kbd } from "@/components/ui/kbd";
import { useOwner } from "@/components/app/shell-nav";
import type { SwitcherOwner } from "@/components/app/team-switcher";

const SearchDialog = dynamic(() => import("./search-dialog").then((m) => m.SearchDialog), { ssr: false });

/** ⌘K over everything in the current owner. One list, grouped by section, so the
 *  answer to "where is X" is the same keystroke regardless of what X turns out to
 *  be. Filtering is cmdk's — it scores against the item's text, so the item value
 *  carries the words someone would actually type, not just the display label.
 *
 *  Scope: what this owner has. It is a jump-to, not a content search — nothing
 *  here reads file contents, because no endpoint serves them yet. Only repos are
 *  listed: they are the one thing the api serves a list of.
 *
 *  The dialog itself (and its repo fetch) load only once ⌘K is first opened. */
export function GlobalSearch({
  me,
  owners,
}: {
  me: string;
  owners: SwitcherOwner[];
}) {
  const owner = useOwner(me);
  const [open, setOpen] = useState(false);
  const [opened, setOpened] = useState(false); // once true, stays mounted so reopening is instant
  const router = useRouter();

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "k" && (e.metaKey || e.ctrlKey)) {
        e.preventDefault();
        setOpened(true);
        setOpen((v) => !v);
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, []);

  const go = (href: string) => {
    setOpen(false);
    router.push(href);
  };

  return (
    <>
      <Button
        variant="outline"
        onClick={() => {
          setOpened(true);
          setOpen(true);
        }}
        className="hidden w-64 justify-start border-edge font-normal text-muted-foreground hover:border-edge-hover hover:text-foreground md:flex"
      >
        <Search />
        Search
        <Kbd className="ml-auto">⌘K</Kbd>
      </Button>

      {opened && <SearchDialog owner={owner} owners={owners} open={open} onOpenChange={setOpen} go={go} />}
    </>
  );
}
