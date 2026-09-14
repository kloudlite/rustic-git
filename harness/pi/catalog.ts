/**
 * Every tool the bench can call, described once: the extension registers from
 * this list and the Settings › Tools page renders it, so what the model sees
 * and what a person reads are the same catalogue.
 */
export type ToolSpec = {
  name: string;
  group: "workspace" | "environment" | "platform" | "shell";
  summary: string;
  /** What the call does to the platform; a person decides on this. */
  effect: "read" | "write" | "destroy";
  builtin?: boolean;
};

export const TOOLS: ToolSpec[] = [
  { name: "bash", group: "shell", summary: "Run a shell command in the bench's working directory (can be sent to the background).", effect: "write", builtin: true },
  { name: "read", group: "shell", summary: "Read a file.", effect: "read", builtin: true },
  { name: "write", group: "shell", summary: "Write a file.", effect: "write", builtin: true },
  { name: "edit", group: "shell", summary: "Replace an exact block in a file.", effect: "write", builtin: true },
  { name: "grep", group: "shell", summary: "Search file contents.", effect: "read", builtin: true },
  { name: "find", group: "shell", summary: "Find files by name.", effect: "read", builtin: true },
  { name: "ls", group: "shell", summary: "List a directory.", effect: "read", builtin: true },
  { name: "process", group: "shell", summary: "Start, watch, and stop long-running processes (dev servers, watchers) with their logs.", effect: "write" },

  { name: "kl_workspaces", group: "workspace", summary: "List workspaces — yours, or a team's with `team`.", effect: "read" },
  { name: "kl_workspace", group: "workspace", summary: "One workspace in full: state, node, packages, its space's environment.", effect: "read" },
  { name: "kl_workspace_create", group: "workspace", summary: "Create a workspace in a region, optionally from a repo and branch.", effect: "write" },
  { name: "kl_workspace_start", group: "workspace", summary: "Start a stopped workspace.", effect: "write" },
  { name: "kl_workspace_stop", group: "workspace", summary: "Stop a running workspace (cuts a sync point first).", effect: "write" },
  { name: "kl_workspace_push", group: "workspace", summary: "Take a snapshot of the workspace, with a message.", effect: "write" },
  { name: "kl_workspace_clone", group: "workspace", summary: "Clone a workspace into a new one from its latest sync point.", effect: "write" },
  { name: "kl_workspace_packages", group: "workspace", summary: "Set the workspace's package list (`attr` or `attr@version`).", effect: "write" },
  { name: "kl_workspace_delete", group: "workspace", summary: "Delete a workspace; its snapshots survive on the volume.", effect: "destroy" },

  { name: "kl_my_environment", group: "environment", summary: "Which environment each of your spaces (per team, plus personal) uses.", effect: "read" },
  { name: "kl_my_environment_set", group: "environment", summary: "Use an environment for every workspace and the bench in one of your spaces.", effect: "write" },
  { name: "kl_my_environment_clear", group: "environment", summary: "Stop using an environment in one of your spaces.", effect: "write" },
  { name: "kl_environments", group: "environment", summary: "List environments you can see.", effect: "read" },
  { name: "kl_environment", group: "environment", summary: "One environment in full: services, ports, intercepts.", effect: "read" },
  { name: "kl_environment_create", group: "environment", summary: "Create an environment from a services list.", effect: "write" },
  { name: "kl_environment_start", group: "environment", summary: "Start a stopped environment.", effect: "write" },
  { name: "kl_environment_stop", group: "environment", summary: "Stop an environment.", effect: "write" },
  { name: "kl_environment_push", group: "environment", summary: "Snapshot an environment.", effect: "write" },
  { name: "kl_environment_clone", group: "environment", summary: "Clone an environment into a new one.", effect: "write" },
  { name: "kl_intercept", group: "environment", summary: "Deliver a service's traffic to a workspace (with port remap), or clear it.", effect: "write" },
  { name: "kl_environment_delete", group: "environment", summary: "Delete an environment.", effect: "destroy" },

  { name: "kl_regions", group: "platform", summary: "Regions available to place work in.", effect: "read" },
  { name: "kl_quota", group: "platform", summary: "Your quota and what of it is in use.", effect: "read" },
  { name: "kl_volumes", group: "platform", summary: "Volumes and their snapshots (`history` of one by name).", effect: "read" },
  { name: "kl_builder", group: "platform", summary: "Your image builder: state and why it is not ready.", effect: "read" },
  { name: "kl_whoami", group: "platform", summary: "Who the bench acts as on the platform, and until when.", effect: "read" },
];
