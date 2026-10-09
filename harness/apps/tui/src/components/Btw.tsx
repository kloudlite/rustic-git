import { TextAttributes } from "@opentui/core";
import { theme } from "../theme.ts";
import { clip } from "../ui/text.ts";
import { Md } from "./Transcript.tsx";

export type BtwState = { q: string; answer?: string; error?: string };

/**
 * A `/btw` side question and its one answer, above the prompt like the queue. Never part of the
 * transcript: it lives only in app state and Esc drops it.
 */
export function Btw({ state, width }: { state: BtwState; width: number }) {
  return (
    <box flexDirection="column" flexShrink={0} marginBottom={1}>
      <box flexDirection="row" justifyContent="space-between" height={1} paddingLeft={1} paddingRight={1}>
        <text selectable={false}>
          <span fg={theme.accent} attributes={TextAttributes.BOLD}>
            {clip(`btw: ${state.q}`, Math.max(10, width - 12))}
          </span>
        </text>
        <text selectable={false} fg={theme.muted}>
          esc close
        </text>
      </box>
      <box paddingLeft={1} paddingRight={1}>
        {state.error !== undefined ? (
          <text fg={theme.error}>{state.error}</text>
        ) : state.answer === undefined ? (
          <text fg={theme.muted}>thinking…</text>
        ) : (
          <Md text={state.answer} />
        )}
      </box>
    </box>
  );
}
