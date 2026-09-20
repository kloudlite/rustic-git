import { createSignal, type Accessor, type Setter } from "solid-js";
import { validateCompactOperationResult, type OperationEvent, type OperationSnapshot, type OperationState } from "../../../bench/src/operations/contracts.ts";
import type { OperationRendererBridge } from "./bridge.ts";
import {
  applyOperationEvents,
  applyOperationSnapshot,
  createOperationView,
  markDisconnected,
  markReconnected,
} from "./reduce.ts";
import { requestLabel, type OperationView, type ResyncRequest } from "./types.ts";

export type OperationProjection = {
  operationId: string;
  sessionId: string;
  workspaceId?: string;
  view: Accessor<OperationView>;
  resync(request?: ResyncRequest): Promise<void>;
  reload(): Promise<void>;
  dispose(): void;
  controls: Pick<OperationRendererBridge, "decide" | "answer" | "cancel">;
  error: Accessor<OperationLoadError | undefined>;
};

export type OperationLoadError = { kind: "snapshot" | "events"; code: string; message: string; retryable: true; status?: number; details?: Record<string, unknown> };

export class OperationStoreError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(message);
    this.code = code;
    this.name = "OperationStoreError";
  }
}

type BenchErrorShape = Error & { status: number; code?: string; details?: Record<string, unknown> };

const isBenchResponseError = (cause: unknown): cause is BenchErrorShape =>
  cause instanceof Error && cause.name === "BenchResponseError" && typeof (cause as Partial<BenchErrorShape>).status === "number";

const loadError = (kind: OperationLoadError["kind"], fallback: string, cause: unknown): OperationLoadError => isBenchResponseError(cause)
  ? { kind, code: cause.code ?? fallback, message: cause.message, retryable: true, status: cause.status, ...(cause.details && Object.keys(cause.details).length ? { details: cause.details } : {}) }
  : { kind, code: cause instanceof OperationStoreError ? cause.code : fallback, message: cause instanceof Error ? cause.message : String(cause), retryable: true };

export type OperationTaskState = "running" | "waiting" | "reconciling" | "partial" | "failed" | "cancelled" | "completed";

export type OperationTaskRow = {
  id: string;
  kind: "operation";
  title: string;
  state: OperationTaskState;
  started: number;
  ended?: number;
  projection: OperationProjection;
};

const taskState = (state: OperationState | undefined): OperationTaskState => {
  if (state === "awaiting_approval" || state === "needs_input") return "waiting";
  if (state === "reconciling") return "reconciling";
  if (state === "partial") return "partial";
  if (state === "failed" || state === "expired") return "failed";
  if (state === "cancelled") return "cancelled";
  if (state === "completed") return "completed";
  return "running";
};

export function operationTaskRow(view: OperationView, projection?: OperationProjection): OperationTaskRow {
  const state = taskState(view.state);
  return {
    id: view.operationId,
    kind: "operation",
    title: requestLabel(view) ?? view.operationId,
    state,
    started: view.createdAt ?? 0,
    ended: ["partial", "failed", "cancelled", "completed"].includes(state) ? view.updatedAt : undefined,
    projection: projection ?? { operationId: view.operationId, sessionId: "", view: () => view, resync: async () => undefined, reload: async () => undefined, dispose: () => undefined, controls: {}, error: () => undefined },
  };
}

export function operationIdFromAction(action: { tool?: string; output?: string; args?: Record<string, unknown> }): string | undefined {
  if (action.tool !== "operate") return undefined;
  try {
    const result = validateCompactOperationResult(JSON.parse(action.output ?? ""));
    if (result.ok) return result.value.operationId;
  } catch {
    return undefined;
  }
  return undefined;
}

export async function collectOperationEventPages(
  load: (after: string | undefined) => Promise<{ events: OperationEvent[]; nextCursor?: string; hasMore: boolean }>,
  afterSequence: number,
): Promise<OperationEvent[]> {
  const events: OperationEvent[] = [];
  let cursor: string | undefined = String(afterSequence);
  const seen = new Set<string>();
  while (cursor !== undefined) {
    if (seen.has(cursor)) throw new OperationStoreError("operation_cursor_stalled", "operation event cursor made no progress");
    seen.add(cursor);
    const page = await load(cursor);
    events.push(...page.events);
    if (!page.hasMore) break;
    if (!page.nextCursor || page.nextCursor === cursor || seen.has(page.nextCursor)) throw new OperationStoreError("operation_cursor_stalled", "operation event cursor made no progress");
    cursor = page.nextCursor;
  }
  return events;
}

export function createOperationStore(bridge: OperationRendererBridge) {
  const projections = new Map<string, OperationProjection>();
  const setters = new Map<string, Setter<OperationView>>();
  const releases = new Map<string, () => void>();
  const catchUps = new Map<string, (lastSequence?: number, snapshot?: boolean) => Promise<void> | undefined>();
  const reconnects = new Map<string, () => void>();
  // How many open views share one projection. `open()` increments; a view's own `dispose()`
  // only really releases the projection at zero. `disposeSession`/`dispose` bypass this and
  // force the release, because a gone session has nothing left to share.
  const refCounts = new Map<string, number>();
  const forceReleases = new Map<string, () => void>();
  const [entries, setEntries] = createSignal<OperationProjection[]>([]);
  let connected = true;
  let disposed = false;

  const connection = (next: boolean) => {
    if (next === connected || disposed) return;
    connected = next;
    const at = Date.now();
    for (const projection of projections.values()) {
      const current = projection.view();
      const updated = next ? markReconnected(current, at) : markDisconnected(current, at);
      setters.get(projection.operationId)?.(updated);
      if (next) reconnects.get(projection.operationId)?.();
    }
  };

  const open = (operationId: string, owner: { sessionId?: string; workspaceId?: string } = {}): OperationProjection => {
    const held = projections.get(operationId);
    if (held) {
      // An empty `sessionId` is "not resolved yet", never a real owner: a ToolCall that opened
      // before its thread resolved must not be read as conflicting with the first caller that
      // supplies the real one.
      if ((owner.sessionId !== undefined && held.sessionId !== "" && held.sessionId !== owner.sessionId) || (owner.workspaceId !== undefined && held.workspaceId !== undefined && held.workspaceId !== owner.workspaceId)) {
        const mismatch: OperationLoadError = { kind: "snapshot", code: "operation_owner_mismatch", message: "operation owner mismatch", retryable: true };
        // A mismatched view is read-only: no `resync`/`reload`/`controls`, so Approve, Cancel and
        // the reconnect repair banner cannot act on a projection this caller does not own.
        return { ...held, resync: async () => undefined, reload: async () => undefined, dispose: () => undefined, controls: {}, error: () => mismatch };
      }
      // A second ToolCall (or the inspector's task row) showing the same operation shares this
      // projection rather than opening a duplicate fetch; each caller's own `dispose()` only
      // counts down, so the first one to unmount does not pull the projection out from under
      // whichever other view still shows it.
      refCounts.set(operationId, (refCounts.get(operationId) ?? 1) + 1);
      // A ToolCall can mount before its thread resolves a real session id (`open(id, { sessionId:
      // "" })`); once a caller supplies the real one, fill it in so `disposeSession` can find this
      // projection under the session that actually owns it, rather than leaving it stranded on
      // the empty placeholder forever.
      if (held.sessionId === "" && owner.sessionId) held.sessionId = owner.sessionId;
      if (held.workspaceId === undefined && owner.workspaceId !== undefined) held.workspaceId = owner.workspaceId;
      return held;
    }
    const [view, setView] = createSignal(createOperationView(operationId, Date.now()));
    const [error, setError] = createSignal<OperationLoadError>();
    let loading: Promise<void> | undefined;
    let announcedTarget = 0;
    let generation = 0;
    let released = false;
    let reconnectPending = false;
    let pendingNotification = false;

    const loadEvents = async (after: number, currentGeneration = generation) => {
      try {
        const events = await bridge.loadEvents(operationId, after);
        if (!disposed && !released && currentGeneration === generation) {
          setError(undefined);
          setView((current) => applyOperationEvents(current, events));
        }
      } catch (cause) {
        if (!disposed && !released && currentGeneration === generation) setError(loadError("events", "event_load_failed", cause));
      }
    };
    const loadSnapshot = async (currentGeneration = generation) => {
      let snapshot: OperationSnapshot;
      try {
        snapshot = await bridge.loadSnapshot(operationId);
      } catch (cause) {
        if (!disposed && !released && currentGeneration === generation) setError(loadError("snapshot", "snapshot_load_failed", cause));
        return;
      }
      if (disposed || released || currentGeneration !== generation) return;
      if (snapshot.operationId !== operationId || (owner.sessionId !== undefined && snapshot.actor.sessionId !== owner.sessionId)) {
        setError({ kind: "snapshot", code: "snapshot_owner_mismatch", message: "operation snapshot owner mismatch", retryable: true });
        return;
      }
      setError(undefined);
      setView((current) => applyOperationSnapshot(current, snapshot));
      await loadEvents(snapshot.lastSequence, currentGeneration);
    };
    const resync = async (request = view().resync) => {
      if (!request || disposed) return;
      if (request.need === "snapshot") await loadSnapshot();
      else await loadEvents(request.afterSequence);
    };
    const catchUp = async (lastSequence?: number, snapshot = false) => {
      if (disposed || released || (lastSequence !== undefined && lastSequence <= view().lastSequence)) return loading;
      if (lastSequence !== undefined) announcedTarget = Math.max(announcedTarget, lastSequence);
      if (loading) {
        if (lastSequence !== undefined) pendingNotification = true;
        return loading;
      }
      const currentGeneration = generation;
      loading = (snapshot ? loadSnapshot(currentGeneration) : loadEvents(view().lastSequence, currentGeneration)).finally(() => {
        loading = undefined;
        if (reconnectPending) {
          reconnectPending = false;
          if (!disposed && !released) void catchUp();
        } else if (!disposed && !released && pendingNotification) {
          pendingNotification = false;
          const target = announcedTarget;
          announcedTarget = 0;
          void catchUp(target > view().lastSequence ? target : undefined);
        }
      });
      return loading;
    };

    const release = () => {
      if (released) return;
      released = true;
      generation += 1;
      refCounts.delete(operationId);
      forceReleases.delete(operationId);
      releases.get(operationId)?.();
      releases.delete(operationId);
      catchUps.delete(operationId);
      reconnects.delete(operationId);
      projections.delete(operationId);
      setters.delete(operationId);
      setEntries([...projections.values()]);
    };
    forceReleases.set(operationId, release);
    // A view's own `dispose()` (ToolCall's `onCleanup`, or a test that opened one directly) only
    // counts itself out; the projection is actually released once nothing still holds it.
    const releaseOne = () => {
      const left = (refCounts.get(operationId) ?? 1) - 1;
      if (left <= 0) release();
      else refCounts.set(operationId, left);
    };
    const projection = { operationId, sessionId: owner.sessionId ?? "", workspaceId: owner.workspaceId, view, resync, reload: () => catchUp(undefined, true) ?? Promise.resolve(), dispose: releaseOne, controls: { decide: bridge.decide, answer: bridge.answer, cancel: bridge.cancel }, error };
    projections.set(operationId, projection);
    refCounts.set(operationId, 1);
    catchUps.set(operationId, catchUp);
    reconnects.set(operationId, () => {
      if (loading) {
        reconnectPending = true;
      }
      else void catchUp();
    });
    setters.set(operationId, setView);
    setEntries([...projections.values()]);
    void catchUp(undefined, true).then(() => {
      if (disposed || released) return;
      const release = bridge.watch?.(operationId, (lastSequence) => { void catchUp(lastSequence); });
      if (release) releases.set(operationId, release);
    });
    return projection;
  };

  const releaseConnection = bridge.onConnection?.(connection);
  let releaseConnectionOnce = releaseConnection;

  const taskRows = (sessionId: string, workspaceId?: string) => [...projections.values()]
    .filter((projection) => projection.sessionId === sessionId && (projection.workspaceId === undefined || projection.workspaceId === workspaceId))
    .map((projection) => operationTaskRow(projection.view(), projection));

  // A deleted or archived session's projections go regardless of how many views still had them
  // open — those views are unmounting anyway — so this calls the real release, not the
  // refcounted one a view's own `dispose()` uses.
  const disposeSession = (sessionId: string) => {
    for (const projection of [...projections.values()]) if (projection.sessionId === sessionId) forceReleases.get(projection.operationId)?.();
  };

  const archiveSession = disposeSession;

  const dispose = () => {
    disposed = true;
    for (const release of releases.values()) release();
    releases.clear();
    releaseConnectionOnce?.();
    releaseConnectionOnce = undefined;
    projections.clear();
    setters.clear();
    setEntries([]);
  };

  return { open, entries, taskRows, connection, archiveSession, disposeSession, dispose };
}

export type OperationStore = ReturnType<typeof createOperationStore>;

let rendererStore: OperationStore | undefined;

/** Install the session-scoped renderer store. App owns disposal when its live session ends. */
export function setOperationStore(store: OperationStore | undefined): void {
  rendererStore = store;
}

export function operationStore(): OperationStore | undefined {
  return rendererStore;
}

/** Install the transport when main/preload grows operation methods; absent means no live operation UI. */
export function configureOperationBridge(bridge: OperationRendererBridge | undefined): OperationStore | undefined {
  rendererStore?.dispose();
  rendererStore = bridge ? createOperationStore(bridge) : undefined;
  return rendererStore;
}
