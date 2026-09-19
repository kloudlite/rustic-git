/**
 * Production seam for operation controls. O05 owns durable operation storage and is not
 * implemented here: production must explicitly provide a module factory containing the real
 * source. UI credentials are always checked online against this bench's platform identity.
 */
import {
  type OperationControlOptions,
  type OperationPrincipal,
  type OperationSource,
} from "./control.ts";

type FactoryResult = {
  operationSource: OperationSource;
};

type ProductionModule = { createOperationControl?: (context: { owner: string; team: string; bench: string }) => FactoryResult | Promise<FactoryResult> };

type BenchIdentity = { id: string; owner: string; team: string; access: string };

const BEARER = /^Bearer ([A-Za-z0-9._~+\/-]{1,8192})$/;

function identity(value: unknown): BenchIdentity | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
  const row = value as Record<string, unknown>;
  if (typeof row.id !== "string" || typeof row.owner !== "string" || typeof row.team !== "string" || typeof row.access !== "string") return undefined;
  return { id: row.id, owner: row.owner, team: row.team, access: row.access };
}

export async function loadOperationControl(options: {
  owner?: string;
  team?: string;
  bench?: string;
  api?: string;
  module?: string;
  fetch?: typeof fetch;
  importModule?: (name: string) => Promise<ProductionModule>;
  log?: (message: string) => void;
} = {}): Promise<OperationControlOptions> {
  if (!options.owner || !options.team || !options.bench || !options.api || !options.module) return {};
  const unavailable: OperationControlOptions = { unavailable: { code: "operation_source_unavailable", message: "operation source unavailable" } };
  let base: URL;
  try {
    base = new URL(options.api);
  } catch {
    options.log?.("operation control unavailable: invalid API URL");
    return unavailable;
  }
  if (base.protocol !== "https:" && base.hostname !== "127.0.0.1" && base.hostname !== "localhost") {
    options.log?.("operation control unavailable: invalid API URL");
    return unavailable;
  }
  let imported: ProductionModule;
  try {
    imported = await (options.importModule ?? ((name) => import(name) as Promise<ProductionModule>))(options.module);
  } catch {
    options.log?.("operation control unavailable: module load failed");
    return unavailable;
  }
  if (typeof imported.createOperationControl !== "function") {
    options.log?.("operation control unavailable: invalid module");
    return unavailable;
  }
  let created: FactoryResult;
  try {
    created = await imported.createOperationControl({ owner: options.owner, team: options.team, bench: options.bench });
  } catch {
    options.log?.("operation control unavailable: module initialization failed");
    return unavailable;
  }
  if (!created?.operationSource) {
    options.log?.("operation control unavailable: invalid module");
    return unavailable;
  }
  const request = options.fetch ?? fetch;
  const operationAuthorizer = async ({ authorization, owner, login }: { authorization?: string; owner?: string; login?: string }): Promise<OperationPrincipal | undefined> => {
    const match = authorization?.match(BEARER);
    if (!match || owner !== options.team || login !== options.owner) return undefined;
    let response: Response;
    try {
      const url = new URL("/v1/bench", base);
      url.searchParams.set("team", options.team!);
      response = await request(url, { headers: { authorization: authorization! }, redirect: "error", signal: AbortSignal.timeout(5000) });
    } catch {
      return undefined;
    }
    if (!response.ok) return undefined;
    const verified = identity(await response.json().catch(() => undefined));
    if (!verified || verified.id !== options.bench || verified.owner !== options.owner || verified.team !== options.team || verified.access !== "Full") return undefined;
    return { actorId: verified.owner, tenantId: verified.team, tokenKind: "person" };
  };
  return {
    operationSource: created.operationSource,
    operationAuthorizer,
  };
}
