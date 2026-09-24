// Moved to bench/src/operations/catalog.ts (the bench must not import anything under harness/pi);
// this file re-exports so the electron side and harness/pi's own tests keep compiling unchanged.
export * from "../bench/src/operations/catalog.ts";
