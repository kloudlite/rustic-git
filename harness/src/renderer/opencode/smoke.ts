/**
 * harness: a compile-and-bundle smoke import. The vendored renderer has no entry point of its own
 * (opencode's `exports` map is the entry), so this file names the pieces the port will mount — if
 * an alias, a dependency or a worker URL is wrong, the build fails here rather than in the pane.
 *
 * It is imported by nothing at runtime; the adapter and the pane import these directly.
 */
export { Message, AssistantParts, Part, MessageDivider, groupParts, renderable, getToolInfo } from "@opencode-ai/session-ui/message-part";
export { SessionTurn } from "@opencode-ai/session-ui/session-turn";
export { BasicTool } from "@opencode-ai/session-ui/basic-tool";
export { ToolErrorCard } from "@opencode-ai/session-ui/tool-error-card";
export { SessionRetry } from "@opencode-ai/session-ui/session-retry";
export { DockPrompt } from "@opencode-ai/session-ui/dock-prompt";
export { SessionProgressIndicatorV2 } from "@opencode-ai/session-ui/v2/session-progress-indicator-v2";
export { applyTheme } from "@opencode-ai/ui/theme/loader";
export { resolveFileDiff, normalize } from "@opencode-ai/session-ui/session-diff";
