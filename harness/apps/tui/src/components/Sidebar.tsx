import { TextAttributes } from "@opentui/core";
import { theme } from "../theme.ts";
import { clip } from "../ui/text.ts";
import { type Service, type Workspace } from "../workspaces.ts";
import { useWheelAccel } from "../wheel.ts";

// resolved per render: the theme singleton mutates on /theme


/**
 * Section label: bold title-case, then a hairline across the rest of the row —
 * the same separator the session title bar uses, instead of SHOUTING CAPS.
 */
function Heading({ children, count, width, flush }: { children: string; count?: number; width: number; flush?: boolean }) {
  const label = count === undefined ? children : `${children}  ${count}`;
  return (
    <box paddingLeft={1} paddingRight={1} height={1} marginTop={flush ? 0 : 1}>
      <text selectable={false}>
        <span fg={theme.muted} attributes={TextAttributes.BOLD}>{children}</span>
        {count !== undefined && (
          <span fg={theme.border}>{"  "}{String(count)}</span>
        )}
        <span fg={theme.border}>{` ${"─".repeat(Math.max(0, width - label.length - 3))}`}</span>
      </text>
    </box>
  );
}

/** Label left, meta right; a raised band when selected. */
function Row({
  left,
  right,
  on = false,
  onMouseDown,
}: {
  left: React.ReactNode;
  right?: React.ReactNode;
  on?: boolean;
  onMouseDown?: () => void;
}) {
  return (
    <box
      flexDirection="row"
      justifyContent="space-between"
      height={1}
      overflow="hidden"
      paddingLeft={1}
      paddingRight={1}
      backgroundColor={on ? theme.surfaceRaised : undefined}
      onMouseDown={onMouseDown}
    >
      <text selectable={false}>{left}</text>
      <text selectable={false}>
        {right}
      </text>
    </box>
  );
}

/**
 * Session header, WORKSPACES (each with its ephemeral tasks under a guide
 * rail), and a pinned Environment block at the bottom — the connected
 * environment with the services that belong to it directly under it.
 */
export function Sidebar({
  workspaces,
  services,
  envName,
  envOwner,
  user,
  unavailable,
  snapshot,
  running,
  waiting,
  focus,
  width,
  onFocus,
}: {
  workspaces: Workspace[];
  services: Service[];
  /** Undefined = the space has no environment: the block renders nothing. */
  envName?: string;
  envOwner?: string;
  /** The signed-in user: only their workspaces are focusable. */
  user: string;
  /** Why the platform could not be read; replaces the workspace list. */
  unavailable?: string;
  /** The restore point the environment is on, if the agent switched to one. */
  snapshot?: string;
  /** Per-workspace: its session has a turn running */
  running: boolean[];
  /** Per-workspace: a permission card or question is waiting for the user */
  waiting: boolean[];
  /** 0 = the session, 1..N = workspace */
  focus: number;
  width: number;
  onFocus?: (focus: number) => void;
  /** The environment row opens the connect picker. */
}) {
  const inner = width - 4; // padding + row padding
  const wheel = useWheelAccel();
  // the tree counts workspaces, not their ephemeral tasks
  const count = workspaces.filter((w) => !w.parent).length;

  return (
    <box flexDirection="column" width={width} height="100%" flexShrink={0} backgroundColor={theme.sidebarBg}>
      {/* session header: the product mark, then what we're connected to */}
      <box
        flexDirection="row"
        height={3}
        alignItems="center"
        paddingLeft={2}
        backgroundColor={focus === 0 ? theme.surfaceRaised : theme.surface}
        onMouseDown={onFocus ? () => onFocus(0) : undefined}
      >
        <text selectable={false}>
          <span fg={theme.accent}>✦ </span>
          <span fg={theme.fg} attributes={TextAttributes.BOLD}>Working</span>
          <span fg={theme.muted}> Session</span>
        </text>
      </box>

      <scrollbox flexGrow={1} flexBasis={0} flexShrink={1} paddingLeft={1} paddingRight={1} scrollbarOptions={{ visible: false }} scrollAcceleration={wheel}>
        <box flexDirection="column" flexShrink={0}>
          <Heading count={count} width={width - 2} flush>Workspaces</Heading>
          {unavailable && <Row left={<span fg={theme.muted}>{clip(`platform not reachable: ${unavailable}`, inner - 2)}</span>} />}
          {!unavailable && workspaces.length === 0 && (
            <Row left={<span fg={theme.muted}>none yet — a to create one</span>} />
          )}
          {workspaces.map((w, i) => {
            const mine = w.owner === user;
            const on = mine && focus === i + 1;
            const click = onFocus && mine ? () => onFocus(i + 1) : undefined;

            if (w.parent) {
              // last task of its workspace closes the branch
              const last = !workspaces.some((o, j) => j > i && o.parent === w.parent);
              // an ephemeral workspace: branch glyph, short name, and the
              // same one state word its parent gets — these are running agents,
              // so they must read as plainly as the workspace above them
              const tag = waiting[i] ? "needs you" : running[i] ? "working" : "";
              return (
                <Row
                  key={w.id}
                  on={on}
                  onMouseDown={click}
                  left={
                    <span>
                      <span fg={theme.border}>{last ? "└ " : "├ "}</span>
                      <span fg={on ? theme.accent : theme.fg}>
                        {clip(w.name, inner - 3 - tag.length)}
                      </span>
                    </span>
                  }
                  right={
                    <span
                      fg={waiting[i] ? theme.warning : running[i] ? theme.accent : theme.muted}
                    >
                      {tag}
                    </span>
                  }
                />
              );
            }

            // one word on the right, and only when it earns the space
            const stateTag = waiting[i]
              ? "needs you"
              : running[i]
              ? "working"
              : w.status === "cloning"
                ? w.progress ?? "cloning"
                : !mine
                  ? w.owner
                  : "";
            return (
              <Row
                key={w.id}
                on={on}
                onMouseDown={click}
                left={
                  <span>
                    <span
                      fg={on ? theme.accent : w.status === "stopped" ? theme.muted : theme.fg}
                      attributes={on ? TextAttributes.BOLD : undefined}
                    >
                      {clip(w.name, inner - 1 - stateTag.length)}
                    </span>
                  </span>
                }
                right={
                  <span
                    fg={waiting[i] ? theme.warning : running[i] ? theme.accent : theme.muted}
                  >
                    {stateTag}
                  </span>
                }
              />
            );
          })}
        </box>
      </scrollbox>

      {/* pinned: the one environment, and the services it runs */}
      {envName !== undefined && <box flexDirection="column" flexShrink={0} paddingBottom={1}>
        <box flexDirection="column" paddingLeft={1} paddingRight={1}>
        <Heading width={width - 2} flush>Environment</Heading>
        <Row
          left={
            <span>
              {envOwner ? <span fg={theme.muted}>{envOwner}/</span> : ""}
              <span fg={theme.fg} attributes={TextAttributes.BOLD}>{envName}</span>
            </span>
          }
        />

        {services.map((svc) => (
          <Row
            key={svc.name}
            left={
              <span>
                <span fg={theme.border}>{"  "}</span>
                <span fg={svc.interceptedBy ? theme.accent : theme.fg}>{svc.name}</span>
              </span>
            }
            right={
              <span>
                {svc.interceptedBy ? (
                  <span fg={theme.accent}>{"→ "}{svc.interceptedBy}{"  "}</span>
                ) : ""}
                <span fg={theme.muted}>{svc.proto ?? "tcp"}:{svc.port}</span>
              </span>
            }
          />
        ))}
        {snapshot ? (
          // last row of the block: labelled, because a bare name reads like a
          // tag and would collide with a long env name up on the env row
          <>
            <box height={1} flexShrink={0} />
            <Row
              left={
                <span>
                  <span fg={theme.muted}>{"current snapshot: "}</span>
                  <span fg={theme.fg}>{clip(snapshot, inner - 18)}</span>
                </span>
              }
            />
          </>
        ) : null}
        </box>
      </box>}
    </box>
  );
}
