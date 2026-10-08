---
name: codemode
description: How to write codemode scripts. Read this before your first codemode call in a session, and whenever a task needs several tool calls, a loop over many items, or filtering of large output.
---

# Codemode

`codemode` runs a JavaScript script that calls the other tools. The script runs as the body of an async function, so top-level `await` and `return` work. Only what the script returns or prints reaches you. Use this to make many calls in one turn and keep raw output out of your context.

## When to use it

Use a script when:

- several calls are independent: fetch ten URLs, read five files, run three commands. Run them in parallel.
- one call's result picks the next call: list, then read each match.
- the output is large and you need a small part of it: a log, a JSON API, a directory listing.

Use a single plain call inside a script when you only need one simple command and its whole output, such as `git status`. Do not build loops or helpers for one call.

## What each call returns

- `tools.bash({ command })` resolves to an object, not a string:
  `{ output, exit_code, truncated, wall_time_seconds }`. Read `.output`. Check `.exit_code`; a non-zero exit does not throw.
- `tools.web_fetch({ url })` resolves to the page text. JSON comes back verbatim, so `JSON.parse` it directly. A failed fetch returns a string starting with `error`; it does not throw.
- `tools.read({ path })` resolves to the file's text.
- A call that is blocked or gets bad arguments rejects with an `Error`.

## Rules

1. Run independent calls in parallel with `Promise.all`, or `Promise.allSettled` when some may fail. Make one tool call per item. Never chain commands with `;` or `&&` inside one `bash` call to fetch many things in a row; that runs them one at a time.
2. Prefer `web_fetch` over `curl` in `bash` for HTTP. It is one call per URL and returns text you can parse.
3. Return a compact result: the fields you need, not raw pages. Shape it with `.map`, `.filter` and `.slice` before returning.
4. A failed call made before a script error is not undone. Writes are real.

## Examples

Top 40 Hacker News stories, fetched in parallel:

```js
const ids = JSON.parse(await tools.web_fetch({ url: "https://hacker-news.firebaseio.com/v0/topstories.json" })).slice(0, 40);
const items = await Promise.allSettled(
  ids.map((id) => tools.web_fetch({ url: `https://hacker-news.firebaseio.com/v0/item/${id}.json` }).then(JSON.parse)),
);
return items
  .filter((r) => r.status === "fulfilled")
  .map((r, i) => `${i + 1}. ${r.value.title} (${r.value.score} points)`)
  .join("\n");
```

Several commands at once, each checked:

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

Find, then read only what matters:

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

Filter a large output down:

```js
const r = await tools.bash({ command: "journalctl -u myapp --since '1 hour ago' --no-pager" });
const errors = r.output.split("\n").filter((l) => /error|panic/i.test(l));
return { total: errors.length, last: errors.slice(-20) };
```

## Wrong patterns

```js
// Wrong: bash returns an object, not a JSON string
JSON.parse(await tools.bash({ command: "curl -s https://example.com/api" }));

// Wrong: ten fetches run one after another inside one shell
await tools.bash({ command: "curl -s $A; curl -s $B; curl -s $C" });
```
