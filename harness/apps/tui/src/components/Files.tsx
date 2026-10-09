import { useEffect, useMemo, useRef, useState } from "react";
import { useKeyboard } from "@opentui/react";
import { TextAttributes, type ScrollBoxRenderable } from "@opentui/core";
import { theme } from "../theme.ts";
import { SplitBorder } from "../ui/border.ts";
import { DiffView } from "./Diff.tsx";
import { SPECIAL } from "./Input.tsx";
import type { FileDiff } from "../diff.ts";
import { useWheelAccel } from "../wheel.ts";
import { backend } from "../hello.ts";
import { filesApi, NO_SEARCH, unreachable } from "../filesApi.ts";
import { displayRoot, type Change, type Match, type TreeNode } from "../git.ts";

/** One selectable row in the left pane. */
type Row =
  | { kind: "header"; label: string; extra?: string }
  | { kind: "change"; change: Change }
  | { kind: "node"; node: TreeNode; depth: number };

/**
 * Files view: one list holding both CHANGES (vs HEAD) and the FILES tree,
 * with a reader pane on the right (diff, or the full file). `/` filters paths,
 * `s` searches contents; esc backs out a layer at a time. `/` filters paths, `s` searches file
 * contents; both work in either mode. NORMAL-mode letter keys; esc goes back.
 */
export function Files({
  root,
  workspace,
  refreshKey,
  onClose,
  onCycle,
}: {
  root: string;
  /** A workspace view reads that workspace's `~/workspace` from its pod, never the bench's files. */
  workspace?: string;
  /** bump to re-scan (agent finished an edit/write) */
  refreshKey: number;
  onClose: () => void;
  /** `f` moves on to the next view, same as outside. */
  onCycle: () => void;
}) {
  const wheel = useWheelAccel();
  const fs = filesApi(workspace, backend());
  const [podError, setPodError] = useState<string | null>(null);
  const down = (e: unknown) => workspace && setPodError(unreachable(e));
  const [changes, setChanges] = useState<Change[]>([]);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [dirCache, setDirCache] = useState<Record<string, TreeNode[]>>({});
  const [sel, setSel] = useState(0);
  const [pane, setPane] = useState<"tree" | "diff">("tree");
  const [open, setOpen] = useState<{ path: string; status?: Change["status"] } | null>(null);
  const [view, setView] = useState<"diff" | "full">("diff");
  // one typing prompt for both "/" (filter paths) and "s" (search contents)
  const [prompt, setPrompt] = useState<{ kind: "filter" | "search"; text: string } | null>(null);
  const [filter, setFilter] = useState("");
  const [search, setSearch] = useState<{ query: string; matches: Match[] } | null>(null);
  const [matchIdx, setMatchIdx] = useState(0);
  const [flash, setFlash] = useState(false);
  const scrollRef = useRef<ScrollBoxRenderable>(null);
  const [git, setGit] = useState(false);
  useEffect(() => {
    fs.isGitRepo(root).then((g) => (setGit(g), setPodError(null))).catch(down);
  }, [root]);

  const rescan = () => {
    fs.changes(root).then((c) => (setChanges(c), setPodError(null))).catch((e) => (setChanges([]), down(e)));
    setDirCache({});
  };
  useEffect(rescan, [root]);
  useEffect(() => {
    if (refreshKey === 0) return;
    rescan();
    setFlash(true);
    const t = setTimeout(() => setFlash(false), 1200);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [refreshKey]);

  const dir = (rel: string): TreeNode[] => {
    if (!(rel in dirCache)) {
      dirCache[rel] = []; // mark in flight; never re-request while it loads
      fs.listDir(root, rel)
        .then((nodes) => setDirCache((c) => ({ ...c, [rel]: nodes })))
        .catch(down);
    }
    return dirCache[rel]!;
  };

  const changeStatus = (path: string) => changes.find((c) => c.path === path)?.status;

  // flatten the visible tree
  const rows: Row[] = useMemo(() => {
    const out: Row[] = [];
    const total = changes.reduce((n, c) => ({ a: n.a + c.added, r: n.r + c.removed }), { a: 0, r: 0 });
    out.push({
      kind: "header",
      label: "CHANGES",
      extra: changes.length ? `${changes.length} · +${total.a} −${total.r}` : git ? "clean" : "no git",
    });
    for (const c of changes) out.push({ kind: "change", change: c });
    out.push({ kind: "header", label: "FILES" });
    const walk = (rel: string, depth: number) => {
      for (const node of dir(rel)) {
        if (filter && !node.dir && !node.path.toLowerCase().includes(filter.toLowerCase())) continue;
        out.push({ kind: "node", node, depth });
        if (node.dir && !node.ignored && (expanded.has(node.path) || filter)) walk(node.path, depth + 1);
      }
    };
    walk("", 0);
    return out;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [changes, expanded, dirCache, filter, git]);

  const selectable = rows.map((r, i) => (r.kind === "header" ? -1 : i)).filter((i) => i >= 0);
  const cur = selectable.includes(sel) ? sel : (selectable[0] ?? 0);

  const openRow = (row: Row) => {
    if (row.kind === "change") {
      setOpen({ path: row.change.path, status: row.change.status });
      setView("diff");
    } else if (row.kind === "node") {
      if (row.node.dir) {
        if (row.node.ignored) return;
        setExpanded((e) => {
          const n = new Set(e);
          n.has(row.node.path) ? n.delete(row.node.path) : n.add(row.node.path);
          return n;
        });
      } else {
        // browsing files shows the file; diffs belong to the CHANGES rows
        setOpen({ path: row.node.path, status: changeStatus(row.node.path) });
        setView("full");
      }
    }
  };

  const shownChanges = useMemo(
    () => changes.filter((c) => !filter || c.path.toLowerCase().includes(filter.toLowerCase())),
    [changes, filter],
  );
  const [diff, setDiff] = useState<FileDiff | null>(null);
  useEffect(() => {
    if (!open?.status) return setDiff(null);
    let live = true;
    fs.fileDiff(root, open.path, open.status)
      .then((d) => live && setDiff(d))
      .catch(() => {});
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, changes, refreshKey]);
  const [full, setFull] = useState<FileDiff | null>(null);
  useEffect(() => {
    if (!open) return setFull(null);
    let live = true;
    fs.fullFile(root, open.path, open.status)
      .then((lines) => live && setFull({ path: open.path, lines, added: 0, removed: 0 }))
      .catch(() => {});
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, refreshKey]);
  const body: FileDiff | null = useMemo(() => {
    if (!open) return null;
    if (view === "diff" && diff) return diff;
    return full && { ...full, added: diff?.added ?? 0, removed: diff?.removed ?? 0 };
  }, [open, view, diff, full]);
  // hunk starts: rows where a change run begins (in full view, tinted runs)
  const hunks = useMemo(() => {
    if (!body) return [] as number[];
    const idx: number[] = [];
    body.lines.forEach((l, i) => {
      const changed = l.sign !== " ";
      const prevChanged = i > 0 && body.lines[i - 1]!.sign !== " ";
      if (changed && !prevChanged) idx.push(i);
    });
    return idx;
  }, [body]);
  const [hunk, setHunk] = useState(0);
  useEffect(() => setHunk(0), [open]);

  /** Open a path in the reader pane, optionally scrolled to a line. */
  function openPath(path: string, line?: number) {
    setOpen({ path, status: changeStatus(path) });
    setView(changeStatus(path) && line === undefined ? "diff" : "full");
    setPane("diff");
    setSearch(null);
    if (line !== undefined) setTimeout(() => scrollRef.current?.scrollTo(Math.max(0, line - 3)), 30);
  }

  useKeyboard((key) => {
    // ^f is the cycle key when vim is off (app.tsx command()); the app's own handler is parked while this view is up
    if (key.ctrl && key.name === "f" && !key.meta) return onCycle();
    if (key.ctrl || key.meta || key.option) return;
    // "/" filter and "s" search share one typing prompt
    if (prompt) {
      if (key.name === "escape") {
        if (prompt.kind === "filter") setFilter("");
        return setPrompt(null);
      }
      if (key.name === "return") {
        setPrompt((p) => {
          if (p?.kind === "search") {
            const query = p.text.trim();
            if (!query) setSearch(null);
            else
              fs.grep(root, query)
                .then((matches) => setSearch({ query, matches }))
                .catch(() => setSearch({ query, matches: [] }));
            setMatchIdx(0);
          }
          return null;
        });
        return;
      }
      if (key.name === "backspace" || key.name === "delete") {
        setPrompt((p) => {
          if (!p) return p;
          const text = p.text.slice(0, -1);
          if (p.kind === "filter") setFilter(text);
          return { ...p, text };
        });
        return;
      }
      if (SPECIAL.has(key.name)) return;
      const t = key.sequence;
      if (t && !t.startsWith("\x1b") && t >= " ") {
        setPrompt((p) => {
          if (!p) return p;
          const text = p.text + t;
          if (p.kind === "filter") setFilter(text);
          return { ...p, text };
        });
      }
      return;
    }
    if (key.name === "escape") {
      if (search) return setSearch(null);
      if (filter) return setFilter("");
      return onClose();
    }
    if (key.sequence === "/") return setPrompt({ kind: "filter", text: filter });
    if (key.name === "s" && workspace) return setPodError(NO_SEARCH);
    if (key.name === "s") return setPrompt({ kind: "search", text: search?.query ?? "" });
    if (key.name === "f") return onCycle();
    if (key.name === "r") return rescan();
    // search results own the keyboard while they're up
    if (search) {
      const n = search.matches.length;
      if (!n) return;
      if (key.name === "j" || key.name === "down") return setMatchIdx((i) => Math.min(n - 1, i + 1));
      if (key.name === "k" || key.name === "up") return setMatchIdx((i) => Math.max(0, i - 1));
      if (key.name === "return" || key.name === "l") {
        const m = search.matches[matchIdx]!;
        return openPath(m.path, m.line);
      }
      return;
    }
    if (key.name === "tab") return setPane((p) => (p === "tree" ? "diff" : "tree"));
    if (pane === "tree") {
      const pos = Math.max(0, selectable.indexOf(cur));
      if (key.name === "j" || key.name === "down") return setSel(selectable[Math.min(selectable.length - 1, pos + 1)] ?? cur);
      if (key.name === "k" || key.name === "up") return setSel(selectable[Math.max(0, pos - 1)] ?? cur);
      if (key.name === "l" || key.name === "return") {
        const row = rows[cur];
        if (!row) return;
        const isFile = row.kind === "change" || (row.kind === "node" && !row.node.dir);
        const already = isFile && open?.path === (row.kind === "change" ? row.change.path : row.kind === "node" ? row.node.path : "");
        openRow(row);
        // second l/enter on the opened file (or enter) expands into the reader pane
        if (isFile && (already || key.name === "return")) setPane("diff");
        return;
      }
      if (key.name === "h") {
        const row = rows[cur];
        if (row?.kind === "node") {
          if (row.node.dir && expanded.has(row.node.path)) {
            setExpanded((e) => {
              const n = new Set(e);
              n.delete(row.node.path);
              return n;
            });
          } else {
            // jump to parent dir row
            const parent = row.node.path.split("/").slice(0, -1).join("/");
            const pi = rows.findIndex((r) => r.kind === "node" && r.node.path === parent);
            if (pi >= 0) setSel(pi);
          }
        }
        return;
      }
      return;
    }
    // diff pane
    const sb = scrollRef.current;
    const page = Math.max(1, (sb?.viewport.height ?? 20) - 2);
    if (key.name === "j" || key.name === "down") sb?.scrollBy(1);
    if (key.name === "k" || key.name === "up") sb?.scrollBy(-1);
    if (key.name === "d") sb?.scrollBy(Math.ceil(page / 2));
    if (key.name === "u") sb?.scrollBy(-Math.ceil(page / 2));
    // v flips this file between its diff and its full contents
    if (key.name === "v" && open?.status) setView((v) => (v === "diff" ? "full" : "diff"));
    if (key.name === "h") setPane("tree");
    if (key.name === "n" || (key.name === "N" && key.shift)) {
      if (!hunks.length) return;
      const next = key.shift ? (hunk - 1 + hunks.length) % hunks.length : (hunk + 1) % hunks.length;
      setHunk(next);
      sb?.scrollTo(hunks[next]!);
    }
  });


  const statusColor = (s?: Change["status"]) =>
    s === "A" ? theme.diffAdded : s === "D" ? theme.diffRemoved : s === "M" ? theme.warning : theme.muted;

  return (
    <box flexDirection="column" flexGrow={1} minHeight={0}>
      {/* header */}
      <box flexDirection="row" justifyContent="space-between" paddingLeft={1} paddingRight={1}>
        <text>
          <span fg={theme.muted}>{workspace ? "~/workspace" : displayRoot(root)}</span>
          {podError ? <span fg={theme.error}>  {podError}</span> : ""}
          {flash ? <span fg={theme.success}>  ● updated</span> : ""}
        </text>
        <text fg={theme.muted}>
          {prompt ? (
            <span>
              <span fg={theme.accent}>{prompt.kind === "filter" ? "/" : "search "}</span>
              <span fg={theme.fg}>{prompt.text}</span>
              <span attributes={TextAttributes.INVERSE}> </span>
            </span>
          ) : search ? (
            <span>
              <span fg={theme.accent}>search </span>
              <span fg={theme.fg}>{search.query}</span>
              <span fg={theme.muted}> · {search.matches.length} matches</span>
            </span>
          ) : filter ? (
            <span>
              <span fg={theme.accent}>/</span>
              <span fg={theme.fg}>{filter}</span>
              <span fg={theme.muted}> · {shownChanges.length}/{changes.length} changed</span>
            </span>
          ) : (
            `${changes.length} changed`
          )}
        </text>
      </box>

      {search ? (
        <scrollbox flexGrow={1} flexBasis={0} minHeight={0} marginTop={1} paddingLeft={1} scrollbarOptions={{ visible: false }} scrollAcceleration={wheel}>
          <box flexDirection="column" flexShrink={0} width="100%">
          {search.matches.length === 0 ? (
            <box paddingLeft={1} paddingTop={1}>
              <text fg={theme.muted}>no matches for "{search.query}" — s searches again, esc clears</text>
            </box>
          ) : (
            search.matches.map((m, i) => {
              const on = i === matchIdx;
              return (
                <box
                  key={`${m.path}:${m.line}:${i}`}
                  height={1}
                  overflow="hidden"
                  paddingLeft={1}
                  backgroundColor={on ? theme.selection : undefined}
                  onMouseDown={() => {
                    setMatchIdx(i);
                    openPath(m.path, m.line);
                  }}
                >
                  <text selectable={false}>
                    <span fg={on ? theme.bg : theme.fg}>{m.path}</span>
                    <span fg={on ? theme.bg : theme.muted}>:{m.line}</span>
                    <span fg={on ? theme.bg : theme.muted}>  {m.text}</span>
                  </text>
                </box>
              );
            })
          )}
          </box>
        </scrollbox>
      ) : (
      <box flexDirection="row" flexGrow={1} minHeight={0} marginTop={1}>
        {/* left: tree */}
        <box
          flexDirection="column"
          width={open ? "42%" : "100%"}
          minWidth={24}
          flexShrink={0}
          paddingLeft={1}
          onMouseDown={() => setPane("tree")}
        >
          <scrollbox flexGrow={1} flexBasis={0} scrollbarOptions={{ visible: false }} scrollAcceleration={wheel}>
            {rows.map((row, i) => {
              const active = i === cur && pane === "tree";
              if (row.kind === "header")
                return (
                  <box key={`h${row.label}`} marginTop={i === 0 ? 0 : 1} flexDirection="row" justifyContent="space-between" paddingRight={1}>
                    <text fg={theme.muted}><b>{row.label}</b></text>
                    {row.extra ? <text fg={theme.muted}>{row.extra}</text> : null}
                  </box>
                );
              if (row.kind === "change")
                return (
                  <box key={`c${row.change.path}`} flexDirection="column">
                    <box
                      flexDirection="row"
                      height={1}
                      overflow="hidden"
                      backgroundColor={active ? theme.selection : undefined}
                      paddingLeft={1}
                      onMouseDown={() => {
                        setSel(i);
                        setPane("tree");
                        openRow(row);
                      }}
                    >
                      <text selectable={false} fg={active ? theme.bg : statusColor(row.change.status)}>{row.change.status} </text>
                      <text selectable={false} fg={active ? theme.bg : theme.fg}>{row.change.path}</text>
                    </box>
                  </box>
                );
              const n = row.node;
              const st = n.dir ? undefined : changeStatus(n.path);
              const glyph = n.dir ? (n.ignored ? "  " : expanded.has(n.path) ? "▾ " : "▸ ") : "  ";
              return (
                <box key={`n${n.path}`} flexDirection="column">
                  <box
                    flexDirection="row"
                    height={1}
                    overflow="hidden"
                    backgroundColor={active ? theme.selection : undefined}
                    paddingLeft={1 + row.depth * 2}
                    onMouseDown={() => {
                      setSel(i);
                      setPane("tree");
                      openRow(row);
                    }}
                  >
                    <text selectable={false} fg={active ? theme.bg : n.ignored ? theme.placeholder : n.dir ? theme.fg : st ? theme.fg : theme.muted}>
                      {glyph}{n.name}{n.dir ? "/" : ""}
                    </text>
                    {st ? <text fg={active ? theme.bg : statusColor(st)}> {st}</text> : null}
                  </box>
                </box>
              );
            })}
          </scrollbox>
        </box>

        {/* right: the reader — the one place a diff or file body is rendered */}
        <box
          flexDirection="column"
          flexGrow={1}
          minHeight={0}
          {...SplitBorder}
          border={["left"]}
          borderColor={pane === "diff" ? theme.accent : theme.border}
          onMouseDown={() => setPane("diff")}
        >
          {!open || !body ? (
            <box paddingLeft={2} paddingTop={1}>
              <text fg={theme.muted}>select a file — l/enter opens · v diff/full · tab switches panes · / filters · s searches</text>
            </box>
          ) : (
            <>
              <box flexDirection="row" justifyContent="space-between" paddingLeft={2} paddingRight={2}>
                <text>
                  <span fg={theme.muted}>{view === "diff" ? "diff of " : ""}</span>
                  <span fg={theme.fg}>{open.path}</span>
                  {open.status && view === "diff" ? (
                    <span>
                      <span fg={theme.diffAdded}>  +{body.added}</span>
                      <span fg={theme.diffRemoved}> −{body.removed}</span>
                    </span>
                  ) : (
                    ""
                  )}
                </text>
                <text fg={theme.muted}>
                  {view === "diff"
                    ? hunks.length > 1
                      ? `hunk ${hunk + 1}/${hunks.length} · n next · v file`
                      : "v shows the file"
                    : open.status
                      ? `${body.lines.length} lines · v shows the diff`
                      : `${body.lines.length} lines`}
                </text>
              </box>
              <scrollbox ref={scrollRef} flexGrow={1} flexBasis={0} marginTop={1} paddingLeft={1} scrollbarOptions={{ visible: false }} scrollAcceleration={wheel}>
                <DiffView diff={body} maxLines={5000} />
              </scrollbox>
            </>
          )}
        </box>
      </box>
      )}

      {/* footer hints */}
      <box flexDirection="row" gap={2} paddingLeft={1} marginTop={1}>
        <text fg={theme.muted}>
          {search
            ? `search › ${search.matches[matchIdx]?.path ?? "—"}`
            : `files › ${open?.path ?? "—"}`}
        </text>
        <box flexGrow={1} />
        {search ? (
          <>
            <text fg={theme.fg}>j k <span fg={theme.muted}>match</span></text>
            <text fg={theme.fg}>enter <span fg={theme.muted}>open at line</span></text>
            <text fg={theme.fg}>s <span fg={theme.muted}>search again</span></text>
          </>
        ) : (
          <>
            <text fg={theme.fg}>j k <span fg={theme.muted}>move</span></text>
            <text fg={theme.fg}>l <span fg={theme.muted}>open</span></text>
            <text fg={theme.fg}>h <span fg={theme.muted}>up</span></text>
            <text fg={theme.fg}>tab <span fg={theme.muted}>pane</span></text>
            <text fg={theme.fg}>n N <span fg={theme.muted}>hunks</span></text>
            <text fg={theme.fg}>v <span fg={theme.muted}>diff/full</span></text>
            <text fg={theme.fg}>/ <span fg={theme.muted}>filter</span></text>
            <text fg={theme.fg}>s <span fg={theme.muted}>search</span></text>
          </>
        )}
        <text fg={theme.fg}>esc <span fg={theme.muted}>back</span></text>
      </box>
    </box>
  );
}
