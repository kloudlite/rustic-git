import { Show } from "solid-js";
import { pickRenderer } from "./pick";
import { WorkspaceCard } from "./WorkspaceCard";
import { EnvironmentCard } from "./EnvironmentCard";
import { QuotaCard } from "./QuotaCard";
import { HistoryRows } from "./HistoryRows";
import { AskChip } from "./AskChip";
import { ProcessRows } from "./ProcessRows";
import { PackagesList } from "./PackagesList";
import { CapabilitiesList } from "./CapabilitiesList";

export { pickRenderer } from "./pick";

/**
 * A tool's answer, drawn. `undefined` when nothing here fits, and the caller keeps the block it
 * had — a route this does not know yet must degrade to readable JSON, never to a blank panel.
 */
export function ResultCard(props: { tool?: string; output?: string; args?: Record<string, unknown> }) {
  const card = () => pickRenderer(props.tool, props.output, props.args);
  return (
    <Show when={card()}>
      {(c) => {
        const k = c();
        switch (k.kind) {
          case "workspace": return <WorkspaceCard data={k.data} />;
          case "environment": return <EnvironmentCard data={k.data} />;
          case "quota": return <QuotaCard data={k.data} />;
          case "history": return <HistoryRows data={k.data} />;
          case "ask": return <AskChip workspace={k.data.workspace} />;
          case "processes": return <ProcessRows data={k.data} />;
          case "packages": return <PackagesList data={k.data} />;
          case "capabilities": return <CapabilitiesList data={k.data} />;
        }
      }}
    </Show>
  );
}
