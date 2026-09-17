# Vendored: opencode's session renderer

Upstream: <https://github.com/sst/opencode>, MIT (`LICENSE-opencode` beside this file).
Commit: **`5a8335857b0ebec44ef6aa1d52b339cf25c329ca`** (`5a83358`, "fix(console): show migrated
workspaces in selector (#49520)"), package `@opencode-ai/session-ui` **1.18.31**.
Working copy surveyed: `/Volumes/kdisk/rustic-git-wt/opencode-ref`.

Spec §23: the centre pane is a PORT of that package, not a re-drawing. Edits to vendored files are
minimal and every one is marked `// harness:` so a re-vendor is a diff, not an archaeology dig.

## Why this is portable at all

`session-ui` makes **no** HTTP, WebSocket, SDK, `localStorage`, `import.meta.env` or node calls. Every
piece of server state arrives through `context/data.tsx`, which is a `createSimpleContext` SHAPE the
consuming app fills. That is the seam our adapter writes into, and it is why a port is possible
without ever calling opencode's API.

`session-ui` also has no build step: its `exports` point straight at `.tsx`/`.ts`/`.css`, so the
consumer compiles it. We are that consumer.

## What is vendored

Paths below are relative to the upstream repo; they land under `src/renderer/opencode/` keeping
their own shape (`session-ui/…`, `ui/…`, `core/…`, `sdk/…`).

### 1. `packages/session-ui/src` — the renderer itself

- `components/` (v1): `basic-tool` · `dock-prompt` · `file` · `file-media` · `file-search` ·
  `file-ssr` · `line-comment` · `line-comment-annotations` · `line-comment-styles` ·
  `markdown` · `markdown-cache` · `markdown-code-state` · `markdown-inline-code-kind` ·
  `markdown-projection` · `markdown-stream` · `markdown-worker` · `markdown-worker-protocol` ·
  `markdown-worker-queue` · `markdown-worker-transport` · `markdown.worker` · `message-file` ·
  `message-nav` · `message-part` · `message-part-text` · `part-default-open` · `apply-patch-file` ·
  `session-diff` · `session-retry` · `session-review` · `session-turn` · `tool-count-label` ·
  `tool-count-summary` · `tool-error-card` · `tool-status-title`, each with its sibling `.css`
  (`basic-tool.css`, `file.css`, `markdown.css`, `message-nav.css`, `message-part.css`,
  `session-review.css`, `session-turn.css`, `shell-submessage.css`, `tool-count-label.css`,
  `tool-count-summary.css`, `tool-error-card.css`, `tool-status-title.css`).
- `v2/components/`: `attachment-card-v2` · `basic-tool-v2` · `comment-card-v2` ·
  `line-comment-annotations-v2` · `session-file-panel-v2` · `session-progress-indicator-v2` ·
  `session-review-empty-changes-v2` · `session-review-empty-no-git-v2` ·
  `session-review-file-preview-v2` (+ `-virtualize.ts`) · `session-review-v2` ·
  `tool-error-card-v2`, with their `.css`.
- `v2/components/prompt-input/`: `index.tsx` · `interaction.ts` · `machine.ts` · `store.ts` ·
  `types.ts` · `attachments.ts` + `attachments.css` — the composer.
- `pierre/`: `comment-hover` · `commented-lines` · `diff-selection` · `file-find` · `file-runtime` ·
  `file-selection` · `index` · `media` · `selection-bridge` · `virtualizer` · `worker`.
- `context/`: `index.ts`, `data.tsx`.
- `styles/index.css` (v1 CSS only — the v2 CSS is imported per component).

**Not taken:** every `*.stories.tsx` (12 in `components/`, 4 under `v2/`) and every `*.test.ts`
(11 in `components/`, 3 under `v2/`). Their tests stay upstream; our own tests cover the adapter.

### 2. `packages/ui/src` — only what session-ui imports

The package is 1682 files / 11 MB; we take the import closure, not the package.

- Components (`.tsx` + `.css`): `accordion` · `animated-number` · `button` · `card` · `checkbox` ·
  `collapsible` · `diff-changes` · `dock-surface` · `dropdown-menu` · `file-icon` · `icon` ·
  `icon-button` · `image-preview` · `provider-icon` · `radio-group` · `resize-handle` ·
  `scroll-view` · `spinner` · `sticky-accordion-header` · `text-reveal` · `text-shimmer` · `tooltip`.
- v2 components: `button-v2` · `diff-changes-v2` · `icon-button-v2` · `icon.tsx` · `keybind-v2` ·
  `line-comment-v2` · `menu-v2` · `segmented-control-v2` · `text-input-v2` · `text-shimmer-v2` ·
  `tooltip-v2`.
- `context/`: `index.ts` · `helper.tsx` · `file.tsx` · `dialog.tsx` · `i18n.tsx` ·
  `marked-parser.tsx` · `marked-theme.tsx` · `marked-theme-register.tsx` · `worker-pool.tsx`.
- `hooks/`: `index.ts` · `use-filtered-list.tsx` · `create-auto-scroll.tsx`.
- `i18n/en.ts` (the one locale; the other ~60 are not taken).
- Icon sprites: `components/file-icons/{sprite.svg,types.ts}`,
  `components/provider-icons/{sprite.svg,types.ts}`.
- `styles/`: `index.css` (TRIMMED — see below), `colors.css`, `theme.css`, `base.css`,
  `utilities.css`, `animations.css`, `v2/styles/{colors,theme,tailwind}.css`.
- `theme/`: `index.ts` · `color.ts` · `resolve.ts` · `loader.ts` · `context.tsx` · `types.ts` ·
  `default-themes.ts` · `v2/resolve.ts`, and from `theme/themes/` only **`one-dark.json`** and
  `oc-2.json` (the loader's default id).

### 3. `packages/core/src/util` — three pure files, 129 lines

`path.ts` (`getFilename`, `getDirectory`, `getFilenameTruncated`), `encode.ts` (`checksum`,
`sampledChecksum`), `binary.ts` (`Binary.search`). `@opencode-ai/core` itself is a node/bun server
package and is NOT taken.

### 4. `packages/sdk/js/src/v2/gen/types.gen.ts` — types only

13,625 lines of generated types. Vendored as types; nothing from `@opencode-ai/sdk` runs.

## Dependencies to add (opencode's own versions, `catalog:` resolved)

| package | version | why |
|---|---|---|
| `@kobalte/core` | `0.13.11` | collapsible, hover-card, i18n primitives |
| `@pierre/diffs` | `1.2.10` | the diff view (+ `/ssr`, `/worker`) |
| `@shikijs/stream` | `4.2.0` | streaming highlight |
| `@shikijs/transformers` | `3.9.2` | markdown code transforms |
| `shiki` | `4.2.0` | highlighter |
| `@solid-primitives/bounds` | `0.1.3` | layout measurement |
| `@solid-primitives/event-listener` | `2.4.5` | listeners |
| `@solid-primitives/media` | `2.3.3` | `prefers-reduced-motion`, breakpoints |
| `@solid-primitives/resize-observer` | `2.1.3` | sticky headers, virtualiser |
| `@solidjs/meta` | `0.29.4` | peer of `@opencode-ai/ui` |
| `diff` | `8.0.2` | patch parsing |
| `dompurify` | `3.3.1` | markdown sanitising |
| `fuzzysort` | `3.1.0` | command/file filtering |
| `katex` | `0.16.27` | maths in markdown (pulled by `marked-parser`) |
| `luxon` | `3.6.1` | timestamps |
| `marked` | `18.0.7` | **we have `^18.0.13`** — pin to theirs |
| `marked-shiki` | `1.2.1` | code fences |
| `morphdom` | `2.7.8` | streaming markdown patches |
| `motion` | `12.34.5` | springs, the shell submessage entry |
| `motion-dom` / `motion-utils` | `12.34.3` / `12.29.2` | motion's own deps, pinned upstream |
| `remeda` | `2.26.0` | small utilities |
| `remend` | `1.3.0` | markdown repair while streaming |
| `solid-list` | `0.3.0` | list navigation |
| `strip-ansi` | `7.1.2` | shell output |

We already have `solid-js` (1.9, theirs is 1.9.10 **patched** — see risks) and `marked`.

## What cannot be taken, and what we do instead

| upstream thing | why | our answer |
|---|---|---|
| `@opencode-ai/client/promise` (`FileDiffInfo`) | a local tarball in their repo, not on npm; used type-only at `session-diff.ts:4`, `session-review.tsx:19`, `session-turn.tsx:7`, `context/data.tsx:2`, `session-review-file-preview-v2.tsx:10` | hand-written type shim in `opencode/shims/client-promise.ts` |
| `@opencode-ai/sdk/v2` barrel in VALUE position — `message-nav.tsx:1`, `message-part.tsx:19-33`, `session-review.tsx:18`, `session-turn.tsx:1-6` | the barrel re-exports `./server.js`, which starts a node HTTP server | rewrite each to `import type` from the vendored `types.gen.ts` (`// harness:` marked) |
| `@opencode-ai/sdk/v2/client` (`createClient`) | real HTTP client | never imported; only its types are used |
| `@opencode-ai/core` | node/bun server package | copy the three pure util files |
| `context/data.tsx`'s store | it is a shape the app fills with SERVER state | our `adapter.ts` fills it from the bench's own events — this is the whole port's seam |
| `packages/app` composer/docks (`session-composer-*`, `session-question-dock`, `session-permission-dock`, `prompt-input-v2`) | every one calls `useSDK()` (`session-question-dock.tsx:11,65`, `session-composer-state.ts:9,30`, `session-composer-controls.ts:14,28,68`, `prompt-model-selection.ts:6,12`, `prompt-input-v2.tsx:25,82`) | not vendored: we re-implement the thin app layer against `PromptInputV2` and `DockShell`/`DockTray`, calling the BENCH |
| their storybook scaffold/fixtures | stories only | not taken |
| `one-light.json` | **does not exist upstream** — each theme JSON carries its own `light` and `dark` variants (`loader.ts:22-25`) | use `one-dark.json`'s two variants, which is what §23 means |

## Build notes for our Vite/Solid app

- Path aliases in `vite.config.ts` + `tsconfig.renderer.json`: `@opencode-ai/session-ui/*`,
  `@opencode-ai/ui/*`, `@opencode-ai/core/*`, `@opencode-ai/sdk/*`, `@opencode-ai/client/*` →
  the vendored tree. Nothing outside `src/renderer/opencode/` learns those names.
- Workers: `markdown-worker.ts:120` and `pierre/worker.ts:10` construct `new Worker(url, {type:"module"})`,
  and `pierre/worker.ts:2` uses Vite's `?worker&url` — we are Vite, so both work; shiki's wasm has
  to be served as an asset.
- CSS: `ui/styles/index.css` `@import`s ~45 component stylesheets and `katex/dist/katex.min.css`;
  trimmed to the components we take. `session-ui/styles/index.css` is v1 only — v2 CSS is imported
  per component. Both are `@layer`-based, which coexists with our Tailwind 4 layers.
- SSR guards (`isServer`, `typeof window`) are inert in Electron; left as they are.
- Clipboard: `tool-error-card.tsx:92`, `message-part.tsx:86` — browser APIs, fine.

## Risks, stated before the work

1. **Size.** The closure is ~120 session-ui files plus ~90 ui files plus a 13.6k-line type file.
   It will dwarf the rest of the renderer, and a re-vendor is the only supported way to update it.
2. **`solid-js@1.9.10` is patched upstream** (`patches/solid-js@1.9.10.patch`). We run unpatched
   Solid; if a vendored component depends on the patch it fails at runtime, not at compile.
3. **Two design systems in one document.** Their `colors.css`/`theme.css` and our One Dark tokens
   both define `:root` variables; the owner's one-cell 13px rule and their 14/13px ramp disagree.
   The port takes THEIR typography inside the pane — that is what "pixel-parity by construction"
   means — and our tokens keep the sidebar, inspector and terminal.
4. **Deleting our hand-drawn pane** (`ToolCall`, `results/*`, `Motion`, …) drops behaviour the port
   has no equivalent for yet: our proposal/question rows, exchanges, `kl_*` result cards, process
   rows, the memory/plan panels. Each needs a home in the port or an explicit removal.
5. `@pierre/diffs` and `shiki` pull a WASM highlighter into an Electron renderer that currently
   ships `highlight.js`; bundle size and cold start both change.

## `// harness:` edits, in full

| file | edit | why |
|---|---|---|
| `shims/client-promise.ts` | new file | `@opencode-ai/client` is a tarball in their repo; one type is used |
| `session-ui/components/markdown.worker.ts` | `bundledLanguages` → our `LANGUAGES` | shiki's full map code-split ~600 grammars (22 MB) into the build |
| `ui/context/marked.tsx` | same swap | same reason |
| `session-ui/components/message-file.ts` | `bundledLanguagesInfo` → our `LANGUAGE_NAMES` | the metadata table pulled the whole bundle in for a chip label |
| `ui/v2/components/toast-v2.tsx` | deleted | needs `solid-sonner`, imported by nothing in the closure |
| `../../tsconfig.renderer.json` | `lib: ES2023, DOM.Iterable` | they use `findLast`, `toReversed`, NodeList iteration |
| `../../vite.config.ts` | `worker: { format: "es" }` | rollup will not code-split an IIFE worker |
| `keybinds.ts` | their `packages/tui/src/config/keybind.ts` transcribed as DATA | their file imports `@opentui/core`, `@opentui/keymap` and `effect` — a terminal runtime we do not run |

The language list lives in `languages.ts`: ts/tsx/js/jsx/json/yaml/toml/rust/go/python/bash/sh/
dockerfile/sql/html/css/md/diff and their aliases. Anything else highlights as plain text, which is
what an unknown language already did. Build after the restriction: **13 MB** (was 22 MB); what is
left comes from `@pierre/diffs`' own worker bundle, inside the dependency, not from our imports.

## TODO — behaviour the port has no home for yet

| ours | where it goes now | still to do |
|---|---|---|
| proposals (`role: "question"`) | ANSWERED: their `question` tool part in the transcript. LIVE: their `DockPrompt` permission dock above the composer (`opencode/PermissionDock.tsx`), answering `POST /proposals/:id` | their three-way answer is once/always/reject; ours is yes/no, and "always" switches this session to accept-edits |
| asks and agents (`ask`) | a `task` part, `metadata.agent` = the agent's name or the workspace it was sent to | their task card links to a child session; ours has no child session id |
| `kl_*` platform answers | a synthetic `text` part with the answer as a fenced `json` block | our `WorkspaceCard`/`QuotaCard`/`EnvironmentCard` have no part type; either register a harness part or keep the block |
| process rows, memory, plan panel | the inspector, which stays ours | nothing — deliberate |
| exchanges (asks in flight) | the queue dock above the composer, ours | fold into their followup dock when the composer lands |

## The composer's cursor

Their prompt is a terminal input: it sets `input.cursorColor = theme.text` while live and
`theme.backgroundElement` when disabled (`packages/tui/src/component/prompt/index.tsx:252-253`) and
leaves the style to the terminal's block. Nothing in their code blinks it. Chromium's `caret-shape:
block` is not that cursor, so `opencode/BoxCursor.tsx` draws it: a mirror of the textarea in the
same metrics, with the character at the caret in reverse video — a block in the TEXT colour, steady,
hidden when the composer is not focused. The browser's own caret is `caret-transparent` beneath it.

## Theme, fonts and keys

- **Theme**: `src/renderer/theme.ts` calls their `applyTheme(one-dark.json, "one-dark")` at start
  (`ui/theme/loader.ts`), which writes the resolved tokens into `<style id="opencode-theme">` and
  stamps `data-theme`. Our own tokens moved to `data-scheme`, so the two coexist: their tokens rule
  the pane, ours the sidebar, inspector and terminal. `setColorScheme()` follows our light/dark/system.
- **Fonts**: IBM Plex reaches the port through THEIR variables — `--font-family-sans` and
  `--font-family-mono` are re-pointed at our `--font-ui` / `--font-mono` in `app.css`, so no
  vendored stylesheet is edited.
- **Keys**: `opencode/keybinds.ts` is their whole `Definitions` table (162 commands) with
  `LEADER_DEFAULT = "ctrl+x"`. `keys.ts` reads the leader from it, `chordOf()` expands `<leader>`,
  and the palette lists every upstream command this app answers under its own name and chord. Our
  ⌘ keys stay as the alias layer; both work.

## Order of work (one commit each)

1. **This file.** — "Plan the opencode session-ui port"
2. Vendor the trees verbatim + `LICENSE-opencode`, add the deps, aliases, make it compile.
   — "Vendor opencode's session renderer"
3. `adapter.ts`: our live store → their `Message`/`Part` shapes, with tests over a canned event
   sequence. — "Feed opencode's renderer from the bench"
4. Switch the centre pane to the ported view; delete what it replaces.
   — "The centre pane is opencode's"
5. Keybinds through their config (our `ctrl+x` leader stays), theme JSON through their loader,
   IBM Plex through their font variables. — "Keys and theme through opencode's own code"
