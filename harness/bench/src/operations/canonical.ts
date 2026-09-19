import { Hash } from "fast-sha256";
import type { JsonValue } from "./shape.ts";

/** Deterministic JSON with sorted keys; string contents are preserved exactly. */
export function stableStringify(value: JsonValue): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(stableStringify).join(",")}]`;
  const keys = Object.keys(value).sort();
  return `{${keys.map((key) => `${JSON.stringify(key)}:${stableStringify(value[key])}`).join(",")}}`;
}

/** sha256 over the canonical form; key order and formatting never change the digest. */
export function canonicalDigest(value: JsonValue): string {
  const bytes = new Hash().update(new TextEncoder().encode(stableStringify(value))).digest();
  return `sha256:${Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("")}`;
}
