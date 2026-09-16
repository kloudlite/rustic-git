import { test } from "node:test";
import assert from "node:assert/strict";
import { shouldRefreshOn } from "../../src/renderer/refresh.ts";

test("only a finished kloudlite/workspace/environment tool call asks for a refresh", () => {
  const table: [Record<string, unknown>, boolean][] = [
    [{ type: "tool_execution_end", toolName: "kl_workspace_create" }, true],
    [{ type: "tool_execution_end", toolName: "workspace_start" }, true],
    [{ type: "tool_execution_end", toolName: "environment_attach" }, true],
    [{ type: "tool_execution_end", toolName: "bash" }, false],
    [{ type: "tool_execution_end", toolName: "read" }, false],
    [{ type: "tool_execution_start", toolName: "kl_workspaces" }, false],
    [{ type: "tool_execution_end" }, false],
    [{ type: "text_delta", toolName: "kl_x" }, false],
    [{}, false],
  ];
  for (const [ev, want] of table) assert.equal(shouldRefreshOn(ev), want, JSON.stringify(ev));
});
