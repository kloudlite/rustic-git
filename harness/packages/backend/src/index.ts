//! The seam between the TUI and the agent. `LocalBackend` (./local) runs the agent in-process; a
//! `RemoteBackend` (./remote) reaches the same thing on the bench over a websocket through bench-proxy (./wire). The TUI
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
export type LiveSessionMeta = SessionMeta & { busy: boolean };
export type LoginOption = ReturnType<typeof loginOptions>[number];
export type ProviderAuth = Awaited<ReturnType<typeof providerAuth>>[number];
export type LoginType = Parameters<typeof loginProvider>[1];
/** pi's `AuthInteraction`: `{ signal?, prompt(p): Promise<string>, notify(e): void }`. */
export type LoginUi = Parameters<typeof loginProvider>[2];
export type CatalogModel = { provider: string; id: string; name: string; input: string[] };
/** What the sidebar shows, read by the bench from the platform (./space). JSON-safe: it crosses the websocket. */
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
  /** The snapshot the environment sits on (its message, else its id); only the connected one. */
  snapshot?: string;
};
export type TaskState = "queued" | "running" | "blocked" | "done" | "failed";
export type BoardTask = { id: string; title: string; priority: number; dependsOn: string[]; state: TaskState; note?: string; created: number };
export type SpaceView = {
  available: boolean;
  error?: string;
  user: string;
  workspaces: SpaceWorkspace[];
  environments: SpaceEnvironment[];
  /** The environment the space follows (`GET /v1/me/environments`). */
  connected?: string;
  /** Every session's own board, main first; only the local backend fills it. */
  boards?: { session: string; tasks: BoardTask[] }[];
  /** The newest messages sessions sent each other (messages.ts); only the local backend fills it. */
  messages?: Message[];
};
/** One message between sessions; `from`/`to` are base session keys. `for` is the sender's own task id, `reply` the id of the message answered. */
export type Message = { id: string; from: string; to: string; text: string; at: string; for?: string; kind?: string; reply?: string };
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
  /** Cards waiting for an answer, for a connection that arrives late. */
  asks: Ask[];
  /** The permission mode every TUI shows. */
  mode: PermMode;
};

export type PermMode = "default" | "acceptEdits" | "plan" | "bypass";
/** A pending question to the person: raised by the daemon, shown by each TUI for its active key. */
export type Ask = {
  id: string;
  /** The asking key (a delegated session's caller). */
  key: string;
  kind: "permission" | "question";
  tool: string;
  title: string;
  subtitle?: string;
  body?: string;
  diff?: FileDiff;
  options: { id: string; label: string }[];
};
/** Everything the daemon says to every connection, outside any one session. */
export type BenchEvent =
  | { type: "ask"; ask: Ask }
  | { type: "ask_resolved"; id: string }
  | { type: "perm"; mode: PermMode }
  | { type: "settings"; settings: Settings }
  | { type: "auth_changed" }
  | { type: "fs_changed"; ws?: string }
  | { type: "space"; view: SpaceView };

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

/** What the daemon holds for one live key and pushes to every view (protocol 2). */
export type SessionState = {
  type: "session_state";
  model: ModelRef;
  thinkingLevel: ThinkingLevel;
  autoCompact: boolean;
  codemode: boolean;
  queued: { steering: string[]; followUp: string[] };
  /** Assistant tokens so far; 0 after clear. */
  tokens: number;
};

export type SessionOpts = {
  /** Used only when the daemon has no live agent for the key and builds one; never applied to a live one. */
  initial?: { model?: ModelRef; thinkingLevel?: ThinkingLevel; autoCompact?: boolean; codemode?: boolean };
  fresh?: boolean;
  /** Tools the TUI owns; their `run` executes in the TUI process. */
  tools: ToolDef[];
  /** Called only for gated tools: house actions always; bash, exec and web_fetch unless the fence holds (`mustAsk`); never write, edit, patch. Absent on internal opens (a
   * view opened to deliver a reply): cards then go to every connected TUI (cards.ts). */
  permission?(req: PermissionRequest, signal: AbortSignal): Promise<Decision>;
  /** Set only by serve.ts: this view is a person at a client, so what is typed through it counts as their words (consent.ts). */
  client?: boolean;
};

type Image = Parameters<AgentSession["steer"]>[1] extends (infer I)[] | undefined ? I : never;
/** `session_closed`: the agent behind this handle was disposed (rebuilt, or idle with no views);
 * the handle is dead and the client reopens on its next action. */
export type SessionEvent = (AgentSessionEvent & { diff?: FileDiff }) | { type: "session_closed"; reopen: boolean } | SessionState;

export type SessionHandle = {
  /** Snapshot taken when the session opened; restoreTranscript reads it. */
  messages: AgentSession["messages"];
  isClaude: boolean;
  /** A turn is running right now (live: a client that reconnects mid-turn sees it). */
  readonly busy: boolean;
  /** Current snapshot; kept live by `session_state` events. */
  readonly state: SessionState;
  prompt(text: string, o?: { images?: Image[] }): Promise<void>;
  steer(text: string, images?: Image[]): Promise<void>;
  followUp(text: string, images?: Image[]): Promise<void>;
  clearQueue(): Promise<void>;
  /** Side question answered from the conversation so far; never recorded anywhere. */
  btw(question: string): Promise<string>;
  abort(): Promise<void>;
  dispose(): Promise<void>;
  setModel(ref: ModelRef): Promise<void>;
  setThinkingLevel(level: ThinkingLevel): Promise<void>;
  setAutoCompactionEnabled(on: boolean): Promise<void>;
  setCodemode(on: boolean): Promise<void>;
  subscribe(cb: (e: SessionEvent) => void): () => void;
};

export interface Backend {
  hello(): Promise<Hello>;
  /** Bench-wide events (cards, permission mode). Resolves with the unsubscribe. */
  watch(cb: (e: BenchEvent) => void): Promise<() => void>;
  asks: { answer(id: string, choice: string): Promise<void> };
  mode: { set(m: PermMode): Promise<void> };
  session(key: string, opts: SessionOpts): Promise<SessionHandle>;
  sessions: {
    list(prefix?: string): Promise<SessionMeta[]>;
    /** Pushed list with live busy: `cb` runs once before this resolves, then on every open,
     * close, turn start, turn end, name, describe and clear. Rejects on a bench without the op
     * (the caller keeps fetching `list`). Resolves with the unsubscribe. */
    watch(cb: (list: LiveSessionMeta[]) => void): Promise<() => void>;
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
