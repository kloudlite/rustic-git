/**
 * O09 — the fixture-driven operation UI slice.
 *
 * Exports, in one place, what the wiring needs:
 *
 * - the projection: `createOperationView`, `applyOperationSnapshot`, `applyOperationEvent(s)`,
 *   `markDisconnected`, `markReconnected`, `markCancelRequested`, `markDecisionPresented`;
 * - the reading: every `*Rows` selector in `./present.ts`, plus `OperationPanel`;
 * - the bridge: callback payload types and builders in `./bridge.ts`, whose only job is to
 *   hand the trusted bridge an intent (never a grant record);
 * - the fixtures in `./fixtures/scenarios.ts`, which the test replays against O01-valid
 *   snapshots.
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
export {
  SCENARIOS,
  applyScenarioRepair,
  applyScriptEntry,
  openScenario,
  scenarioById,
  type FixtureAction,
  type FixtureScriptEntry,
  type OperationScenario,
  type ScenarioExpectation,
} from "./fixtures/scenarios.ts";
export { OperationPanel, type OperationPanelProps } from "./components/OperationPanel.tsx";
