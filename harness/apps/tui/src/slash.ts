type SlashCommand = {
  name: string;
  description: string;
  /** Command takes an argument — typing "/cmd " opens its option list. */
  hasOptions?: boolean;
};

export const commands: SlashCommand[] = [
  { name: "/help", description: "Keyboard shortcuts and commands" },
  { name: "/tools", description: "List registered tools" },
  { name: "/clear", description: "Clear the transcript" },
  { name: "/session", description: "Name or continue a main session", hasOptions: true },
  { name: "/model", description: "Set this session's model", hasOptions: true },
  { name: "/theme", description: "Switch theme", hasOptions: true },
  { name: "/login", description: "Log in to a provider", hasOptions: true },
  { name: "/files", description: "Browse workspace files and diffs" },
  { name: "/settings", description: "Adjust settings", hasOptions: true },
  { name: "/exit", description: "Quit" },
];

/** One row in the slash menu: `insert` is submitted, `label`/`hint` rendered. */
export type MenuItem = { insert: string; label: string; hint: string };

type MenuContext = {
  models: { provider: string; id: string; hint: string }[];
  themes: string[];
  logins: { provider: string; type: string; label: string }[];
  /** flat option rows for /settings, hint marks the current value */
  settings: { key: string; value: string; hint: string }[];
  /** this environment's main sessions, newest first */
  sessions: { id: string; label: string; hint: string }[];
};

export const MENU_MAX = 8;

/** Menu for the current input: command list, or the typed command's options. */
function loginHint(type: string, prefix: string): string {
  return prefix + (type === "oauth" ? "oauth" : type === "claude_code" ? "claude code" : "api key");
}

export function menuItems(input: string, ctx: MenuContext): MenuItem[] {
  if (!input.startsWith("/")) return [];

  const space = input.indexOf(" ");
  if (space === -1) {
    return commands
      .filter((c) => c.name.startsWith(input))
      .map((c) => ({
        insert: c.hasOptions ? `${c.name} ` : c.name,
        label: c.name,
        hint: c.description,
      }));
  }

  const cmd = input.slice(0, space);
  const q = input.slice(space + 1).toLowerCase();
  const filter = (label: string) => label.toLowerCase().includes(q);

  if (cmd === "/model") {
    // Nothing is connected, so there is nothing to pick — offer the logins
    // here rather than an empty menu. Filtering models to connected providers
    // otherwise dead-ends a first run: no models to choose and no sign that
    // /login is what's missing.
    if (ctx.models.length === 0)
      return ctx.logins
        .filter((l) => filter(`${l.provider} ${l.label}`))
        .map((l) => ({
          insert: `/login ${l.provider} ${l.type}`,
          label: `${l.provider} · ${l.label}`,
          hint: loginHint(l.type, "connect · "),
        }));
    return ctx.models
      .filter((m) => filter(`${m.provider}/${m.id}`))
      .map((m) => ({
        insert: `/model ${m.provider}/${m.id}`,
        label: `${m.provider}/${m.id}`,
        hint: m.hint,
      }));
  }
  if (cmd === "/theme")
    return ctx.themes.filter(filter).map((name) => ({
      insert: `/theme ${name}`,
      label: name,
      hint: "theme",
    }));
  if (cmd === "/login")
    return ctx.logins
      .filter((l) => filter(`${l.provider} ${l.label}`))
      .map((l) => ({
        insert: `/login ${l.provider} ${l.type}`,
        label: `${l.provider} · ${l.label}`,
        hint: loginHint(l.type, ""),
      }));
  if (cmd === "/session") {
    const rest = input.slice(space + 1);
    // "/session name <text>" is free text — no options to pick from
    if (/^name\s/.test(rest)) return [];
    return [
      { insert: "/session name ", label: "name", hint: "name the session you're in" },
      ...ctx.sessions.map((sn) => ({
        insert: `/session use ${sn.id}`,
        label: sn.label,
        hint: sn.hint,
      })),
    ].filter((o) => filter(o.label));
  }
  if (cmd === "/settings") {
    const rest = input.slice(space + 1);
    // Two levels: one row per setting until a setting is named, then its
    // values. Listing every key/value pair at once ran to 20-odd rows and
    // buried the settings that are not thinkingLevel.
    const named = ctx.settings.find((o) => rest.startsWith(`${o.key} `));
    if (named)
      return ctx.settings
        .filter((o) => o.key === named.key && filter(`${o.key} ${o.value}`))
        .map((o) => ({
          insert: `/settings ${o.key} ${o.value}`,
          label: `${o.key} ${o.value}`,
          hint: o.hint,
        }));
    const seen = new Set<string>();
    return ctx.settings
      .filter((o) => !seen.has(o.key) && seen.add(o.key) && filter(o.key))
      .map((o) => {
        const current = ctx.settings.find(
          (s) => s.key === o.key && s.hint.split(" · ")[0] === "current",
        );
        return {
          // trailing space so picking the key reopens the menu at its values
          insert: `/settings ${o.key} `,
          label: o.key,
          hint: current ? current.value : "",
        };
      });
  }
  return [];
}

/** Rotating input hints. */
export const placeholders = [
  "Ask anything, or / for commands",
  "^j/^k workspaces · ^1-9 jump",
  "Esc to interrupt",
  "\\ + Enter for a new line",
];
