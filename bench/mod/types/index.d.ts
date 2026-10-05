// Self-contained on purpose (the manifest's type contract forbids import/export/require): kept in
// sync by hand with the real shapes in `lib/view.ts`, which is the source of truth for structure.
export type KloudliteRow = {
  id: string;
  name: string;
  status: "idle" | "running" | "errored" | "stopped";
  doing: string;
  queued: number;
  intercepts: string[];
  agents: { id: string; label: string; status: string }[];
};
export type KloudliteEnvRow = {
  id: string;
  name: string;
  services: { name: string; ports: number[]; interceptedBy?: string }[];
};
export type KloudliteSessionAgent = { id: string; label: string; status: string; lines?: string[] };
export type KloudliteSessionWs = { lines: string[]; busy: boolean; queued: string[]; agents: KloudliteSessionAgent[] };
export type KloudliteTask = { id: string; cmd: string; state: string };

declare module "claude-code" {
  interface PluginState {
    kloudlite: {
      scroll: number;
      selected: string | null;
      envId: string | null;
      view: { workspaces: KloudliteRow[]; environment: KloudliteEnvRow | null };
      rawAgents: Record<string, KloudliteSessionWs>;
      pollError: string | null;
      tick: number;
      tasks: KloudliteTask[];
    };
  }
}
