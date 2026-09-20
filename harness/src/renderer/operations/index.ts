/**
 * O09 — the fixture-driven operation UI slice.
 *
 * Exports, in one place, what the wiring needs:
 *
 * - the projection: `createOperationView`, `applyOperationSnapshot`, `applyOperationEvent(s)`,
 *   `markDisconnected`, `markReconnected`, `markCancelRequested`, `markDecisionPresented`;
 * - the reading: every `*Rows` selector in `./present.ts`, plus `OperationPanel`;
 * - the bridge: callback payload types and builders in `./bridge.ts`, whose only job is to
 *   hand the trusted bridge an intent (never a grant record).
 *
 * The scenario fixtures are NOT re-exported here: they are test-only content (1168 lines of
 * scenario data) and every test imports them directly from their own module path, so
 * re-exporting them from here shipped them in the production bundle for nothing (review M1).
 *
 * Live O05/O08 wiring is not part of this slice: `OperationUiBridge` documents the hooks, and
 * `docs/superpowers/plans/2026-09-18-operation-ui-handoff.md` records the remaining gates.
 */
export * from "./types.ts";
import "./styles/operations.css";
export * from "./reduce.ts";
export * from "./present.ts";
export * from "./bridge.ts";
export * from "./store.ts";
export { OperationPanel, type OperationPanelProps } from "./components/OperationPanel.tsx";
export { useOperationClock } from "./clock.ts";
