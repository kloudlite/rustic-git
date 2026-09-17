import { For, Show } from "solid-js";
import { DockPrompt } from "@opencode-ai/session-ui/dock-prompt";
import { Button } from "@opencode-ai/ui/button";
import { Icon } from "@opencode-ai/ui/icon";
import "@opencode-ai/ui/dock-surface.css";

/**
 * A tool waiting to run, as opencode renders a permission
 * (`packages/app/src/pages/session/composer/session-permission-dock.tsx`): their `DockPrompt` above
 * the composer, a warning icon and a title, the request itself, and deny / always / once in their
 * own buttons. Ours answers `POST /proposals/:id` instead of their SDK — that call is the only
 * thing this file adds to theirs.
 *
 * "Always" is our accept-edits mode for this tool: the bench answers this one yes and stops asking
 * for that tool in this session.
 */
export function PermissionDock(props: {
  summary: string;
  tool: string;
  patterns?: string[];
  responding?: boolean;
  onDecide: (answer: "once" | "always" | "reject") => void;
}) {
  return (
    <DockPrompt
      kind="permission"
      header={
        <div data-slot="permission-row" data-variant="header">
          <span data-slot="permission-icon">
            <Icon name="warning" size="normal" />
          </span>
          <div data-slot="permission-header-title">Permission required</div>
        </div>
      }
      footer={
        <>
          <div />
          <div data-slot="permission-footer-actions">
            <Button variant="ghost" size="normal" onClick={() => props.onDecide("reject")} disabled={props.responding}>
              Deny
            </Button>
            <Button variant="secondary" size="normal" onClick={() => props.onDecide("always")} disabled={props.responding}>
              Allow always
            </Button>
            <Button variant="primary" size="normal" onClick={() => props.onDecide("once")} disabled={props.responding}>
              Allow once
            </Button>
          </div>
        </>
      }
    >
      <div data-slot="permission-row">
        <span data-slot="permission-spacer" aria-hidden="true" />
        <div data-slot="permission-hint">{props.summary}</div>
      </div>
      <Show when={props.patterns?.length}>
        <div data-slot="permission-row">
          <span data-slot="permission-spacer" aria-hidden="true" />
          <div data-slot="permission-patterns">
            <For each={props.patterns}>{(pattern) => <code class="break-all">{pattern}</code>}</For>
          </div>
        </div>
      </Show>
    </DockPrompt>
  );
}
