import { For, createMemo } from "solid-js";
import { Message as OcMessage } from "@opencode-ai/session-ui/message-part";
import { File } from "@opencode-ai/session-ui/file";
import { FileComponentProvider } from "@opencode-ai/ui/context/file";
import { DataProvider } from "@opencode-ai/session-ui/context/data";
import "@opencode-ai/session-ui/styles";
import "@opencode-ai/ui/styles";
import type { Message } from "../model";
import { partsOf, toParts } from "./adapter";

/**
 * The centre pane, drawn by opencode's own renderer (spec §23). Everything below `adapter.ts` is
 * ours; everything from here up is their code, unedited — which is what "pixel-parity by
 * construction" means: there is no second implementation to drift.
 *
 * Their `Message` takes a message and its parts directly, so the pane is a list of messages and
 * the data context is filled for the components that read it (diffs, file previews).
 */
export function OpencodePane(props: {
  messages: readonly Message[];
  session: string;
  model?: string;
  agent?: string;
  cwd?: string;
  showReasoning?: boolean;
}) {
  const bundle = createMemo(() => toParts(props.messages, { session: props.session, model: props.model, agent: props.agent, cwd: props.cwd }));
  const store = createMemo(() => ({
    session: [],
    session_status: {},
    session_diff: {},
    message: { [props.session]: bundle().messages },
    part: Object.fromEntries(bundle().messages.map((m) => [m.id, partsOf(bundle(), m.id)])),
  }));
  return (
    <DataProvider data={store() as never} directory={props.cwd ?? ""} sessionID={props.session}>
      <FileComponentProvider component={File}>
        <div data-component="session-messages" class="flex min-w-0 flex-col gap-4">
          <For each={bundle().messages}>
            {(m) => <OcMessage message={m} parts={partsOf(bundle(), m.id)} showReasoningSummaries={props.showReasoning !== false} />}
          </For>
        </div>
      </FileComponentProvider>
    </DataProvider>
  );
}
