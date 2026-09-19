import type { IncomingMessage, ServerResponse } from "node:http";
import {
  validateJsonValue,
  validateOperationEvent,
  validateOperationSnapshot,
  validateRecordedDecision,
  type JsonValue,
  type OperationEvent,
  type OperationSnapshot,
  type Validation,
} from "./contracts.ts";

export type OperationPrincipal = {
  actorId: string;
  tenantId: string;
  tokenKind: "person" | "child";
};

export type OperationAuthRequest = { authorization?: string; owner?: string; login?: string };
export type OperationAuthorizer = (request: OperationAuthRequest) => Promise<OperationPrincipal | undefined>;

export type VerifiedBenchCredential = {
  actorId: string;
  tokenKind: "person" | "child";
  teams: string[];
};

export function createBenchOperationAuthorizer(options: {
  owner: string;
  verify: (token: string) => Promise<VerifiedBenchCredential | undefined>;
}): OperationAuthorizer {
  return async ({ authorization, owner, login }) => {
    if (!authorization?.startsWith("Bearer ") || owner !== options.owner || !login) return undefined;
    const token = authorization.slice("Bearer ".length);
    if (!token || token.trim() !== token) return undefined;
    const verified = await options.verify(token);
    if (!verified || verified.actorId !== login || (owner !== verified.actorId && !verified.teams.includes(owner))) return undefined;
    return { actorId: verified.actorId, tenantId: owner, tokenKind: verified.tokenKind };
  };
}

export type OperationEventPage = {
  events: OperationEvent[];
  nextCursor?: string;
  hasMore: boolean;
};

export interface OperationSource {
  inspect(operationId: string): Promise<unknown>;
  events(operationId: string, after?: string, limit?: number): Promise<unknown>;
  cancel(operationId: string, expectedRevision: number): Promise<unknown>;
  recordDecision(operationId: string, decisionId: string, intent: DecisionIntent, principal: OperationPrincipal): Promise<unknown>;
  provideInput(operationId: string, decisionId: string, expectedRevision: number, inputs: Record<string, JsonValue>): Promise<unknown>;
}

export type DecisionIntent = { stepId: string; expectedRevision: number; outcome: "granted" | "denied" };

export type OperationControlOptions = {
  operationSource?: OperationSource;
  operationAuthorizer?: OperationAuthorizer;
  unavailable?: { code: "operation_source_unavailable"; message: string };
};

type ErrorBody = { error: { code: string; message: string; issues?: unknown; expectedRevision?: number; actualRevision?: number } };

const ID = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/;
const APPROVAL_KEY = /^(approve|approved|approval|grant|granted|decision|outcome|authorize|authorized)$/i;

function send(res: ServerResponse, status: number, value: unknown): true {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(value));
  return true;
}

function fail(res: ServerResponse, status: number, code: string, message: string, extra: Partial<ErrorBody["error"]> = {}): true {
  return send(res, status, { error: { code, message, ...extra } });
}

function validated<T>(result: Validation<T>, res: ServerResponse, source: boolean): T | undefined {
  if (result.ok) return result.value;
  fail(res, source ? 502 : 400, source ? "invalid_source_payload" : "invalid_request", source ? "operation source returned an invalid payload" : "invalid request body", { issues: result.issues });
}

function validRevision(value: unknown): value is number {
  return Number.isSafeInteger(value) && (value as number) >= 1;
}

function validInputs(value: unknown): value is Record<string, JsonValue> {
  if (typeof value !== "object" || value === null || Array.isArray(value) || Object.keys(value).length === 0) return false;
  const visit = (input: unknown): boolean => {
    if (Array.isArray(input)) return input.every(visit);
    if (typeof input !== "object" || input === null) return true;
    return Object.entries(input).every(([key, child]) => !APPROVAL_KEY.test(key) && visit(child));
  };
  return visit(value) && validateJsonValue(value).ok;
}

function validResponseOwner(snapshot: OperationSnapshot, operationId: string, who: OperationPrincipal, res: ServerResponse): boolean {
  if (snapshot.operationId === operationId && snapshot.actor.actorId === who.actorId && snapshot.actor.tenantId === who.tenantId) return true;
  fail(res, 502, "invalid_source_payload", "operation source returned another operation");
  return false;
}

function eventPage(value: unknown): Validation<OperationEventPage> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return { ok: false, issues: [{ path: "$", code: "wrong_type", message: "must be an event page" }] };
  const page = value as Record<string, unknown>;
  const keys = Object.keys(page);
  if (keys.some((key) => !["events", "nextCursor", "hasMore"].includes(key)) || !Array.isArray(page.events) || typeof page.hasMore !== "boolean" || (page.nextCursor !== undefined && (typeof page.nextCursor !== "string" || !page.nextCursor))) {
    return { ok: false, issues: [{ path: "$", code: "wrong_type", message: "must contain events, hasMore, and optional nextCursor" }] };
  }
  const events: OperationEvent[] = [];
  for (let index = 0; index < page.events.length; index++) {
    const result = validateOperationEvent(page.events[index], `$.events[${index}]`);
    if (!result.ok) return result;
    events.push(result.value);
  }
  return { ok: true, value: { events, ...(page.nextCursor ? { nextCursor: page.nextCursor as string } : {}), hasMore: page.hasMore } };
}

async function principal(req: IncomingMessage, res: ServerResponse, authorize: OperationAuthorizer): Promise<OperationPrincipal | undefined> {
  const who = await authorize({
    authorization: typeof req.headers.authorization === "string" ? req.headers.authorization : undefined,
    owner: typeof req.headers["x-kl-owner"] === "string" ? req.headers["x-kl-owner"] : undefined,
    login: typeof req.headers["x-kl-login"] === "string" ? req.headers["x-kl-login"] : undefined,
  });
  if (!who) return fail(res, 401, "unauthenticated", "authentication required"), undefined;
  if (who.tokenKind !== "person") return fail(res, 403, "child_token_refused", "operation controls require a person credential"), undefined;
  return who;
}

async function owned(source: OperationSource, operationId: string, who: OperationPrincipal, res: ServerResponse): Promise<OperationSnapshot | undefined> {
  const value = validated(validateOperationSnapshot(await source.inspect(operationId)), res, true);
  if (!value) return undefined;
  if (value.operationId !== operationId || value.actor.actorId !== who.actorId || value.actor.tenantId !== who.tenantId) {
    fail(res, 404, "operation_not_found", "operation not found");
    return undefined;
  }
  return value;
}

export async function handleOperationControl(
  req: IncomingMessage,
  res: ServerResponse,
  url: URL,
  parts: string[],
  readBody: () => Promise<Record<string, unknown>>,
  options: OperationControlOptions,
): Promise<boolean> {
  if (parts[0] !== "operations" || parts.length < 2) return false;
  const operationId = parts[1];
  if (!ID.test(operationId)) return fail(res, 400, "invalid_request", "invalid operation id");
  const method = req.method ?? "GET";
  const route = parts.length === 2 && method === "GET" ? "inspect"
    : parts.length === 3 && parts[2] === "events" && method === "GET" ? "events"
      : parts.length === 3 && parts[2] === "cancel" && method === "POST" ? "cancel"
        : parts.length === 4 && parts[2] === "decisions" && method === "POST" ? "decision"
          : parts.length === 3 && parts[2] === "input" && method === "POST" ? "input"
            : undefined;
  if (!route) return false;
  let after: string | undefined;
  let limit: number | undefined;
  if (route === "events") {
    const keys = [...url.searchParams.keys()];
    if (keys.some((key) => key !== "after" && key !== "limit") || url.searchParams.getAll("after").length > 1 || url.searchParams.getAll("limit").length > 1) {
      return fail(res, 400, "invalid_request", "events query accepts only after and limit once");
    }
    const rawAfter = url.searchParams.get("after");
    if (rawAfter !== null && rawAfter.length === 0) return fail(res, 400, "invalid_request", "after must be nonempty");
    after = rawAfter ?? undefined;
    const rawLimit = url.searchParams.get("limit");
    limit = rawLimit === null ? undefined : Number(rawLimit);
    if (limit !== undefined && (!Number.isSafeInteger(limit) || limit < 1 || limit > 200)) return fail(res, 400, "invalid_request", "limit must be an integer from 1 to 200");
  } else if (url.search.length > 0) {
    return false;
  }
  const source = options.operationSource;
  const authorize = options.operationAuthorizer;
  if (!source || !authorize) return fail(res, 503, options.unavailable?.code ?? "operation_source_unavailable", options.unavailable?.message ?? "operation source unavailable");
  const who = await principal(req, res, authorize);
  if (!who) return true;
  const snapshot = await owned(source, operationId, who, res);
  if (!snapshot) return true;
  if (route === "inspect") return send(res, 200, snapshot);
  if (route === "events") {
    const page = validated(eventPage(await source.events(operationId, after, limit)), res, true);
    if (!page || page.events.some((item) => item.operationId !== operationId)) return page ? fail(res, 502, "invalid_source_payload", "event belongs to another operation") : true;
    return send(res, 200, page);
  }
  if (route === "cancel") {
    const body = await readBody();
    if (Object.keys(body).some((key) => key !== "expectedRevision") || !validRevision(body.expectedRevision)) return fail(res, 400, "invalid_request", "expectedRevision must be a positive integer");
    const result = validated(validateOperationSnapshot(await source.cancel(operationId, body.expectedRevision)), res, true);
    return result && validResponseOwner(result, operationId, who, res) ? send(res, 200, result) : true;
  }
  if (route === "decision") {
    const decisionId = parts[3];
    const body = await readBody();
    if (!ID.test(decisionId) || Object.keys(body).some((key) => !["stepId", "expectedRevision", "outcome"].includes(key)) || typeof body.stepId !== "string" || !ID.test(body.stepId) || !validRevision(body.expectedRevision) || (body.outcome !== "granted" && body.outcome !== "denied")) return fail(res, 400, "invalid_request", "invalid decision intent");
    const result = validated(validateRecordedDecision(await source.recordDecision(operationId, decisionId, { stepId: body.stepId, expectedRevision: body.expectedRevision, outcome: body.outcome }, who)), res, true);
    if (!result) return true;
    if (result.operationId !== operationId || result.actorId !== who.actorId || result.tenantId !== who.tenantId) return fail(res, 502, "invalid_source_payload", "operation source returned another decision");
    return send(res, 200, result);
  }
  if (route === "input") {
    const body = await readBody();
    if (Object.keys(body).some((key) => !["decisionId", "expectedRevision", "inputs"].includes(key)) || typeof body.decisionId !== "string" || !ID.test(body.decisionId) || !validRevision(body.expectedRevision) || !validInputs(body.inputs)) {
      return fail(res, 400, "invalid_request", "decisionId, expectedRevision, and non-approval inputs are required");
    }
    const result = validated(validateOperationSnapshot(await source.provideInput(operationId, body.decisionId, body.expectedRevision, body.inputs)), res, true);
    return result && validResponseOwner(result, operationId, who, res) ? send(res, 200, result) : true;
  }
  return false;
}

export function operationControlError(res: ServerResponse, error: unknown): boolean {
  const value = error as Error & { code?: string; expectedRevision?: number; actualRevision?: number; status?: number };
  if (value.code === "stale_revision") return fail(res, 409, value.code, value.message, { expectedRevision: value.expectedRevision, actualRevision: value.actualRevision });
  if (value.code === "not_found") return fail(res, 404, "operation_not_found", value.message);
  if (value.code === "forbidden") return fail(res, 403, "forbidden", value.message);
  return false;
}
