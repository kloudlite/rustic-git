import type { CapabilityAdapter, CapabilityRuntime } from "../src/operations/capabilities.ts";

const RUNTIME = Symbol.for("kloudlite.operations.capability-runtime");

export function capabilityRuntime(adapters: Readonly<Record<string, CapabilityAdapter>>): CapabilityRuntime {
  const registered = new Map(Object.entries(adapters));
  return Object.freeze({
    [RUNTIME]: true as const,
    resolve: (capability: string) => registered.get(capability),
  }) as CapabilityRuntime;
}
