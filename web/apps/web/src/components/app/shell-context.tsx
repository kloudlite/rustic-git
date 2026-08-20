"use client";

import { createContext, useContext, useEffect, useState } from "react";

/** What the chrome needs to know about the repo being viewed, and cannot work out
 *  from the URL: whether it is public. Set by the repo layout, which has already
 *  read it. */
type RepoMeta = { visibility: "public" | "private" } | null;

const Ctx = createContext<{ meta: RepoMeta; set: (m: RepoMeta) => void }>({
  meta: null,
  set: () => {},
});

export function ShellState({ children }: { children: React.ReactNode }) {
  const [meta, set] = useState<RepoMeta>(null);
  return <Ctx.Provider value={{ meta, set }}>{children}</Ctx.Provider>;
}

export function useRepoMeta() {
  return useContext(Ctx).meta;
}

/** Renders nothing; tells the chrome about the repo underneath it.
 *
 *  The shell stays mounted across every navigation — that is the whole point of
 *  it — so it cannot read a prop from a page that is being replaced beneath it.
 *  It is told instead, and told again when the repo changes. */
export function SetRepoMeta({ visibility }: { visibility: "public" | "private" }) {
  const { set } = useContext(Ctx);
  useEffect(() => {
    set({ visibility });
    return () => set(null);
  }, [visibility, set]);
  return null;
}
