//! The seam between the TUI and the agent. `LocalBackend` (./local) runs the agent in-process; a
//! `RemoteBackend` (./remote) reaches the same thing on the bench over ssh stdio (./wire). The TUI
//! imports only this file and ./remote at runtime, so the laptop binary bundles no agent.
//! Design: docs/superpowers/specs/2026-10-08-local-tui-design.md.
import type {
  AgentSession,
  AgentSessionEvent,
  ClaudeSession,
  listSessions,
  loginOptions,
  loginProvider,
  ModelRef,
  providerAuth,
  readSettings,
  ThinkingLevel,
} from "@kloudlite-tui/agent";
import type { ToolDef } from "@kloudlite-tui/tools";
import type { FileDiff, DiffLine } from "./diff.ts";
import type { Change, ChangeStatus, Match, TreeNode } from "./git.ts";

export type { DiffLine, FileDiff } from "./diff.ts";
export type { Change, ChangeStatus, Match, TreeNode } from "./git.ts";
export type { ModelRef, ThinkingLevel, ToolDef };
export { PROTOCOL } from "./wire.ts";
export { connect, RemoteBackend } from "./remote.ts";

export type Settings = ReturnType<typeof readSettings>;
export type SessionMeta = ReturnType<typeof listSessions>[number];
export type LoginOption = ReturnType<typeof loginOptions>[number];
export type ProviderAuth = Awaited<ReturnType<typeof providerAuth>>[number];
export type LoginType = Parameters<typeof loginProvider>[1];
/** pi's `AuthInteraction`: `{ signal?, prompt(p): Promise<string>, notify(e): void }`. */
export type LoginUi = Parameters<typeof loginProvider>[2];
export type CatalogModel = { provider: string; id: string; name: string; input: string[] };
/** What the sidebar shows, read by the bench from the platform (./space). JSON-safe: it crosses ssh. */
export type SpaceProcess = { id: string; tree?: string; cmd: string; state: string; exit_code?: number | null; failed?: boolean; started_at?: string; logs: { text: string; err?: true }[] };
export type SpaceWorkspace = {
  id: string;
  name: string;
  owner: string;
  state: string;
  repo?: string;
  branch?: string;
  attached_environment?: string;
  parent?: string;
  task?: string;
  processes?: SpaceProcess[];
  changes?: number;
};
export type SpaceEnvironment = {
  id: string;
  name: string;
  owner: string;
  state: string;
  services: { name: string; ports: number[]; interceptedBy?: string }[];
};
export type TaskState = "queued" | "running" | "blocked" | "done" | "failed";
export type BoardTask = { id: string; title: string; workspace?: string; priority: number; dependsOn: string[]; state: TaskState; note?: string; created: number };
export type SpaceView = {
  available: boolean;
  error?: string;
  user: string;
  workspaces: SpaceWorkspace[];
  environments: SpaceEnvironment[];
  /** The environment the space follows (`GET /v1/me/environments`). */
  connected?: string;
  /** Main's task board; only the local backend fills it. */
  tasks?: BoardTask[];
};
export type ToolSpec = Pick<ToolDef, "name" | "description" | "inputSchema">;

export type Hello = {
  protocol: number;
  settings: Settings;
  catalog: CatalogModel[];
  defaultModel: ModelRef;
  logins: LoginOption[];
  sessions: SessionMeta[];
  /** The backend's working directory: what Files browses and the agent edits. */
  cwd: string;
  home: string;
  /** Tools the backend registers itself (web_fetch, web_search). */
  tools: string[];
};

export type PermissionRequest = {
  name: string;
  args: any;
  diff?: FileDiff;
  /** Key of the session that asks, when it is not the one the callback was opened for (delegated sessions). */
  session?: string;
  /** The model's one-sentence reason (because.reason). */
  reason?: string;
  /** A quote the model said the person typed but that failed the check (because.asked). */
  claimed?: string;
};
export type Decision = { block?: boolean; reason?: string };

export type SessionOpts = {
  model: ModelRef;
  fresh?: boolean;
  thinkingLevel?: ThinkingLevel;
  autoCompact?: boolean;
  codemode?: boolean;
  /** Tools the TUI owns; their `run` executes in the TUI process. */
  tools: ToolDef[];
  /** Called only for gated tools: house actions always; bash, exec and web_fetch unless the fence holds (`mustAsk`); never write, edit, patch. Absent on internal opens (a
   * view opened to deliver a reply): cards then go to whichever client holds the session (./clients). */
  permission?(req: PermissionRequest, signal: AbortSignal): Promise<Decision>;
  /** Set only by serve.ts: this view is a person at a client, so what is typed through it counts as their words (consent.ts). */
  client?: boolean;
};

type Image = Parameters<AgentSession["steer"]>[1] extends (infer I)[] | undefined ? I : never;
/** `session_closed`: the agent behind this handle was disposed (rebuilt, or idle with no views);
 * the handle is dead and the client reopens on its next action. */
export type SessionEvent = (AgentSessionEvent & { diff?: FileDiff }) | { type: "session_closed" };

export type SessionHandle = {
  /** Snapshot taken when the session opened; restoreTranscript reads it. */
  messages: AgentSession["messages"];
  isClaude: boolean;
  /** A turn is running right now (live: a client that reconnects mid-turn sees it). */
  readonly busy: boolean;
  prompt(text: string, o?: { images?: Image[] }): Promise<void>;
  steer(text: string, images?: Image[]): Promise<void>;
  followUp(text: string, images?: Image[]): Promise<void>;
  clearQueue(): Promise<void>;
  abort(): Promise<void>;
  dispose(): Promise<void>;
  setModel(ref: ModelRef): Promise<void>;
  setThinkingLevel(level: ThinkingLevel): Promise<void>;
  setAutoCompactionEnabled(on: boolean): Promise<void>;
  subscribe(cb: (e: SessionEvent) => void): () => void;
};

export interface Backend {
  hello(): Promise<Hello>;
  session(key: string, opts: SessionOpts): Promise<SessionHandle>;
  sessions: {
    list(prefix?: string): Promise<SessionMeta[]>;
    name(key: string, name: string): Promise<void>;
    describe(key: string, description: string): Promise<void>;
    clear(key: string): Promise<void>;
  };
  settings: { write(patch: Partial<Settings>): Promise<void> };
  /** The space's real workspaces and environments; never throws, `available: false` says why. */
  space(): Promise<SpaceView>;
  models: { refresh(): Promise<CatalogModel[]> };
  auth: {
    providers(): Promise<ProviderAuth[]>;
    login(provider: string, type: LoginType, ui: LoginUi): Promise<void>;
    claudeSignedIn(fresh?: boolean): Promise<boolean>;
  };
  fs: {
    isGitRepo(root: string): Promise<boolean>;
    changes(root: string): Promise<Change[]>;
    fileDiff(root: string, path: string, status: ChangeStatus): Promise<FileDiff | null>;
    fullFile(root: string, path: string, status?: ChangeStatus): Promise<DiffLine[]>;
    listDir(root: string, rel: string): Promise<TreeNode[]>;
    grep(root: string, query: string, limit?: number): Promise<Match[]>;
  };
  /** The same reads against a workspace's own `~/workspace`, from its pod's ide server. Rejects
   * with the reason when the pod cannot be reached; there is no grep (no pod endpoint). */
  podfs: {
    isGitRepo(ws: string): Promise<boolean>;
    changes(ws: string): Promise<Change[]>;
    fileDiff(ws: string, path: string, status: ChangeStatus): Promise<FileDiff | null>;
    fullFile(ws: string, path: string, status?: ChangeStatus): Promise<DiffLine[]>;
    listDir(ws: string, rel: string): Promise<TreeNode[]>;
  };
}
