/**
 * Every tool the bench can call, described once: the extension registers from
 * this list and the Settings › Tools page renders it, so what the model sees
 * and what a person reads are the same catalogue. A bench session has no hands
 * in its own pod — no builtin tools, no shell — so nothing here is local:
 * `kl_ws_*` runs in a workspace, `kl_*` is /v1.
 */
export type ToolSpec = {
  name: string;
  group: "workspace" | "environment" | "platform";
  summary: string;
  /** What the call does to the platform; a person decides on this. */
  effect: "read" | "write" | "destroy";
};

export const TOOLS: ToolSpec[] = [
  { name: "kl_ws_read", group: "workspace", summary: "Read a file in a workspace, by workspace id.", effect: "read" },
  { name: "kl_ws_write", group: "workspace", summary: "Create or overwrite a file in a workspace.", effect: "write" },
  { name: "kl_ws_edit", group: "workspace", summary: "Exact replacements in one file in a workspace, all or nothing.", effect: "write" },
  { name: "kl_ws_exec", group: "workspace", summary: "Run a shell command inside a workspace and return its output.", effect: "write" },
  { name: "kl_ws_grep", group: "workspace", summary: "Regex search inside a workspace, gitignore-aware.", effect: "read" },
  { name: "kl_ws_glob", group: "workspace", summary: "Files matching a glob inside a workspace, newest first.", effect: "read" },
  { name: "kl_ws_ls", group: "workspace", summary: "List a directory inside a workspace.", effect: "read" },

  { name: "kl_workspaces", group: "workspace", summary: "List workspaces — yours, or a team's with `team`.", effect: "read" },
  { name: "kl_workspace", group: "workspace", summary: "One workspace in full: state, node, packages, its space's environment.", effect: "read" },
  { name: "kl_workspace_create", group: "workspace", summary: "Create a workspace in a region, optionally from a repo and branch.", effect: "write" },
  { name: "kl_workspace_start", group: "workspace", summary: "Start a stopped workspace.", effect: "write" },
  { name: "kl_workspace_stop", group: "workspace", summary: "Stop a running workspace (cuts a sync point first).", effect: "write" },
  { name: "kl_workspace_push", group: "workspace", summary: "Take a snapshot of the workspace, with a message.", effect: "write" },
  { name: "kl_workspace_clone", group: "workspace", summary: "Clone a workspace into a new one from its latest sync point.", effect: "write" },
  { name: "kl_workspace_packages", group: "workspace", summary: "Set the workspace's package list (`attr` or `attr@version`).", effect: "write" },
  { name: "kl_workspace_packages_update", group: "workspace", summary: "Re-resolve the workspace's pinned packages to their newest allowed versions.", effect: "write" },
  { name: "kl_workspace_restore", group: "workspace", summary: "Restore a snapshot into a new workspace.", effect: "write" },
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
  { name: "kl_environment_services", group: "environment", summary: "Change an environment's services in place (the whole list); removals delete their workloads.", effect: "write" },
  { name: "kl_environment_restore", group: "environment", summary: "Restore a snapshot into a new environment.", effect: "write" },
  { name: "kl_environment_restore_in_place", group: "environment", summary: "Restore a snapshot back over an existing environment.", effect: "write" },
  { name: "kl_environment_delete", group: "environment", summary: "Delete an environment.", effect: "destroy" },

  { name: "kl_regions", group: "platform", summary: "Regions available to place work in.", effect: "read" },
  { name: "kl_quota", group: "platform", summary: "Your quota and what of it is in use.", effect: "read" },
  { name: "kl_volumes", group: "platform", summary: "Volumes and their snapshots (`history` of one by name).", effect: "read" },
  { name: "kl_builder", group: "platform", summary: "Your image builder: state and why it is not ready.", effect: "read" },
  { name: "kl_volume_history", group: "platform", summary: "A volume's snapshots, newest first.", effect: "read" },
  { name: "kl_volume_delete", group: "platform", summary: "Delete a detached volume with all its snapshots.", effect: "destroy" },
  { name: "kl_requests", group: "platform", summary: "Requests you have open (quota, access, region, other).", effect: "read" },
  { name: "kl_request_create", group: "platform", summary: "Open a request for something that has to be granted.", effect: "write" },
  { name: "kl_whoami", group: "platform", summary: "Who the bench acts as on the platform, and until when.", effect: "read" },
];
