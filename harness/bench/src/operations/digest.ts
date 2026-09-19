import type { JsonValue, OperateRequest } from "./contracts.ts";
import { canonicalDigest } from "./contracts.ts";

export { canonicalDigest };

export function requestDigest(request: OperateRequest): string {
  return canonicalDigest(request as unknown as JsonValue);
}
