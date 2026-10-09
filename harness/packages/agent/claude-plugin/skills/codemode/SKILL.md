---
name: codemode
description: How to write codemode scripts. Read this skill before your first codemode call in a session. Also read it when a task needs many tool calls, a loop over many items, or a filter on a large output.
---

# Codemode

This skill is written in ASD-STE100 Simplified Technical English. Each word has one meaning. Each instruction is one sentence.

`codemode` runs a JavaScript script that calls the other tools. The script is the body of an async function. Thus `await` and `return` work at the top level. You get only the value that the script returns or prints. Use codemode to make many calls in one turn and to keep large outputs out of your context.

## When to use codemode

Use a script in these conditions:

- The calls are independent. Example: get ten URLs, read five files, or run three commands. Run them in parallel.
- The result of one call selects the next call. Example: get a list, then read each item that matches.
- The output is large and you need only a small part of it. Example: a log, a JSON API or a directory list.

If you need only one simple command and all of its output, make one plain call. Example: `git status`. Do not write loops or helper functions for one call.

## What each call returns

| Call | It returns | Failure |
|---|---|---|
| `tools.bash({ command })` | An object: `{ output, exit_code, truncated, wall_time_seconds }`. It is not a string. Read `.output`. | An exit code that is not 0 does not throw. Examine `.exit_code`. |
| `tools.exec({ cmd })` | An object: `{ exit_code, stdout, stderr }`. | An exit code that is not 0 does not throw. Examine `.exit_code`. |
| `tools.web_fetch({ url })` | The text of the page. JSON comes back as it is. Use `JSON.parse` on it directly. | A failed fetch returns a string that starts with `error`. It does not throw. |
| `tools.read({ path })` | The text of the file. | |
| Any call | | A call that is blocked or has bad arguments rejects with an `Error`. |

The main session has `bash`. A workspace session does not have `bash`. It has `exec`. Use the shell that is in your tool list. Do not guess.

## Rules

1. For one action, call the tool directly. Use codemode only for many calls, a loop over items, or a filter on a large output.
2. `await tools.bash(...)` returns an object. Read `.output` and `.exit_code`. Do not call string methods on the object.
3. Get URLs with `tools.web_fetch`. Do not use `curl` in `bash`.
4. Make one tool call for each item. Run independent calls in parallel with `Promise.all`. If some calls can fail, use `Promise.allSettled`.
5. Do not join many commands with `;` or `&&` in one `bash` call. The commands then run one after the other.
6. Return a small result. Return only the fields that you need. Do not return full pages. Use `.map`, `.filter` and `.slice` to make the result small.
7. `tools.searchTools(...)` and `tools.describeTool(...)` are async. Always use `await` with them.
8. To show the person a table, list or report, make it in the script. Then give it to `tools.display({ markdown })`. You can read the displayed text later. Reply in one line. Do not type the displayed text again.
9. A failed platform call throws, and the script stops there. The calls before it are not undone. Writes are real.

## Poll loops

A lifecycle call returns before the work is done. To wait for the result, write a poll loop. Obey these rules:

1. Set a maximum number of tries.
2. Stop the loop immediately when the state is the state that you want.
3. Stop the loop when the state is `error` or `deleted`. Return that state.
4. Use the correct state:

   | Object | The state when it is up |
   |---|---|
   | Workspace | `ready`. A workspace is not `running` at any time. |
   | Environment | `running` |

5. For a service, examine `services[].ready` or `service_status[].ready`. Do not use a regex on the spec.
6. Use the id (`ws-…` or `env-…`) in the call. Do not use the name.
7. Before the loop, make sure that the write call passed. A failed tool call throws.

```js
// Wait until a workspace is up
for (let i = 0; i < 60; i++) {
  const ws = JSON.parse(await tools.workspace_get({ workspace: "ws-23d55aca095079c4" }));
  if (ws.state === "ready" || ws.state === "error" || ws.state === "deleted") return ws.state;
  await tools.bash({ command: "sleep 5" });
}
return "still not ready after 60 tries";
```

## Examples

The top Hacker News stories as a table. Make the table in the script, show it, and reply in one line:

```js
const ids = JSON.parse(await tools.web_fetch({ url: "https://hacker-news.firebaseio.com/v0/topstories.json" })).slice(0, 20);
const items = await Promise.all(
  ids.map((id) => tools.web_fetch({ url: `https://hacker-news.firebaseio.com/v0/item/${id}.json` }).then(JSON.parse)),
);
const rows = items.map((s, i) => `| ${i + 1} | ${s.title.replace(/\|/g, "\\|")} | ${s.score} | ${s.descendants ?? 0} | ${s.url ?? `https://news.ycombinator.com/item?id=${s.id}`} |`);
const markdown = ["| # | Title | Points | Comments | URL |", "|---|---|---|---|---|", ...rows].join("\n");
await tools.display({ markdown });
return "shown";
```

Then reply in one line. Example: "The top 20 stories are above." Do not type the table again.

Many commands at the same time. Examine the result of each command:

```js
const cmds = { branch: "git branch --show-current", status: "git status --short", log: "git log --oneline -5" };
const out = {};
await Promise.all(
  Object.entries(cmds).map(async ([k, command]) => {
    const r = await tools.bash({ command });
    out[k] = r.exit_code === 0 ? r.output.trim() : `failed (${r.exit_code}): ${r.output.trim()}`;
  }),
);
return out;
```

Find files, then read only the necessary lines:

```js
const r = await tools.bash({ command: "grep -rl 'TODO' src --include=*.ts" });
const files = r.output.split("\n").filter(Boolean).slice(0, 10);
const hits = await Promise.all(
  files.map(async (path) => {
    const lines = (await tools.read({ path })).split("\n");
    return lines.flatMap((l, i) => (l.includes("TODO") ? [`${path}:${i + 1}: ${l.trim()}`] : []));
  }),
);
return hits.flat().join("\n");
```

Make a large output small:

```js
const r = await tools.bash({ command: "journalctl -u myapp --since '1 hour ago' --no-pager" });
const errors = r.output.split("\n").filter((l) => /error|panic/i.test(l));
return { total: errors.length, last: errors.slice(-20) };
```

## Incorrect patterns

```js
// Incorrect: bash returns an object, not a JSON string
JSON.parse(await tools.bash({ command: "curl -s https://example.com/api" }));

// Incorrect: ten fetches run one after the other in one shell
await tools.bash({ command: "curl -s $A; curl -s $B; curl -s $C" });

// Incorrect: a workspace is "ready", not "running". This loop does not stop until its limit.
if (ws.state === "running") return "up";

// Incorrect: the call uses a name, not an id. The answer is 404.
await tools.workspace_get({ workspace: "backend" });
```
