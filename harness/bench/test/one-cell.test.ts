import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

/**
 * A terminal has one cell (owner, 2026-09-17: "for a terminal all font sizes will be same; only
 * color and weight vary"). Nothing in the centre pane may set a size of its own — difference is
 * carried by weight, colour and case. This greps for the ways a size creeps back in.
 */
const PANE = [
  "src/renderer/components/Chat.tsx",
  "src/renderer/components/ToolCall.tsx",
  ...fs.readdirSync("src/renderer/components/results").filter((f) => f.endsWith(".tsx")).map((f) => `src/renderer/components/results/${f}`),
];
const FORBIDDEN = [/\btext-(2xs|xs|sm|base|md|lg|xl)\b/, /\btext-\[\d+px\]/, /font-size\s*:/, /\bleading-\[[\d.]+px\]/];

test("nothing in the centre pane sets its own font size", () => {
  for (const f of PANE) {
    const src = fs.readFileSync(path.resolve(f), "utf8");
    for (const re of FORBIDDEN) {
      const hit = re.exec(src);
      assert.equal(hit, null, `${f}: ${hit?.[0]} — the pane is one cell; use weight, colour or case`);
    }
  }
});

test("the cell is a token, and .prose is set in it", () => {
  const css = fs.readFileSync(path.resolve("src/renderer/styles/app.css"), "utf8");
  assert.match(css, /--cell: 13px;/);
  assert.match(css, /--cell-lh: 20px;/);
  // Every rule under .prose that sets a size sets it to the token.
  for (const m of css.matchAll(/\.prose[^{]*\{([^}]*)\}/g)) {
    const size = /font-size:\s*([^;]+);/.exec(m[1]);
    if (size) assert.match(size[1].trim(), /var\(--cell\)/, `.prose rule sets ${size[1]}`);
  }
  // A heading is bold, not bigger.
  assert.match(css, /\.prose h1, \.prose h2, \.prose h3, \.prose h4 \{ font-size: var\(--cell\)/);
});

/**
 * opencode's One Dark, transcribed (`one-dark.json`). These are the values a diff is drawn with,
 * so a hunk in the bench and a hunk in opencode are the same green — checked against the source
 * rather than trusted to a hand copy.
 */
test("the one-dark diff palette is transcribed exactly, both themes", () => {
  const css = fs.readFileSync(path.resolve("src/renderer/styles/app.css"), "utf8");
  for (const [name, dark, light] of [
    ["diff-added-bg", "#2c382b", "#eafbe9"],
    ["diff-removed-bg", "#3a2d2f", "#fce9e8"],
    ["diff-highlight-added", "#aad482", "#489447"],
    ["diff-highlight-removed", "#e8828b", "#d65145"],
    ["diff-line-number", "#9398a2", "#666666"],
    ["diff-added-line-number-bg", "#283427", "#e1f3df"],
    ["diff-removed-line-number-bg", "#36292b", "#f5e2e1"],
    ["diff-hunk-header", "#56b6c2", "#0184bc"],
  ] as const) {
    assert.ok(css.includes(`--${name}: ${dark};`), `${name} is missing its One Dark value`);
    assert.ok(css.includes(`--${name}: ${light};`), `${name} is missing its One Light value`);
  }
});

test("every duration this pane animates at is a token", () => {
  const css = fs.readFileSync(path.resolve("src/renderer/styles/app.css"), "utf8");
  for (const [name, value] of [
    ["--motion-body", "350ms"],
    ["--motion-shell", "320ms"],
    ["--motion-shimmer", "1200ms"],
    ["--motion-progress", "1200ms"],
    ["--motion-copied", "2000ms"],
  ]) assert.ok(css.includes(`${name}: ${value};`), `${name} is missing`);
  // An infinite animation must never be left to run at 1ms: that is a strobe, not less motion.
  assert.match(css, /animation-iteration-count: 1 !important;/);
});

/**
 * ONE status row under the composer. The owner saw the same `Build · model` twice — a turn footer
 * hard against the composer and the composer's own row — and two statements of one fact read as
 * two facts (2026-09-17, side by side with opencode).
 */
test("the composer carries exactly one status row, and the footer says where you are", () => {
  const src = fs.readFileSync(path.resolve("src/renderer/components/Chat.tsx"), "utf8");
  assert.equal(src.split('data-slot="composer-status"').length - 1, 1);
  // The idle footer names the path, not the mode again.
  assert.match(src, /fallback=\{<span class="min-w-0 truncate" title=\{thread\(\)\?\.name\}>\{where\(\)\}<\/span>\}/);
  // opencode's own composer padding (`session-composer-region.tsx:102`), and the theme's accent rail.
  assert.match(src, /border-l-2 border-accent bg-input/);
  assert.match(src, /class="flex items-start px-4 pt-3 pb-1\.5 font-mono"/);
});

/**
 * Every animation answers `prefers-reduced-motion`, and every looping one stops rather than running
 * a thousand times a second. A strobe is not less motion.
 */
test("motion is tokenised, cited and reduced-motion safe", () => {
  const css = fs.readFileSync(path.resolve("src/renderer/styles/app.css"), "utf8");
  for (const cls of ["working-dot", "spring-in", "arrive", "tick"]) assert.ok(css.includes(`@keyframes ${cls}`), `${cls} is missing`);
  // The dot grid pins its middle dot when motion is reduced, as its own stylesheet does.
  assert.match(css, /\[data-dot="12"\] \{ opacity: 1; \}/);
  assert.match(css, /\.springy, \.arrive, \.tick \{ animation: none;/);
  // The composer's caret is a block.
  assert.match(css, /textarea\[data-composer\] \{ caret-shape: block;/);
});

test("the command list is a full-width list above the composer, not a floating panel", () => {
  const src = fs.readFileSync(path.resolve("src/renderer/components/Chat.tsx"), "utf8");
  assert.match(src, /data-slot="slash-menu"/);
  assert.ok(!/data-slot="slash-menu"[\s\S]{0,200}rounded-md/.test(src), "no rounding, no border, nothing floating");
  // The selected row is a solid accent bar with dark text, spanning the full width.
  assert.match(src, /"bg-accent text-bg": i\(\) === pick\(\)/);
});

/**
 * The pane is the TUI's, not their web app's: mono everywhere, one cell, and the rows drawn with
 * the TUI's own geometry (`packages/tui/src/routes/session/index.tsx:1914`, `:1994`).
 */
test("tool rows use the TUI's geometry and colours", () => {
  const src = fs.readFileSync(path.resolve("src/renderer/components/ToolCall.tsx"), "utf8");
  assert.match(src, /w-\[2ch\]/, "the icon column is two cells (INLINE_TOOL_ICON_WIDTH)");
  assert.match(src, /"pl-3": !railed\(\)/, "an inline row is indented three cells");
  assert.match(src, /~ \{\[line\(\)\.verb/, "a pending row reads `~ …`");
  assert.match(src, /"line-through": denied\(\)/, "a refusal is struck through, not reddened");
});

test("the composer's caret is the TUI's block, steady, in the text colour", () => {
  const src = fs.readFileSync(path.resolve("src/renderer/components/BoxCursor.tsx"), "utf8");
  const code = src.replace(/\/\*\*[\s\S]*?\*\//g, "");
  assert.match(src, /bg-fg text-bg/, "live: the text token in reverse video");
  assert.match(src, /bg-line text-bg/, "disabled: the panel tone");
  assert.ok(!/animate|blink|keyframes/i.test(code), "steady — nothing in the TUI blinks it");
  const chat = fs.readFileSync(path.resolve("src/renderer/components/Chat.tsx"), "utf8");
  assert.match(chat, /caret-transparent/);
});

test("the status line shows exactly one working indicator", () => {
  const chat = fs.readFileSync(path.resolve("src/renderer/components/Chat.tsx"), "utf8");
  assert.equal((chat.match(/<Scanner/g) ?? []).length, 1);
  assert.equal((chat.match(/<WorkingDots/g) ?? []).length, 0, "the dot grid was the web app's; the TUI has one scanner");
  assert.equal((chat.match(/<Spinner/g) ?? []).length, 0, "a spinner beside the scanner is the same fact twice");
});

/**
 * A live question takes the composer's place, as Claude Code does it (owner, 2026-09-17): while it
 * is open there is nothing to type, and the transcript keeps only the record of what was asked and
 * answered — never a tool row, never a second copy of the card.
 */
/**
 * The per-turn footer shows on hover (opencode's message meta) with its height reserved, so the
 * transcript never jumps; and it is built from the MESSAGE, never from the live `line()`.
 */
/**
 * The picker is ONE grouped list at one cell size: the owner was "confused with the way highlights
 * are happening" when a highlight could be in either of two columns. Every row shares one gutter
 * and one name column, so nothing shifts between a header and a model.
 */
/**
 * The transcript carries the CONVERSATION and nothing else (owner: "check what all is getting into
 * history. remove such unnecessary things"). A command echo, a plan repeat and a failed desktop
 * call are not conversation; a state change worth reading back is a quiet divider.
 */
/** The whole session is visible: filled from the top when short, and read whole when it is small. */
/**
 * The panel lists what the platform answers. It said "No repositories in this team yet" for a team
 * that had them, because `App.tsx` rendered the `REPOS` fixture and nothing fetched.
 */
test("repositories come from the platform, never the fixture", () => {
  const app = fs.readFileSync(path.resolve("src/renderer/App.tsx"), "utf8");
  assert.ok(!/\bREPOS\b/.test(app), "the demo fixture is not what the panel lists");
  assert.match(app, /repos=\{repos\(\)\}/, "the panel renders the fetched list, never a fixture");
  // `/v1/repos` authenticates with a SESSION JWT and refuses the desktop's CLI token, so calling it
  // on the refresh beat 401'd once a beat — and every 401 used to sign the person out.
  assert.ok(!/platform\.repos\(\)/.test(app), "a route that always 401s is not polled");

  const panel = fs.readFileSync(path.resolve("src/renderer/components/TeamPanel.tsx"), "utf8");
  // The two fields the listing does not have, and which the old panel drew.
  assert.ok(!/r\.branch|r\.updated/.test(panel), "a row says nothing rather than inventing a branch or a date");
});

test("a session is shown whole", () => {
  const chat = fs.readFileSync(path.resolve("src/renderer/components/Chat.tsx"), "utf8");
  // A reversed column stacks from the bottom; without this a short thread sat pinned low with the
  // upper 40% of the pane blank (owner's screenshot).
  // NOT `justify-end`: on a reversed scroller it packs the overflow past the start edge, so a long
  // session could not be scrolled at all and its last line sat under the composer. The scroller
  // stays a plain reversed column that may shrink (`min-h-0`); a SHORT thread is filled from the
  // top by `mt-auto` on the inner column, which is inert once the content overflows.
  // The scroller's own class attribute — not the comment above it, which names `justify-end` to
  // say why it must not come back.
  const scrollerClass = /ref=\{scroller\}[\s\S]*?\n\s*class="([^"]+)"/.exec(chat)?.[1] ?? "";
  assert.ok(scrollerClass, "the scroller's class list");
  assert.ok(!/justify-end/.test(scrollerClass), "`justify-end` on the scroller silently kills scrolling");
  assert.match(scrollerClass, /flex-col-reverse/);
  assert.match(scrollerClass, /min-h-0/, "a flex child must be allowed to shrink before it can scroll");
  assert.match(scrollerClass, /overflow-y-auto/);
  assert.match(chat, /ref=\{column\} class="mt-auto/, "a short thread is filled from the top by the column, not the scroller");
  const client = fs.readFileSync(path.resolve("src/bench-client.ts"), "utf8");
  // Opening with a tail and never filling it in is what hid the first prompt.
  assert.match(client, /r\.total <= FULL_UNDER/, "a short session is read whole, not left on its tail");
});

test("only the conversation reaches the transcript", () => {
  const live = fs.readFileSync(path.resolve("src/renderer/live.ts"), "utf8");
  const app = fs.readFileSync(path.resolve("src/renderer/App.tsx"), "utf8");
  // A local command runs without ever being pushed as a row.
  assert.ok(!/live\.thread\(pi\)\.sent\(text\)/.test(app), "a local command is not something the person said");
  // Stop and cancel go to the bench as HTTP: as prompts they landed in pi's context and session file.
  assert.ok(!/\/proc-stop \$\{|\/cancel \$\{/.test(live), "no slash string is ever sent to pi from the renderer");
  assert.match(live, /bench\("POST", `\/procs\/\$\{p\.id\}\/stop`/);
  assert.match(live, /bench\("POST", `\/tasks\/\$\{t\.id\}\/cancel`/);
  // The plan panel is the surface for the plan; it is not repeated into the transcript.
  assert.ok(!/Todo: \$\{/.test(live), "the PLAN panel is the surface");
  // A proposal's answer is the CARD: no user row, and nothing sent to pi — the bench wakes the
  // waiting tool and the answer reaches the model as that tool's result.
  const ans = live.slice(live.indexOf("export function answerProposal"), live.indexOf("export function answerProposal") + 400);
  assert.ok(!/\.sent\(/.test(ans), "the card is the record; a `> yes` row said it twice");
  assert.ok(!/harness\.pi\(/.test(ans), "an answer is a tool result, never a prompt");
  // A change worth reading back is a divider, derived from the bench's rows — never a message.
  assert.match(live, /divider\(`Model changed to \$\{name\}`\)/);
  assert.match(live, /divider\(`Thinking \$\{t\.thinking\}`\)/);
  assert.match(live, /divider\(`Effort \$\{t\.effort\}`\)/);
});

test("the model dialog is one grouped list on a shared gutter", () => {
  const dlg = fs.readFileSync(path.resolve("src/renderer/components/ModelDialog.tsx"), "utf8");
  // The cursor row is a full-width bar at NORMAL weight — never a bar AND bold.
  assert.match(dlg, /classList=\{\{ "bg-hover": cursor\(\) === i\(\) \}\}/);
  assert.ok(!/font-bold/.test(dlg), "the bar is the highlight; bold as well is two highlights");
  // One gutter constant, shared by every row, and models indented 2ch beneath their header.
  assert.match(dlg, /const GUTTER = "w-\[2ch\] shrink-0 select-none"/);
  assert.match(dlg, /class="pl-\[2ch\] text-fg"/);
  // Dropped on the owner's word: no thinking tag, and no wall of unconfigured providers.
  assert.ok(!/thinking</.test(dlg), "every model has thinking; the tag said nothing");
  assert.ok(!/not configured/.test(dlg), "only configured providers are listed");
  assert.match(dlg, /no provider configured — add a key in Settings/);
  // ↑↓ and Enter only: there are no columns left to move between.
  assert.ok(!/ArrowLeft|ArrowRight/.test(dlg));
});

test("a turn's footer is its own and shows on hover", () => {
  const chat = fs.readFileSync(path.resolve("src/renderer/components/Chat.tsx"), "utf8");
  assert.match(chat, /opacity-0 transition-opacity[^"]*group-hover\/text:opacity-100/, "hover, by opacity — the row keeps its height");
  assert.match(chat, /✻&nbsp; \{turnMeta\(\{/, "the footer is built from the message");
  const footer = chat.slice(chat.indexOf("✻&nbsp;"), chat.indexOf("✻&nbsp;") + 600);
  assert.ok(!/\blinef?\(\)/.test(footer), "never the live line: an old turn must not change when the model does");
});

test("a live question replaces the input, and the transcript keeps only the record", () => {
  const chat = fs.readFileSync(path.resolve("src/renderer/components/Chat.tsx"), "utf8");
  // The card is inside the composer block, and the input is hidden while it is up.
  assert.match(chat, /<Show when=\{waiting\(\)\}>/);
  // The picker replaces the input the same way (spec §1.3): it is IN PLACE OF the composer, never
  // stacked above it — the `❯` row stayed visible under the dialog on the fleet.
  assert.match(chat, /classList=\{\{ hidden: !!waiting\(\) \|\| dialogOpen\(\) \}\}/);
  assert.match(chat, /\+\{waitingCount\(\) - 1\} more waiting/, "a second question is counted, not stacked");
  // The transcript renders the record component for a question row, not the card.
  assert.match(chat, /fallback=\{<Answered q=\{b as QuestionRow\} \/>\}/);
  assert.match(chat, /You answered:/, "§21's record: what you answered, then one line per answer");
  // The card carries no argument table and no frame of its own: the composer's rail is the frame.
  // The only use of the arguments is the one muted VALUES line under a proposal's verb.
  assert.equal((chat.match(/props\.q\.args/g) ?? []).length, 1);
  assert.match(chat, /argLine\(props\.q\.args, props\.q\.summary\)/);
  const card = chat.slice(chat.indexOf('data-component="question-card"'), chat.indexOf("Enter to select"));
  assert.ok(!/border|bg-request|rounded/.test(card), "no border, no background, no rounding");
  assert.ok(!/<Time /.test(card), "a card is a thing to answer, not a row in a log");
  // The permission prompt: the verb, what it acts on, one sentence, three ways out. One blank line.
  assert.match(chat, /Do you want to proceed\?/);
  assert.match(chat, /don't ask again for \$\{verb\(\)\.toLowerCase\(\)\} this session/);
  assert.match(chat, /No, and tell the bench what to do differently \(esc\)/);
  assert.equal((chat.match(/h-\[var\(--cell-lh\)\]/g) ?? []).length, 2, "one blank line per shape, no more air");
  // A question's descriptions sit UNDER their option, indented four cells and muted: inline was
  // unreadable at the column (owner, 2026-09-17).
  assert.match(chat, /pl-\[4ch\] wrap-words whitespace-pre-wrap text-subtle/);
  // Option 2 is a standing yes for this tool, for this session only.
  assert.match(chat, /live\.allowTool\(props\.session, props\.q\.tool\)/);
  assert.match(chat, /<Marker on=\{pick\(\) === p\.i\} \/>/, "the selected row is marked (❯), not filled with a bar");
  assert.ok(!/bg-accent|bg-selected/.test(card), "no bar behind the selected option");
  // And no tool row is ever pushed for it.
  const live = fs.readFileSync(path.resolve("src/renderer/live.ts"), "utf8");
  assert.match(live, /if \(name === "question"\) return;/);
});

/**
 * A folder must LOOK like one. On the fleet every entry drew with the file glyph and no chevron,
 * because the rows were read for a flag the tool server does not send (owner, 2026-09-18).
 */
test("the files tree draws folders as folders, and keeps its open state outside the rows", () => {
  const src = fs.readFileSync(path.resolve("src/renderer/components/inspector/FsTree.tsx"), "utf8");
  // A directory row: chevron in the gutter, folder icon beside it — the PLAN tree's grammar.
  assert.match(src, /<Show when=\{p\.dir\}>\s*<Icon name=\{open\(\) \? "chevronDown" : "chevronRight"\}/);
  assert.match(src, /<Icon name=\{p\.dir \? "folder" : "file"\}/);
  assert.match(src, /class="h-5\.5"/, "the same row height as every other tree");
  // Open state is the panel's, keyed by path: no row object carries it.
  assert.match(src, /open: Set<string>/);
  assert.match(src, /props\.open\.has\(here\(\)\)/);
  assert.match(src, /isDir\(e\)/, "a row is a directory because the tool server said `kind: dir`");
  assert.ok(!/createSignal\(!!\w+\.open\)/.test(src), "a row must not hold its own open flag");
  // Every entry stays in its place, directories first, ignored merely dimmed — nothing grouped.
  assert.ok(!/ignored<\/span>|N ignored/.test(src), "no ignored fold");
  assert.match(src, /isDir\(a\) === isDir\(b\) \? a\.name\.localeCompare\(b\.name\) : isDir\(a\) \? -1 : 1/);
  assert.match(src, /rowTone\(p\.letter, p\.ignored\)/, "the name is tinted by its git state");
  assert.match(src, /statusBadge\(p\.letter\)/, "and carries the one-letter badge");
  assert.match(src, /deletedIn\(props\.changes \?\? \[\], props\.path\)/, "a deleted file is drawn where it was");
  // And the panel owns the set, so a refetch cannot shut a fold.
  const view = fs.readFileSync(path.resolve("src/renderer/components/inspector/WorkView.tsx"), "utf8");
  assert.match(view, /const \[open, setOpen\] = createSignal\(new Set<string>\(\)\)/);
  assert.match(view, /<FsTree scope=\{scope\(\)\} open=\{open\(\)\} changes=\{diff\(\)\?\.changes\} onToggle=\{toggle\}/);
});
