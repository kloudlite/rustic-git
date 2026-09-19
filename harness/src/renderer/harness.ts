import type { AuthState } from "../auth/controller.ts";
import type { Team } from "../connect/bench.ts";
import type { ApiEnvironment, ApiRepo, ApiSnapshot, ApiWorkspace } from "../connect/platform.ts";
import type { OperationEvent, OperationSnapshot } from "../../bench/src/operations/contracts.ts";

export type CancelOperationPayload = { operationId: string; expectedRevision: number };
export type DecisionOperationPayload = CancelOperationPayload & { stepId: string; decisionId: string; outcome: "granted" | "denied" };
export type InputOperationPayload = CancelOperationPayload & { stepId: string; decisionId: string; inputs: { answer: string } };

export type Harness = {
  version: string;
  openPreview(url: string, label?: string): Promise<void>;
  find(text: string, next?: boolean): Promise<void>;
  pi(cmd: Record<string, unknown>, id?: string): Promise<Record<string, unknown>>;
  onPi(fn: (ev: Record<string, unknown> & { type: string; pi?: string }) => void): void;
  bench<T = unknown>(method: "GET" | "POST" | "DELETE", path: string, body?: unknown): Promise<T>;
  benchMessages(id: string): Promise<unknown[]>;
  benchBootstrap(session?: string): Promise<Record<string, unknown>>;
  benchState(): Promise<{ configured: boolean; connected: boolean; sessions: unknown[]; exchanges: unknown[] }>;
  benchImport(sessions: { id: string; name: string; seq: number; lastActive?: number; archived?: boolean }[]): Promise<{ added: string[]; files: number }>;
  providers: { list(): Promise<{ id: string; label: string; configured: boolean }[]>; save(id: string, apiKey: string): Promise<void>; remove(id: string): Promise<void> };
  operations: {
    snapshot(operationId: string): Promise<OperationSnapshot>;
    events(operationId: string, after?: string, limit?: number): Promise<{ events: OperationEvent[]; nextCursor?: string; hasMore: boolean }>;
    cancel(payload: CancelOperationPayload): Promise<OperationSnapshot>;
    decision(payload: DecisionOperationPayload): Promise<void>;
    input(payload: InputOperationPayload): Promise<OperationSnapshot>;
  };
  auth: { status(): Promise<AuthState>; signIn(): Promise<void>; cancel(): Promise<void>; openBrowser(): Promise<void>; retry(): Promise<void>; signOut(): Promise<void>; chooseTeam(slug: string): Promise<void>; teams(): Promise<Team[]>; api(): Promise<string>; setApi(url: string): Promise<void>; onState(fn: (state: AuthState) => void): void };
  platform: { workspaces(): Promise<ApiWorkspace[]>; environments(): Promise<ApiEnvironment[]>; repos(): Promise<ApiRepo[]>; environment(id: string): Promise<ApiEnvironment>; snapshots(volume: string): Promise<ApiSnapshot[]>; myEnvironment(): Promise<string | undefined>; setMyEnvironment(id: string): Promise<void>; clearMyEnvironment(): Promise<void> };
  pty: { open(id: string, scope: string, cols: number, rows: number): Promise<void>; write(id: string, data: Uint8Array): void; resize(id: string, cols: number, rows: number): void; close(id: string): void; onData(cb: (id: string, data: Uint8Array) => void): () => void; onExit(cb: (id: string, code: number | undefined, error?: string) => void): () => void; onTitle(cb: (id: string, title: string) => void): () => void };
  watch: { open(scope: string): Promise<void>; close(scope: string): void; onEvent(cb: (scope: string, ev: { path?: string; kind?: string; resync?: true }) => void): () => void };
  setTheme(mode: "system" | "light" | "dark"): Promise<void>;
  reducedMotion(): Promise<boolean>;
};
