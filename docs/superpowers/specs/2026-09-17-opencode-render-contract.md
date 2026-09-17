# opencode session-view render contract

Source of record: `anomalyco/opencode` shallow clone at `/Volumes/kdisk/rustic-git-wt/opencode-ref`.
All `path:line` citations below are relative to that checkout root. Two independent renderers exist
and are documented side by side:

- **TUI** (`packages/tui/src`, OpenTUI/Solid, terminal cells) — the literal row grammar.
- **Web/desktop** (`packages/session-ui/src` rendered by `packages/app` inside `packages/desktop`
  Electron) — Solid + CSS. `packages/session-ui` is the shared session renderer; `packages/app`
  supplies docks, header and context chrome.

Nothing here is invented; every rule is quoted or cited.

---

## 0. Top-level structure

### Web
`SessionTurn` (`packages/session-ui/src/components/session-turn.tsx:153`) renders one **user turn**:
user message → divider → assistant parts → thinking → retry → summary diffs → error.

```
packages/session-ui/src/components/session-turn.tsx:386
    <div data-component="session-turn" class={props.classes?.root}>
      ... data-slot="session-turn-content"
          data-slot="session-turn-message-container"   (data-message={message().id})
            data-slot="session-turn-message-content"   → <Message .../>
            data-slot="session-turn-compaction"        → <MessageDivider label={divider()} />
            data-slot="session-turn-assistant-content" → <AssistantParts .../>
            data-slot="session-turn-thinking"
            <SessionRetry .../>
            data-slot="session-turn-diffs"
            <Card variant="error" class="error-card">
```

`Message` switches on role (`message-part.tsx:936`): `user` → `UserMessageDisplay`,
`assistant` → `AssistantMessageDisplay`. Assistant parts are grouped first (see §1.1), then each part
is dispatched through the registry `PART_MAPPING` by `part.type` (`message-part.tsx:1433`):

```
packages/session-ui/src/components/message-part.tsx:250
export const PART_MAPPING: Record<string, PartComponent | undefined> = {}
```

Registered part types: `"tool"` (:1534), `"compaction"` (:1649), `"text"` (:1654),
`"reasoning"` (:1759). Tool bodies come from a second registry keyed by tool name:

```
packages/session-ui/src/components/message-part.tsx:1489
export function getTool(name: string) {
  return state[name === "apply_patch" ? "patch" : name === "bash" ? "shell" : name]?.render
}
```

Registered tools: `read` (:1776), `list` (:1816), `glob` (:1842), `grep` (:1872), `webfetch` (:1905),
`websearch` (:1951), `task` (:1978), `shell` (:2085), `edit` (:2155), `write` (:2261), `patch` (:2321),
`todowrite` (:2525), `question` (:2576), `skill` (:2621). Anything else falls back to `GenericTool`
(`basic-tool.tsx:323`).

### TUI
`packages/tui/src/routes/session/index.tsx:1199` iterates messages inside a `<scrollbox>`; each is
`UserMessage` (:1364) or `AssistantMessage` (:1469). Assistant parts dispatch through

```
packages/tui/src/routes/session/index.tsx:1578
const PART_MAPPING = {
  text: TextPart,
  tool: ToolPart,
  reasoning: ReasoningPart,
}
```

and `ToolPart` (:1709) switches on `toolDisplay(part.tool)`:

```
packages/tui/src/routes/session/index.tsx:2626
const toolDisplays = new Set([
  "bash","glob","read","grep","webfetch","websearch","write","edit","task",
  "apply_patch","todowrite","question","skill","execute",
])
export function toolDisplay(tool: string) {
  return toolDisplays.has(tool) ? tool : "generic"
}
```

### Visibility filter (both)
```
packages/session-ui/src/components/message-part.tsx:711
export function renderable(part: PartType, showReasoningSummaries = true) {
  if (part.type === "tool") {
    if (HIDDEN_TOOLS.has(part.tool)) return false
    if (part.tool === "question") return part.state.status !== "pending" && part.state.status !== "running"
    return true
  }
  if (part.type === "text") return !!part.text?.trim()
  if (part.type === "reasoning") return showReasoningSummaries && !!part.text?.trim()
  return !!PART_MAPPING[part.type]
}
```
with `const HIDDEN_TOOLS = new Set(["todowrite"])` (`message-part.tsx:608`) — i.e. `todowrite`
is never rendered as a timeline part in the web app (`ToolPartDisplay` also early-returns:
`if (part().tool === "todowrite") return null`, :1538). In the TUI `todowrite` IS rendered (§1.13).

TUI additionally hides completed tools when tool details are off:
```
packages/tui/src/routes/session/index.tsx:1714
  const shouldHide = createMemo(() => {
    if (ctx.showDetails()) return false
    if (props.part.state.status !== "completed") return false
    return true
  })
```

---

## 1. Message part catalogue

### 1.0 Shared web row chassis: `BasicTool`

Every web tool row is a `Collapsible` whose trigger is the row and whose content is the body
(`packages/session-ui/src/components/basic-tool.tsx:257`). The structured trigger grammar is:

```
packages/session-ui/src/components/basic-tool.tsx:196
  data-slot="basic-tool-tool-info-structured"
    data-slot="basic-tool-tool-info-main"
      data-slot="basic-tool-tool-title"     ← <TextShimmer text={title().title} active={pending()} />
      data-slot="basic-tool-tool-subtitle"  ← title().subtitle   (only when !pending || subtitle || args)
      data-slot="basic-tool-tool-arg"       ← one span per title().args entry
    data-slot="basic-tool-tool-action"      ← only when !pending() && title().action
  <Collapsible.Arrow/>                      ← only when hasChildren && !hideDetails && !locked && (!pending || allowOpenWhilePending)
```

Row = **glyph/icon** (`props.icon`, an `IconProps["name"]`) + **title** + **subtitle** + **args**.
Pending state is `props.status === "pending" || props.status === "running"` (:93). While pending the
row cannot be expanded unless `allowOpenWhilePending` (:180: `if (pending() && !props.allowOpenWhilePending) return`).

Generic arg formatting (used by `GenericTool` and echoed by per-tool rows):
```
packages/session-ui/src/components/basic-tool.tsx:304
function label(input) {
  const keys = ["description","query","url","filePath","path","pattern","name"]
  return keys.map((key) => input?.[key]).find((v) => typeof v === "string" && v.length > 0)
}
function args(input) {                                        // :309
  const skip = new Set(["description","query","url","filePath","path","pattern","name"])
  ... `${key}=${value}` for string|number|boolean ... .slice(0, 3)   // max 3 args
}
```
`GenericTool` title is ``i18n.t("ui.basicTool.called", { tool })`` = ``Called `{{tool}}` ``
(`packages/ui/src/i18n/en.ts:170`).

Default-open policy:
```
packages/session-ui/src/components/part-default-open.ts:19
export function partDefaultOpen(part, shell = false, edit = false) {
  if (part.type !== "tool") return
  if (part.tool === "bash" || part.tool === "shell") return shell
  if (part.tool === "edit" || part.tool === "write" || part.tool === "patch" || part.tool === "apply_patch") {
    if (!edit) return false
    return !deletionOnly(part)                 // pure-deletion diffs stay collapsed
  }
}
```

### 1.0b Shared TUI row chassis: `InlineTool` / `BlockTool`

`InlineToolRow` (`routes/session/index.tsx:1914`) is the one-line grammar:

```
packages/tui/src/routes/session/index.tsx:1949
      <Switch>
        <Match when={props.spinner}><Spinner color={props.color} children={props.children} /></Match>
        <Match when={true}>
          <Show fallback={<text paddingLeft={3} ...>~ {props.pending}</text>}
                when={props.complete || props.failed}>
            <box flexDirection="row">
              <text width={INLINE_TOOL_ICON_WIDTH} ...>{props.icon}</text>
              <text flexGrow={1} ...>{props.failed && !props.complete ? (props.failure ?? props.children) : props.children}</text>
```

- Pending row = `~ {pending verb}` at `paddingLeft={3}` (plus the row's own `paddingLeft={3}`).
- Done/failed row = 2-cell icon column (`const INLINE_TOOL_ICON_WIDTH = 2`, :1584) + body text.
- `spinner` replaces the icon column with the braille spinner and the body as its label.
- Denied rows are struck through: `attributes={props.denied ? TextAttributes.STRIKETHROUGH : undefined}` (:1959).
- Failed rows are clickable and expand the raw error underneath (`:1985`
  `<Show when={props.failed && props.errorExpanded}> <text fg={props.errorColor}>{props.error}</text>`).

Row colour precedence (`:1874`):
```
    if (props.color) return props.color
    if (permission()) return theme.warning      // this call is the one awaiting permission
    if (failed()) return theme.error
    if (hover() && props.onClick) return theme.text
    if (props.complete) return theme.textMuted
    return theme.text
```
"denied" is string-sniffed (`:1864`): error includes `QuestionRejectedError`, `rejected permission`,
`specified a rule` or `user dismissed`.

`BlockTool` (:1994) is the multi-line card: left split border, `paddingTop/Bottom 1`,
`paddingLeft 2`, `marginTop 1`, `backgroundColor` = `theme.backgroundMenu` on hover else
`theme.backgroundPanel`; optional title line at `paddingLeft={3}` in `theme.textMuted`; trailing
error line in `theme.error`.

TUI argument formatting:
```
packages/tui/src/routes/session/index.tsx:2609
function input(input, omit?) {
  const primitives = ... typeof value === "string" | "number" | "boolean"
  if (primitives.length === 0) return ""
  return `[${primitives.map(([key, value]) => `${key}=${value}`).join(", ")}]`
}
```

### 1.1 Context group (read / glob / grep / list) — web only

```
packages/session-ui/src/components/message-part.tsx:607
const CONTEXT_GROUP_TOOLS = new Set(["read", "glob", "grep", "list"])
```
Consecutive parts of those tools are folded into one `PartGroup` of type `"context"`
(`groupParts`, :663) and rendered by `ContextToolGroup` (:1043):

- Trigger label: `ToolStatusTitle` with `activeText = i18n.t("ui.sessionTurn.status.gatheringContext")`
  = **"Exploring"**, `doneText = ...gatheredContext` = **"Explored"**, `split={false}` (:1079).
- Summary: `AnimatedCountList` of `{read, search, list}` counts (:1090) where
  `search = glob + grep` (`contextToolSummary`, :899). Plural strings
  `"{{count}} read"/"reads"`, `"{{count}} search"/"searches"`, `"{{count}} list"/"lists"`
  (`packages/ui/src/i18n/en.ts:105-110`).
- Group is pending if any child is pending/running or the turn is working (:1053).
- Expanded body lists each child using the bare `basic-tool-tool-*` slots (no arrow, no body)
  with per-tool trigger text from `contextToolTrigger` (:847):

| tool | title | subtitle | args |
|---|---|---|---|
| read | `Read` | `getFilename(filePath)` | `offset=N`, `limit=N` |
| list | `List` | `getDirectory(path)` | — |
| glob | `Glob` | `getDirectory(path)` | `pattern=…` |
| grep | `Grep` | `getDirectory(path)` | `pattern=…`, `include=…` |

### 1.2 `text` part

**Web** (`message-part.tsx:1654`):
```
      <div data-component="text-part" data-timeline-part-id={part().id}>
        <div data-slot="text-part-body">
          <PacedMarkdown text={text()} cacheKey={part().id} streaming={streaming()} />
```
- `streaming` = assistant message with no `time.completed` (:1704).
- Text source: `readPartText(data.store.part_text_accum_delta, part())` — accumulated delta or
  `part.text`, trimmed (`message-part-text.ts:1`).
- A copy button + meta line render only for the "last text part" (or the explicitly requested id):
  `showCopy` (:1714). Meta line (:1692) is
  `[Agent titlecased, model name, duration, "Interrupted"].filter(Boolean).join(" · ")` where
  duration is `{{count}}s` under 60s else `{{minutes}}m {{seconds}}s`
  (`en.ts:203-205`), and `Interrupted` appears when
  `message.error?.name === "MessageAbortedError"` (:1659).

**TUI** (`routes/session/index.tsx:1686`):
```
      <box ... paddingLeft={3} marginTop={1} flexShrink={0}>
        <markdown syntaxStyle={syntax()} streaming={true} internalBlockMode="top-level"
          content={props.part.text.trim()} tableOptions={{ style: "grid" }}
          conceal={ctx.conceal()} fg={theme.markdownText} bg={theme.background} />
```

### 1.3 `reasoning` part

**Web** (`message-part.tsx:1759`): plain `PacedMarkdown` inside
`<div data-component="reasoning-part">`; no header, no fold. Hidden entirely when
`showReasoningSummaries` is false (`renderable`, :718).

**TUI** (`routes/session/index.tsx:1586`) is richer:
- `[REDACTED]` is stripped: `props.part.text.replace("[REDACTED]", "").trim()` (:1595).
- `opaque` = no content but metadata present → "encrypted" case (:1597).
- Done when `props.part.time.end !== undefined` (:1600) — independent of the message finishing.
- Collapsed single-line in `thinkingMode() === "hide"`; click toggles (:1609).
- Header grammar (`ReasoningHeader`, :1651):
```
  const completed = () => {
    if (props.encrypted) return `Thought${props.duration ? ` · ${props.duration}` : ""}`
    const detail = [props.title, props.duration].filter(Boolean).join(" · ")
    return `${props.toggleable ? (props.open ? "- " : "+ ") : ""}Thought${detail ? `: ${detail}` : ""}`
  }
```
  While running: `<Spinner color={fg()}>{props.title ? "Thinking: " + props.title : "Thinking"}</Spinner>` (:1674).
- Open colour is `theme.warning` faded by `theme.thinkingOpacity`; closed is full `theme.warning` (:1660).
- Body is rendered as markdown with `generateSubtleSyntax(theme)` in `theme.textMuted` (:1635).

### 1.4 `compaction` part

Web: `PART_MAPPING["compaction"]` → `MessageDivider` with label `i18n.t("ui.messagePart.compaction")`
= **"Session compacted"** (`en.ts:104`).
```
packages/session-ui/src/components/message-part.tsx:1635
      <div data-component="compaction-part">
        <div data-slot="compaction-part-divider">
          <span data-slot="compaction-part-line" />
          <span data-slot="compaction-part-label" class="text-12-regular text-text-weak">{props.label}</span>
          <span data-slot="compaction-part-line" />
```
The same divider carries the **interrupted** label for a turn (`session-turn.tsx:293`):
`if (compaction()) return "Session compacted"; if (interrupted()) return "Interrupted"`.

TUI (`index.tsx:1456`): a top border box with centred title `" Compaction "` in `theme.borderActive`.

### 1.5 `file` / attachment parts (user message)

Classification (`packages/session-ui/src/components/message-file.ts`):
```
:5  export function attached(part) { return part.url.startsWith("data:") && !inline(part) }
:9  export function inline(part)   { return part.source?.text?.start !== undefined && part.source?.text?.end !== undefined }
:13 export function kind(part)     { return part.mime.startsWith("image/") ? "image" : "file" }
:26 export function typeLabel(filename, mime, fallback) {
      if (mime === "application/pdf") return "PDF"
      ... extension → shiki language display name, else EXT.toUpperCase(), else fallback
```
Attachments render in `data-slot="user-message-attachments"` (`message-part.tsx:1264`); images are
clickable and open `ImagePreview` in a dialog (:1281, :1237); files show `FileIcon` + name, or, in
the new layout, an `AttachmentCardV2` whose body is `typeLabel(...)` (:1299).

Inline file references and agent mentions are highlighted in place by offsets:
```
packages/session-ui/src/components/message-part.tsx:1430
  return <For each={segments()}>{(segment) => <span data-highlight={segment.type}>{segment.text}</span>}</For>
```
(`type` is `"file" | "agent"`, from `source.text.start/end` and `agent.source.start/end`, :1400).

TUI attachment chips (`index.tsx:1420`):
```
                      <text fg={theme.text}>
                        <span style={{ bg: theme.secondary, fg: theme.background }}>
                          {directory ? " Directory " : " File "}
                        </span>
                        <span style={{ bg: theme.backgroundElement, fg: theme.textMuted }}> {file.filename} </span>
```

### 1.6 `read`

| | Web | TUI |
|---|---|---|
| glyph | icon `glasses` (`message-part.tsx:477`, :1794) | `→` (`index.tsx:2163`) |
| verb | `Read` (`ui.tool.read`) | `Read` |
| argument | `getFilename(input.filePath)` as subtitle | `pathFormatter.format(filePath)` then `input(props.input, ["filePath"])` |
| args | `offset=N`, `limit=N` (:1782) | included in the `[k=v, …]` bracket |
| pending | title shimmers; subtitle hidden while pending | `~ Reading file…`; spinner while `status === "running"` |
| extra | per loaded file: `<div data-component="tool-loaded-file"><Icon name="enter"/> Loaded <path></div>` (:1803) | `↳ Loaded {path}` in `theme.textMuted` (:2176) |

`loaded` only shows when completed (web :1784); TUI additionally skips it when
`props.part.state.time.compacted` (:2156).

### 1.7 `list` / `glob` / `grep`

Web (`message-part.tsx:1816/1842/1872`): `BasicTool` with icons `bullet-list` /
`magnifying-glass-menu` / `magnifying-glass-menu`; subtitle `getDirectory(input.path || "/")`;
args as in §1.1; body is the raw `output` rendered as Markdown inside a scrollable region:
```
          <div data-component="tool-output" data-scrollable tabIndex={0} role="region"
               aria-label={i18n.t("ui.scrollView.ariaLabel")}>
            <Markdown text={props.output!} />
```
CSS caps that region: `max-height: 240px; overflow-y: auto` (`message-part.css:356`, and again :398).

TUI has no `list` renderer (falls to generic). `Glob` (:2137) and `Grep` (:2185):
```
    <InlineTool icon="✱" pending="Finding files…"  ...>Glob "{pattern}" <Show…>in {path} </Show><Show…>({count} match|matches)</Show>
    <InlineTool icon="✱" pending="Searching content…" ...>Grep "{pattern}" <Show…>in {path} </Show><Show…>({matches} match|matches)</Show>
```
Singular/plural: `{count} === 1 ? "match" : "matches"`.

### 1.8 `bash` / `shell`

**Web** (`message-part.tsx:2085`), icon `console`, title `Shell` (`ui.tool.shell`),
`allowOpenWhilePending` (so a running command can be opened):
- Collapsed row shows the command as a `ShellSubmessage` (:2118) which animates in
  (`width: 0 → auto`, spring `visualDuration: 0.25, bounce: 0`; value `opacity 0→1`,
  `blur(2px)→blur(0)`, `duration: 0.32, ease: [0.16, 1, 0.3, 1]` — :102-106).
- Body text is built once, verbatim, and copyable:
```
packages/session-ui/src/components/message-part.tsx:2091
    const text = createMemo(() => {
      const cmd = props.input.command ?? props.metadata.command ?? ""
      const out = stripAnsi(props.output || props.metadata.output || "").replace(/\r\n?/g, "\n")
      return `$ ${cmd}${out ? "\n\n" + out : ""}`
    })
```
  rendered as `<pre data-slot="bash-pre"><code>{text()}</code></pre>` inside
  `data-slot="bash-scroll" data-scrollable` (:2138) with a copy `IconButtonV2` at
  `data-slot="bash-copy"` (:2126). Copy feedback resets after `2000` ms (:2103).

**TUI** (`index.tsx:2046`):
- If `metadata.output !== undefined` → `BlockTool`; title only when a non-`.` workdir exists:
  `` `# Running in ${wd}` `` (:2072).
- Command line: `$ {command}` when done; `<Spinner color={theme.text}>{command}</Spinner>` while running (:2084).
- Output truncation: `maxLines = 10`, `maxChars = maxLines * Math.max(20, ctx.width - 6)` (:2053);
  overflow adds a click toggle line `"Click to expand"` / `"Click to collapse"` (:2091).
- Otherwise inline: `<InlineTool icon="$" pending="Writing command…" complete={command}>{command}</InlineTool>` (:2097).

Truncation helper:
```
packages/tui/src/util/collapse-tool-output.ts:1
export function collapseToolOutput(output, maxLines, maxChars) {
  ... preview = lines.slice(0, maxLines).join("\n")
  if (chars(preview) > maxChars) return { output: chars.slice(0, maxChars-1) + "…", overflow: true }
  return { output: [...lines.slice(0, maxLines), "…"].join("\n"), overflow: true }
}
```

### 1.9 `edit`

**Web** (`message-part.tsx:2155`), icon `code-lines`, `defer` on by default:
```
            <div data-component="edit-trigger">
              data-slot="message-part-title-area"
                data-slot="message-part-title"
                  data-slot="message-part-title-text"      ← TextShimmer "Edit"  (ui.messagePart.title.edit)
                  data-slot="message-part-title-filename"  ← getFilename(filePath), hidden while pending
                data-slot="message-part-path"              ← getDirectory(filePath), only if path contains "/"
              data-slot="message-part-actions"             ← <DiffChanges changes={metadata.filediff}/>, only when !pending
```
Body: a sticky-header accordion per file (`ToolFileAccordion`, :1498) containing the diff view:
```
packages/session-ui/src/components/message-part.tsx:2182
      const fileCompProps = createMemo(() => {
        const source = diffSource()        // {file, patch, before, after} from metadata.filediff
        if (source) { const fileDiff = resolveFileDiff(source)
          if (fileDiff) return { fileDiff, hunkSeparators: fileDiff.isPartial ? "simple" : "line-info-basic" } }
        return { before: {name, contents: filediff.before || input.oldString || ""},
                 after:  {name, contents: filediff.after  || input.newString || ""} }
      })
```
rendered `mode="diff"` with `virtualize={props.virtualizeDiff}` (:2244). Diagnostics follow (§1.16).
Accordion header grammar (:1511): file icon, `‪{directory}‬` (LTR-isolated) + filename,
actions, then `<Icon name="chevron-grabber-vertical" size="small"/>`.

**TUI** (`index.tsx:2390`):
- With `metadata.diff`: `BlockTool` titled `"← Edit " + formattedPath` (:2409) containing the
  `<diff>` renderable, `showLineNumbers={true}`, `wrapMode={ctx.diffWrapMode()}` and the full
  diff colour set (`diffAddedBg`, `diffRemovedBg`, `diffContextBg`, `diffHighlightAdded`,
  `diffHighlightRemoved`, `diffLineNumber`, `diffAddedLineNumberBg`, `diffRemovedLineNumberBg`).
- View mode (:2395): `diff_style === "stacked"` → `unified`, else `ctx.width > 120 ? "split" : "unified"`.
- Without a diff: `<InlineTool icon="←" pending="Preparing edit…">Edit {path} {input({replaceAll})}</InlineTool>` (:2435).

### 1.10 `write`

Web (:2261): identical trigger grammar to `edit` but title `Write`
(`ui.messagePart.title.write`) and no `DiffChanges` (`{/* <DiffChanges diff={diff} /> */}`, :2293);
body is the file content rendered `mode="text"` with `overflow="scroll"` and
`cacheKey: checksum(content)` (:2300).

TUI (:2105): with diagnostics present → `BlockTool` titled `"# Wrote " + path`, body is the content in
a `<line_number minWidth={3} paddingRight={1}>` wrapper; else
`<InlineTool icon="←" pending="Preparing write…">Write {path}</InlineTool>` (:2129).

### 1.11 `patch` / `apply_patch`

**Web** (`message-part.tsx:2321`), icon `code-lines`:
- Files parsed by `patchFiles(props.metadata.files)`; all non-delete files start expanded:
  `setExpanded(list.filter((f) => f.type !== "delete").map((f) => f.filePath))` (:2341).
- Multi-file trigger subtitle: `` `${count} ${count > 1 ? "files" : "file"}` `` (:2344).
- Single-file case reuses the `edit-trigger` grammar with title `Patch` and the file name (:2458).
- Per-file action badge (:2404):

| `file.type` | badge | `data-type` |
|---|---|---|
| `add` | `Created` (`ui.patch.action.created`) | `added` |
| `delete` | `Deleted` | `removed` |
| `move` | `Moved` | `modified` |
| otherwise | `<DiffChanges changes={{additions, deletions}} />` | — |

- Diff body per file uses `hunkSeparators={file.view.fileDiff.isPartial ? "simple" : "line-info-basic"}` (:2437),
  mounted one frame after the accordion opens (`requestAnimationFrame`, :2383).

**TUI** (`index.tsx:2443`), one `BlockTool` per file with title from:
```
    function title(file) {
      if (file.type === "delete") return "# Deleted " + file.relativePath
      if (file.type === "add")    return "# Created " + file.relativePath
      if (file.type === "move")   return "# Moved " + pathFormatter.format(file.filePath) + " → " + file.relativePath
      return "← Patched " + file.relativePath
    }
```
Deletes render only `-{deletions} line|lines` in `theme.diffRemoved` (:2498); everything else renders
the diff plus diagnostics. Fallback row:
`<InlineTool icon="%" pending="Preparing patch…" failure="Patch failed" complete={false}>Patch</InlineTool>` (:2511).

### 1.12 `webfetch` / `websearch`

`webfetch` web (:1905): icon `window-cursor`, `hideDetails` (never expandable), title `Webfetch`;
when not pending the URL is an `<a target="_blank" rel="noopener noreferrer">` styled as the subtitle
plus a `square-arrow-top-right` action icon (:1926-1943).
TUI (:2198): `<InlineTool icon="%" pending="Fetching from the web…">WebFetch {url}</InlineTool>`.

`websearch` web (:1951): icon `window-cursor`; title from
```
packages/session-ui/src/components/message-part.tsx:463
function webSearchProviderLabel(provider, i18n) {
  const name = provider === "parallel" ? "Parallel" : provider === "exa" ? "Exa" : undefined
  if (name) return i18n.t("ui.tool.websearch.provider", { provider: name })   // "{{provider}} Web Search"
  return i18n.t("ui.tool.websearch")                                          // "Web Search"
}
```
subtitle = query with `subtitleClass: "exa-tool-query"`; body = `ExaOutput`, which extracts URLs from
the output and renders them as a link list:
```
packages/session-ui/src/components/message-part.tsx:574
function urls(text) {
  ... [...text.matchAll(/https?:\/\/[^\s<>"'`)\]]+/g)]
    .map((item) => item[0].replace(/[),.;:!?]+$/g, ""))   // strip trailing punctuation, dedupe
```
TUI (:2206): `<InlineTool icon="◈" pending="Searching web…">{providerLabel} "{query}" ({numResults} results)</InlineTool>`.

### 1.13 `todowrite`

Web: hidden from the timeline (§0) but the registered renderer (`message-part.tsx:2525`) is
`defaultOpen`, icon `checklist`, title `To-dos` (`ui.tool.todos`), subtitle
`` `${completedCount}/${list.length}` `` (:2539), body a list of read-only `Checkbox`es with
`data-completed` on completed rows (:2559).

TUI (`index.tsx:2519`): `BlockTool title="# Todos"` listing `TodoItem`s; otherwise
`<InlineTool icon="⚙" pending="Updating todos…" failure="Todo update failed" complete={false}>Updating todos…</InlineTool>`.
Todo row grammar (`packages/tui/src/component/todo-item.tsx:19`):
```
        [{props.status === "completed" ? "✓" : props.status === "in_progress" ? "•" : " "}]{" "}
```
coloured `theme.warning` when `in_progress`, else `theme.textMuted`.

### 1.14 `question` (the tool part, i.e. the answered record)

Web (:2576): icon `bubble-5`, title `Questions`, `defaultOpen={completed()}` where completed means
`metadata.answers.length > 0`. Subtitle: `{{count}} answered` when completed, else
`` `${count} question|questions` `` (:2584). Body pairs each question with its answer:
```
                    <div data-slot="question-answer-item">
                      <div data-slot="question-text">{q.question}</div>
                      <div data-slot="answer-text">{answer().join(", ") || i18n.t("ui.question.answer.none")}</div>
```
`ui.question.answer.none` = `"(no answer)"`. Pending/running question parts are suppressed
(`hideQuestion`, :1540 — the live prompt renders instead, §4). A dismissal error renders as a right-
aligned muted line `Questions dismissed` (:1577-1584).

TUI (`index.tsx:2539`): `BlockTool title="# Questions"` with muted question / normal answer pairs and
the same `"(no answer)"` fallback; otherwise
`<InlineTool icon="→" pending="Asking questions…">Asked {count} question|questions</InlineTool>`.

### 1.15 `task` (subagent) — see also §4

Web (:1978):
```
      <div data-component="task-tool-card" style={{"--task-agent-color": v2Tone(), "--task-agent-legacy-color": tone()}}>
        <div data-component="task-tool-surface">
          ... running ? <span data-component="task-tool-spinner"> (Spinner or SessionProgressIndicatorV2)
                      : <span data-component="task-tool-icon"><Icon name="subagent" size="small"/></span>
              <span data-component="task-tool-title">{title()}</span>
              <span data-slot="basic-tool-tool-subtitle">{subtitle()}</span>
        <div data-component="task-tool-action"><Icon name="square-arrow-top-right" size="small"/></div>   // when clickable
```
- `title` = resolved agent name, else `Agent` (`ui.tool.agent.default`); `getToolInfo` uses
  `"{{type}} Agent"` (`ui.tool.agent`) for the title elsewhere (:512).
- `subtitle` = `input.description || childSessionId`, with `` `${value} (background)` `` when
  `metadata.background === true` (:1992).
- Row is a link (`triggerAsLink`, `triggerHref`) to the child session; plain left-click without
  modifiers navigates in-app (:2012), Enter/Space activates when there is no href (:2018).
- Agent colour resolution: explicit agent colour → theme colour map → per-name tone table → hashed
  palette (`taskAgent` :437, `tone` :431, `agentPalette` :416).

TUI (`index.tsx:2215`) builds a multi-line inline row:
```
:2293  icon={props.part.state.status === "completed" ? "✓" : "│"}   separate={true}   spinner={isRunning()}
:2262  content = [ formatSubagentTitle(titlecase(subagent_type ?? "General"), description, background) ]
         + running & retrying → `↳ ${formatSubagentRetry(attempt, truncate(message, 80))}`
         + running & tools>0  → `↳ ${titlecase(currentTool)} ${currentTitle}`  or  `↳ ${formatSubagentToolcalls(n)}`
         + completed          → `↳ ${formatCompletedSubagentDetail(toolcalls, duration)}`
:2313  formatSubagentToolcalls(count)       = `${count} toolcall${count === 1 ? "" : "s"}`
:2317  formatSubagentTitle(agent, desc, bg) = `${agent} Task${bg ? " (background)" : ""} — ${desc}`
:2321  formatSubagentRetry(attempt, msg)    = `Retrying (attempt ${attempt}) · ${msg}`
:2325  formatCompletedSubagentDetail(n, d)  = n === 0 ? d : `${formatSubagentToolcalls(n)} · ${d}`
```
Duration is child-session wall time: last assistant `time.completed` − first user `time.created` (:2255).
Clicking navigates into the child session; a retry status opens a `DialogAlert("Retry Error", …)` (:2300).

### 1.16 `skill`

Web (:2621): icon `brain`, `hideDetails`, title `input.name || "Skill"` with class
`"capitalize agent-title"`.
TUI (:2575): `<InlineTool icon="→" pending="Loading skill…">Skill "{name}"</InlineTool>`.

### 1.17 `execute` (TUI only)

`index.tsx:2344` — child tool calls stream through `metadata.toolCalls`:
```
:2353  const lines = ["execute"]; for (const call of calls()) lines.push(`↳ ${call.tool}${args ? ` ${args}` : ""}${call.status === "error" ? " (failed)" : ""}`)
:2365  icon={hasRuntimeError() ? "✗" : status === "completed" ? "✓" : "│"}
:2351  outputPreview = collapseToolOutput(output, 4, 4 * Math.max(20, ctx.width - 6)).output   // only shown on runtime error
```

### 1.18 Generic / MCP tool

Web: `GenericTool` (`basic-tool.tsx:323`), icon `mcp`, title ``Called `{{tool}}` ``, subtitle from
`label(input)`, up to three `k=v` args.
TUI (`index.tsx:1798`): default is inline
`<InlineTool icon="⚙" pending="Writing command…" complete={true}>{tool} {input(input)}</InlineTool>`;
when `showGenericToolOutput()` is on and output exists, a `BlockTool` titled
`` `# ${tool} ${input(props.input)}` `` with `maxLines = 3` truncation and click-to-expand (:1803-1830).

### 1.19 Tool error rendering (web)

Any tool part whose `state.status === "error"` short-circuits to `ToolErrorCard`
(`message-part.tsx:1574`, component `tool-error-card.tsx:22`):
- Card `variant="error"`, indicator icon `circle-ban-sign` with `stroke-width: 1.5` (:104).
- Title = friendly tool name from a fixed map (:47) else the raw tool name.
- Message parsing (:66-87): strip leading `Error:`; strip a leading `"{tool} "` prefix; the part
  before the first `": "` (capitalised) becomes the **subtitle**, defaulting to `Failed`
  (`ui.toolErrorCard.failed`); the remainder becomes the collapsible **body**.
- Body carries a copy button (`Copy error`), 2 s "Copied" feedback (:89).

### 1.20 Diagnostics (both)

Web (`message-part.tsx:136`): `diagnostics.filter((d) => d.severity === 1).slice(0, 3)` — errors only,
max 3. Row: `Error` label + `[{line+1}:{char+1}]` + message (:152).
TUI (`index.tsx:2692`, `2583`): the same severity-1 filter and `.slice(0, 3)`, rendered as
`Error [{line+1}:{char+1}] {message}` in `theme.error`.

### 1.21 Turn summary diffs (web)

`session-turn.tsx:257` — `MAX_FILES = 10`; the rest collapse behind
`+{{count}} more files` (`ui.sessionTurn.diffs.more`) and a `Show all` / `Show less` toggle
(:447, :520). Header label is the plural `{{count}} Changed file(s)` plus `<DiffChanges>`.
Each file row: LTR-isolated directory + filename, change counts, chevron; the diff view mounts one
frame after expansion (:475).

---

## 2. Status, footer, queued messages, interrupt

### 2.1 TUI prompt footer (the live status bar)

`packages/tui/src/component/prompt/index.tsx:1513` renders a left/right row:

**Left, when `status().type !== "idle"`** (:1515):
- Spinner: `<spinner color={spinnerDef().color} frames={spinnerDef().frames} interval={40} />`
  (:1525); with animations disabled the fallback is a static `[⋯]` (:1524). Frames/colours come from
  `createFrames/createColors({ color, style: "blocks", inactiveFactor: 0.6, minAlpha: 0.3 })`
  (:1329-1343) where `color` is the agent colour of the message being generated.
- Retry text (:1567):
```
                      const retryText = () => {
                        const baseMessage = message()
                        const truncatedHint = isTruncated() ? " (click to expand)" : ""
                        const duration = formatDuration(seconds())
                        const retryInfo = ` [retrying ${duration ? `in ${duration} ` : ""}attempt #${r.attempt}]`
                        return baseMessage + truncatedHint + retryInfo
                      }
```
  with `message()` truncated at 80 chars + `…`, the special case
  `"gemini is way too hot right now"` when the message mentions quota+gemini (:1538), and
  `isTruncated` at >120 chars opening a `DialogAlert("Retry Error", …)` on click (:1559).
- Interrupt hint, right-aligned within the row (:1587):
```
                <text fg={store.interrupt > 0 ? theme.primary : theme.text}>
                  esc{" "}
                  <span style={{ fg: store.interrupt > 0 ? theme.primary : theme.textMuted }}>
                    {store.interrupt > 0 ? "again to interrupt" : "interrupt"}
                  </span>
```
  i.e. **esc interrupt** → after one press **esc again to interrupt**; the counter resets on a timer
  and fires at `store.interrupt >= 2` (:408-418).

**Left, when idle**: workspace notice / workspace label / move progress / `(new working copy)` /
otherwise the session directory (:1595-1653).

**Right** (hidden while `status().type === "retry"`, :1655): optional editor-context file label, then
either usage or the agents hint, then the commands hint:
```
:1665   <Match when={usage()}>{(item) => <text ...>{[item().context, item().cost].filter(Boolean).join(" · ")}</text>}
:1672   <Match when={true}> {agentShortcut()} <span …>agents</span>
:1678   {paletteShortcut()} <span …>commands</span>
:1682   (shell mode) esc <span …>exit shell mode</span>
```
Usage numbers (`prompt/index.tsx:268`, identical in `subagent-footer.tsx:33`):
```
    const tokens = last.tokens.input + last.tokens.output + last.tokens.reasoning
                 + last.tokens.cache.read + last.tokens.cache.write
    const pct = model?.limit.context ? `${Math.round((tokens / model.limit.context) * 100)}%` : undefined
    context: pct ? `${Locale.number(tokens)} (${pct})` : Locale.number(tokens)
    cost: cost > 0 ? money.format(cost) : undefined     // Intl.NumberFormat("en-US", {style:"currency", currency:"USD"})
```
Placeholders: `` `Ask anything… "${example}"` `` / `` `Run a command… "${example}"` `` (:1316-1319).

### 2.2 TUI app footer (LSP / MCP / permissions / cwd)

`packages/tui/src/routes/session/footer.tsx:52`: left = `directory()` in `theme.textMuted`;
right group:
- permissions (only when >0): `△ {n} Permission(s)` in `theme.warning` (:64)
- `• {n} LSP` — bullet green when `lsp().length > 0` else muted (:69)
- `⊙ {n} MCP` — `⊙` red when any MCP failed, else green; only shown when count > 0 (:72)
- `/status` in muted text (:85)
- Disconnected state alternates a `Get started /connect` message on a 5 s / 10 s cycle (:27-45).

### 2.3 Queued user messages (TUI)

`routes/session/index.tsx:1387`: `queued = props.pending !== undefined && props.index > props.pending`
where `pending()` is the index of the last incomplete assistant message (:243). A queued user message
renders a badge instead of its timestamp (:1449):
```
              <text fg={theme.textMuted}>
                <span style={{ bg: color(), fg: queuedFg(), bold: true }}> QUEUED </span>
              </text>
```
`color()` is the agent colour; `queuedFg()` is `selectedForeground(theme, color())`. When not queued
and timestamps are on, the same slot shows `Locale.todayTimeOrDateTime(message.time.created)` (:1443).

### 2.4 Queued messages (desktop follow-up dock)

`packages/app/src/pages/session/composer/session-followup-dock.tsx:24` — a `DockTray` above the
composer: summary label `language.plural("session.followupDock.summary", n)`, the first item's text as
a truncated preview when collapsed, a rotating `chevron-down` toggle (`rotate(180deg)` when
collapsed), and, expanded, one row per queued item with **Send now** and **Edit** buttons
(`max-h-42 overflow-y-auto no-scrollbar`, :75).

### 2.5 Retry card (web)

`packages/session-ui/src/components/session-retry.tsx:53` — an error `Card` with a `Spinner`
(`size-4 mt-0.5`), the message (truncated at 80 chars + `...`, tooltip carries the full text), and an
info line built from (:42):
```
    const delay = count > 0 ? i18n.t("ui.sessionTurn.retry.inSeconds", { seconds: count }) : ""   // "in {{seconds}}s"
    const line = ["retrying", delay].filter(Boolean).join(" ")
    return i18n.t("ui.sessionTurn.retry.attemptLine", { line, attempt })                          // "{{line}} - attempt #{{attempt}}"
```
The countdown ticks every 1000 ms (:24).

### 2.6 Thinking placeholder (web)

```
packages/session-ui/src/components/session-turn.tsx:422
              <Show when={showThinking()}>
                <div data-slot="session-turn-thinking">
                  <TextShimmer text={i18n.t("ui.sessionTurn.status.thinking")} />     // "Thinking"
                  <Show when={!showReasoningSummaries()}>
                    <TextReveal text={reasoningHeading()} class="session-turn-thinking-heading" travel={25} duration={700} />
```
`showThinking` (:372): only while working, no error, status is not `retry`, and — when reasoning
summaries are shown — only while no assistant part is visible yet. `reasoningHeading` is the first
heading extracted from reasoning text by `heading()` (:125: HTML `<h1-6>`, ATX `#`, setext, then bold
line), stripped of markdown by `clean()` (:117).

Other status verbs exist in the catalogue but only `gatheringContext`/`gatheredContext`/`thinking`
are wired in this tree (`packages/ui/src/i18n/en.ts:85-96`): `Delegating work`,
`Planning next steps`, `Exploring`, `Explored`, `Searching the codebase`, `Searching the web`,
`Making edits`, `Running commands`, `Thinking`, `Thinking - {{topic}}`, `Gathering thoughts`,
`Considering next steps`.

---

## 3. User message block, assistant text, markdown

### 3.1 User message (web)

`packages/session-ui/src/components/message-part.tsx:1315`:
```
    <div data-component="user-message" data-timeline-part-id={textPart()?.id}>
      attachments (before body in legacy, after body when useV2Actions)
      data-slot="user-message-body" > data-slot="user-message-text" dir="auto"
          <HighlightedText text references agents />
          <UserMessageComments … bounded />         // first 5, then a "Show more" button (:1172)
      data-slot="user-message-copy-wrapper"
          data-slot="user-message-meta"      ← Agent · Model        (NBSP · NBSP separator, :1232)
          data-slot="user-message-meta-sep"  ← " · "
          data-slot="user-message-meta-tail" ← short time (Intl.DateTimeFormat timeStyle:"short", :1221)
          revert button (icon "reset", label "Revert message")
          copy button  (icon "copy"/"check", label "Copy message"/"Copied")
```
Only the first non-synthetic text part is shown (:1198). Copy feedback resets after 2000 ms (:1246).

### 3.2 User message (TUI)

`routes/session/index.tsx:1394`: a left split border in the agent colour, body
`paddingTop/Bottom 1`, `paddingLeft 2`, background `theme.backgroundElement` on hover else
`theme.backgroundPanel`, `marginTop` 1 except for the first message. Text is the join of all
non-synthetic text parts with `"\n\n"` (:1373). Then attachment chips, then the QUEUED badge or
timestamp (§2.3).

### 3.3 Assistant meta footer (TUI)

`routes/session/index.tsx:1548` — rendered on the last message, on a real finish, or on abort:
```
              <span style={{ fg: aborted ? theme.textMuted : local.agent.color(props.message.agent) }}>▣{" "}</span>{" "}
              <span style={{ fg: theme.text }}>{Locale.titlecase(props.message.mode)}</span>
              <span style={{ fg: theme.textMuted }}> · {model()}</span>
              …> · {Locale.duration(duration())}</span>
              …> · interrupted</span>
```
`final()` = a `finish` reason outside `["tool-calls","unknown"]` (:1477); `duration` is
`message.time.completed − parentUserMessage.time.created` (:1481).

### 3.4 Markdown rules (web)

- Parsing and highlighting run in a worker (`markdown.worker.ts`): `createMarkdownParser` over
  `marked`, Shiki highlighter with the single theme `OpenCodeTheme`
  (`markdown.worker.ts:144`: `createHighlighter({ themes: [OpenCodeTheme], langs: [] })`); unknown
  languages fall back to `"text"` (:39, :89). Streaming code uses `ShikiStreamTokenizer` and returns
  `stable` + `unstable` token runs (:112-132); completed code is re-tokenised whole with
  `reset: true` (:93).
- Sanitisation is DOMPurify with an explicit profile:
```
packages/session-ui/src/components/markdown-cache.tsx:13
const config = {
  USE_PROFILES: { html: true, mathMl: true },
  SANITIZE_NAMED_PROPS: true,
  FORBID_TAGS: ["style"],
  FORBID_CONTENTS: ["style", "script"],
  ADD_TAGS: ["svg", "path"],
  ADD_ATTR: ["d", "viewBox", "preserveAspectRatio", "xmlns", "target"],
}
```
  plus an `afterSanitizeAttributes` hook forcing `rel="noopener noreferrer"` on `target="_blank"`
  anchors (:23). Rendered HTML is cached LRU at `const max = 200` entries (:11).
- Fallback when parsing fails: HTML-escaped text with `\n` → `<br>` (`markdown.tsx:66`).
- Blocks are diffed with `morphdom` and keyed by `data-markdown-key` / `data-markdown-hash`
  (`markdown.tsx:589`); copy buttons are preserved across morphs (:616).
- Every `<pre>` is wrapped:
```
packages/session-ui/src/components/markdown.tsx:208
    wrapper.setAttribute("data-component", "markdown-code")
    ... wrapper.appendChild(createCopyButton(labels))
```
  with `data-language` and `data-code-kind="shell"` for
  `bash|sh|shell|zsh|fish|console|terminal` (:179-206). `<pre class="shiki OpenCode">` and
  `<code class="language-{lang}">` (:685-687).
- Inline code (new layout only): tagged with `data-inline-code-kind` from `inlineCodeKind()` (:268);
  inline code whose entire content is a bare URL becomes an `<a class="external-link" target="_blank">`
  (:239-265, matcher `/^https?:\/\/[^\s<>()`"']+$/` at :103).
- Code copy feedback resets after `2000` ms (:313).
- Streaming text is paced, not dumped:
```
packages/session-ui/src/components/message-part.tsx:252
const TEXT_RENDER_PACE_MS = 24
const TEXT_RENDER_IMMEDIATE = 512
const TEXT_RENDER_SNAP = /[\s.,!?;:)\]]/
function step(size) { if (size <= 12) return 2; if (size <= 48) return 4; if (size <= 96) return 8; return Math.min(256, Math.ceil(size / 4)) }
```
  `next()` (:263) extends a step to the next snap character within 8 chars, so chunks land on word
  boundaries; a jump larger than 512 chars is applied immediately (:299).

Markdown typography (`packages/session-ui/src/components/markdown.css`): root
`font-family: var(--font-family-sans); font-size: var(--font-size-base) /*14px*/; line-height: 160%`
(:15-17); headings 17px / 15px / 13px / 13px with `line-height: var(--line-height-large)` (:35-69);
code font `var(--font-family-mono)` (:244); code block scrolls `overflow-x: auto` (:277).

### 3.5 Markdown (TUI)

The `<markdown>` renderable with `internalBlockMode="top-level"`, `tableOptions={{ style: "grid" }}`,
`conceal={ctx.conceal()}` and the theme's syntax style (`routes/session/index.tsx:1692`).
Conceal is toggled by `messages_toggle_conceal` (`<leader>h`).

---

## 4. Subagents, permission and question prompts

### 4.1 Subagent affordances

- Web task card: §1.15 — title, description, spinner while running, `square-arrow-top-right` as the
  "view" affordance, whole row links to the child session.
- TUI: after any assistant message containing a `task` tool, a hint line
  (`routes/session/index.tsx:1509`):
```
            {childShortcut()}<span style={{ fg: theme.textMuted }}> view subagents</span>
            … · {backgroundShortcut()}<span …> background</span>     // only with experimentalBackgroundSubagents and a running foreground task
```
- Inside a child session the TUI replaces the prompt with `SubagentFooter`
  (`routes/session/subagent-footer.tsx:65`): left = **label** (`@(\w+) subagent` from the title,
  titlecased, else `Subagent`, :20) + `({index} of {total})` + `context · cost`; right = three
  buttons `Parent {shortcut}`, `Prev {shortcut}`, `Next {shortcut}`.

### 4.2 Permission prompt — TUI

`packages/tui/src/routes/session/permission.tsx`. Per-permission bodies (:204-377) give
title + body:

| permission | title | body |
|---|---|---|
| edit | `Edit {path}` | unified/split `<diff>`, or `No diff provided` (:83) |
| read | `Read {path}` | `Path: {path}` |
| glob | `Glob "{pattern}"` | `Pattern: {pattern}` |
| grep | `Grep "{pattern}"` | `Pattern: {pattern}` |
| list | `List {dir}` | `Path: {dir}` |
| bash | `Shell command` | `$ {command}` |
| task | `{Type} Task` | `◉ {description}` |
| webfetch | `WebFetch {url}` | `URL: {url}` |
| websearch | `{provider} "{query}"` | `Query: {query}` |
| external dir | `Access external directory {dir}` | `Patterns` + `- {pattern}` list |
| failures | `Continue after repeated failures` | `This keeps the session running despite repeated failures.` |
| default | `Call tool {permission}` | `Tool: {permission}` |

Header (:386): `△` in `theme.warning` then `Permission required`, and a second indented line with the
specific title. Options (:405): `{ once: "Allow once", always: "Allow always", reject: "Reject" }`,
`escapeKey="reject"`. Choosing `always` advances to a confirmation stage titled `Always allow`
(:140) which explains either `This will allow {permission} until OpenCode is restarted.` or lists the
patterns. Rejecting opens `RejectPrompt` (:443): `△` in `theme.error`, `Reject permission`,
`Tell OpenCode what to do differently`, a textarea, footer `enter` / `esc cancel`.

Chrome and keys (`Prompt`, :525):
- Card: left border in `theme.warning`, `backgroundColor: theme.backgroundPanel`,
  `maxHeight: 15` when inline; fullscreen mode goes `position: "absolute"` over the viewport (:638).
- Option chips: selected chip has `backgroundColor: theme.warning` and
  `selectedForeground(theme, theme.warning)`; hovering selects (:683-690).
- Footer hints: `{fullscreenShortcut} fullscreen|minimize`, `⇆ select`, `enter confirm` (:698-708).
- Keys: `left`/`h` previous, `right`/`l` next, `return` select, `escape` (and `app.exit`) → the
  escape option, `ctrl+f` fullscreen (:567-626, default from `keybind.ts`).

### 4.3 Question prompt — TUI

`packages/tui/src/routes/session/question.tsx`. Card border is `theme.accent` (:292).
- Tab strip (multi-question only, :296): one chip per question showing `q.header`, plus a trailing
  `Confirm` chip. Active chip = `theme.accent` background; answered = `theme.text`; else muted.
- Question line: `{question}{multi ? " (select all that apply)" : ""}` (:358).
- Options (:364): `{i+1}.` gutter, then `[✓]`/`[ ]` prefix in multi mode or the bare label; a
  trailing ` ✓` in single mode when picked; description on the next line at `paddingLeft={3}`.
  Active row has `theme.backgroundElement` background and `theme.secondary` text; picked rows are
  `theme.success`.
- Custom answer row: `Type your own answer` with an inline textarea (`maxHeight 6`,
  placeholder `Type your own answer`) while editing (:400-452).
- Confirm tab (:459): `Review` then `header: value` per question, `(not answered)` in `theme.error`
  when empty.
- Footer (:490): `⇆ tab` (multi only), `↑↓ select` (not on confirm),
  `enter {confirm ? "submit" : multi ? "toggle" : single ? "submit" : "confirm"}`, `esc dismiss`.
- Keys (:229-281): `left`/`h` previous question, `right`/`l`/`tab` next (shift+tab back),
  `up`/`k` and `down`/`j` move selection, number keys `1..n` select answer n, `return` select/submit,
  `escape` reject.

### 4.4 Permission dock — desktop

`packages/app/src/pages/session/composer/session-permission-dock.tsx:22` — a `DockPrompt kind="permission"`:
header `Icon name="warning"` + `notification.permission.title`; optional hint line from
`settings.permissions.tool.{permission}.description`; the request's patterns as `<code>` lines;
footer buttons right-aligned in order **Deny** (`ghost`), **Allow always** (`secondary`),
**Allow once** (`primary`), all disabled while `responding`.

### 4.5 Which prompt is shown (TUI)

`routes/session/index.tsx:232-241`: permissions and questions are gathered across the whole session
family (children), the prompt is hidden while either is outstanding
(`visible = !parentID && permissions.length === 0 && questions.length === 0`), and only the first
permission — or, with none, the first question — is rendered (:1297-1308).

---

## 5. Header / sidebar

### 5.1 TUI sidebar

`packages/tui/src/routes/session/sidebar.tsx:26`: fixed `width={42}`, `backgroundColor: theme.backgroundPanel`,
padding `1/2/1/2`. Contents: bold session title, session id (non-`latest` channels only), a
`WorkspaceLabel` for the workspace, and the share URL when shared. Footer: `• OpenCode {version}`
with the bullet in `theme.success` (:91-97).

Visibility (`routes/session/index.tsx:270`): `wide = dimensions().width > 120`; sidebar auto-shows
when wide, never in a child session; content width is
`dimensions().width - (sidebarVisible() ? 42 : 0) - 4` (:278). On a narrow terminal the sidebar
overlays with a `RGBA.fromInts(0, 0, 0, 70)` scrim (:1344).

### 5.2 Desktop context indicator

`packages/app/src/components/session-context-usage.tsx:104` — a 16 px `ProgressCircle`
(`strokeWidth 2`) filled to `context()?.usage`%, with a tooltip of three rows (:128):
`context.usage.cost` → USD `Intl.NumberFormat`, `context.usage.usage` → `{n}%`,
`context.usage.tokens` → localised token total. Clicking opens the context tab in the review panel
(:87-102). Number formatting helpers (`session-context-format.ts:3`) render `—` for null/undefined.

MCP/LSP lists and `cwd:branch` live in the TUI footer (§2.2) and the desktop header
(`packages/app/src/components/session/session-header.tsx`, 568 lines, not quoted here).

---

## 6. Theme tokens

### 6.1 TUI theme shape

`packages/tui/src/theme/index.ts:36` defines the full token list (all `RGBA` unless noted):

```
primary, secondary, accent, error, warning, success, info,
text, textMuted, selectedListItemText,
background, backgroundPanel, backgroundElement, backgroundMenu,
border, borderActive, borderSubtle,
diffAdded, diffRemoved, diffContext, diffHunkHeader, diffHighlightAdded, diffHighlightRemoved,
diffAddedBg, diffRemovedBg, diffContextBg, diffLineNumber, diffAddedLineNumberBg, diffRemovedLineNumberBg,
markdownText, markdownHeading, markdownLink, markdownLinkText, markdownCode, markdownBlockQuote,
markdownEmph, markdownStrong, markdownHorizontalRule, markdownListItem, markdownListEnumeration,
markdownImage, markdownImageText, markdownCodeBlock,
syntaxComment, syntaxKeyword, syntaxFunction, syntaxVariable, syntaxString, syntaxNumber,
syntaxType, syntaxOperator, syntaxPunctuation,
thinkingOpacity: number
```

Selected-item foreground (`:96`): explicit `selectedListItemText` wins; on a transparent background
the contrast is computed as `0.299r + 0.587g + 0.114b > 0.5 ? black : white`; otherwise the theme
background is used.

34 themes ship under `packages/tui/src/theme/assets/`: aura, ayu, carbonfox, catppuccin(+frappe,
macchiato), cobalt2, cursor, dracula, everforest, flexoki, github, gruvbox, kanagawa, lucent-orng,
material, matrix, mercury, monokai, nightowl, nord, **one-dark**, **opencode**, orng, osaka-jade,
palenight, rosepine, solarized, synthwave84, tokyonight, vercel, vesper, zenburn.

### 6.2 Default theme — `opencode.json`

Defs (`packages/tui/src/theme/assets/opencode.json:4`):

| ref | dark | light |
|---|---|---|
| Step1 (background) | `#0a0a0a` | `#ffffff` |
| Step2 (backgroundPanel) | `#141414` | `#fafafa` |
| Step3 (backgroundElement) | `#1e1e1e` | `#f5f5f5` |
| Step4 | `#282828` | `#ebebeb` |
| Step5 | `#323232` | `#e1e1e1` |
| Step6 (borderSubtle) | `#3c3c3c` | `#d4d4d4` |
| Step7 (border) | `#484848` | `#b8b8b8` |
| Step8 (borderActive) | `#606060` | `#a0a0a0` |
| Step9 (primary) | `#fab283` | `#3b7dd8` |
| Step10 | `#ffc09f` | `#2968c3` |
| Step11 (textMuted) | `#808080` | `#8a8a8a` |
| Step12 (text) | `#eeeeee` | `#1a1a1a` |
| secondary | `#5c9cf5` | `#7b5bb6` |
| accent | `#9d7cd8` | `#d68c27` |
| red (error) | `#e06c75` | `#d1383d` |
| orange (warning) | `#f5a742` | `#d68c27` |
| green (success) | `#7fd88f` | `#3d9a57` |
| cyan (info) | `#56b6c2` | `#318795` |
| yellow | `#e5c07b` | `#b0851f` |

Diff tokens (:104-148): `diffAdded #4fd6be / #1e725c`, `diffRemoved #c53b53 / #c53b53`,
`diffContext #828bb8 / #7086b5`, `diffHunkHeader` same as context,
`diffHighlightAdded #b8db87 / #4db380`, `diffHighlightRemoved #e26a75 / #f52a65`,
`diffAddedBg #20303b / #d5e5d5`, `diffRemovedBg #37222c / #f7d8db`, `diffContextBg` = Step2,
`diffLineNumber #8f8f8f / #595959`, `diffAddedLineNumberBg #1b2b34 / #c5d5c5`,
`diffRemovedLineNumberBg #2d1f26 / #e7c8cb`.

Markdown/syntax mapping (:151-243): heading→accent, link→Step9, linkText→cyan, code→green,
blockQuote→yellow, emph→yellow, strong→orange, horizontalRule→Step11, listItem→Step9,
listEnumeration→cyan, image→Step9, imageText→cyan, codeBlock→Step12; syntaxComment→Step11,
syntaxKeyword→accent, syntaxFunction→Step9, syntaxVariable→red, syntaxString→green,
syntaxNumber→orange, syntaxType→yellow, syntaxOperator→cyan, syntaxPunctuation→Step12.

### 6.3 One Dark / One Light — `one-dark.json`

Defs (:3):
`darkBg #282c34`, `darkBgAlt #21252b`, `darkBgPanel #353b45`, `darkFg #abb2bf`,
`darkFgMuted #5c6370`, `darkPurple #c678dd`, `darkBlue #61afef`, `darkRed #e06c75`,
`darkGreen #98c379`, `darkYellow #e5c07b`, `darkOrange #d19a66`, `darkCyan #56b6c2`;
`lightBg #fafafa`, `lightBgAlt #f0f0f1`, `lightBgPanel #eaeaeb`, `lightFg #383a42`,
`lightFgMuted #a0a1a7`, `lightPurple #a626a4`, `lightBlue #4078f2`, `lightRed #e45649`,
`lightGreen #50a14f`, `lightYellow #c18401`, `lightOrange #986801`, `lightCyan #0184bc`.

Mapping (:29): primary→blue, secondary→purple, accent→cyan, error→red, warning→yellow,
success→green, info→orange, text→fg, textMuted→fgMuted, background→bg, backgroundPanel→bgAlt,
backgroundElement→bgPanel, `border #393f4a / #d1d1d2`, borderActive→blue,
`borderSubtle #2c313a / #e0e0e1`, diffAdded→green, diffRemoved→red, diffContext→fgMuted,
diffHunkHeader→cyan, `diffHighlightAdded #aad482 / #489447`, `diffHighlightRemoved #e8828b / #d65145`,
`diffAddedBg #2c382b / #eafbe9`, `diffRemovedBg #3a2d2f / #fce9e8`, diffContextBg→bgAlt,
`diffLineNumber #9398a2 / #666666`, `diffAddedLineNumberBg #283427 / #e1f3df`,
`diffRemovedLineNumberBg #36292b / #f5e2e1`, markdownHeading→purple, markdownLink→blue,
markdownLinkText→cyan.

### 6.4 Web typography and radii

```
packages/ui/src/styles/theme.css:2
  --font-family-sans: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
  --font-family-mono: …
  --font-size-small: 13px;  --font-size-base: 14px;  --font-size-large: 16px;  --font-size-x-large: 20px;
  --font-weight-regular: 400;  --font-weight-medium: 500;
  --line-height-normal: 130%;  --line-height-large: 150%;  --line-height-x-large: 180%;  --line-height-2x-large: 200%;
  --radius-xs: 0.125rem; --radius-sm: 0.25rem; --radius-md: 0.375rem; --radius-lg: 0.5rem; --radius-xl: 0.625rem;
```
Utility classes used by session components (`packages/ui/src/styles/utilities.css:47-104`):
`text-12-*`, `text-13-regular`, `text-13-medium`, `text-14-regular`, `text-14-medium`, mono variants.
Tool rows: `font-family: var(--font-family-sans); font-size: 14px; line-height: var(--line-height-large)`
(`basic-tool.css:86-90`, `:110-115`, `:156-161`, `:234-238`); the new-layout (v2) variants drop to
`font-size: 13px; line-height: 20px` with `--v2-font-family-sans, "Inter", sans-serif`
(`basic-tool.css:289-305`). Tool output panes cap at `max-height: 240px; overflow-y: auto`
(`message-part.css:356`, `:398-400`); code inside them is 13px mono (`:414-416`).

Web colour tokens referenced by session components are CSS custom properties, not hexes:
`--icon-agent-{ask,build,docs,plan}-base`, `--syntax-{info,success,warning,property,constant}`,
`--text-diff-{add,delete}-base`, `--icon-{info,success,warning,critical}-base`,
`--text-interactive-base`, `--text-base` and the v2 set `--v2-agent-{build,explore,plan,review,writer}-solid`,
`--v2-text-text-{base,muted,accent}`, `--v2-state-fg-{success,warning,danger,info}`,
`--v2-icon-icon-{base,muted,accent}` (`message-part.tsx:381-429`).

---

## 7. Keybinds

Defaults from `packages/tui/src/config/keybind.ts` (`export const LeaderDefault = "ctrl+x"`, :39).
Session-relevant subset, verbatim:

| command | default | description |
|---|---|---|
| `leader` | `ctrl+x` | Leader key for keybind combinations |
| `app_exit` | `ctrl+c,ctrl+d,<leader>q` | Exit the application |
| `command_list` | `ctrl+p` | List available commands |
| `editor_open` | `<leader>e` | Open external editor |
| `theme_list` | `<leader>t` | List available themes |
| `sidebar_toggle` | `<leader>b` | Toggle sidebar |
| `status_view` | `<leader>s` | View status |
| `session_export` | `<leader>x` | Export session to editor |
| `session_new` | `<leader>n` | Create a new session |
| `session_list` | `<leader>l` | List all sessions |
| `session_timeline` | `<leader>g` | Show session timeline |
| `session_rename` | `ctrl+r` | Rename session |
| `session_delete` | `ctrl+d` | Delete session |
| `session_interrupt` | `escape` | Interrupt current session |
| `session_background` | `ctrl+b` | Background synchronous subagents |
| `session_compact` | `<leader>c` | Compact the session |
| `session_queued_prompts` | `<leader>q` | Manage queued prompts |
| `session_child_first` | `<leader>down` | Go to first child session |
| `session_child_cycle` | `right` | Go to next child session |
| `session_child_cycle_reverse` | `left` | Go to previous child session |
| `session_parent` | `up` | Go to parent session |
| `session_pin_toggle` | `ctrl+f` | Pin or unpin session in the session list |
| `session_quick_switch_1..9` | `<leader>1`…`<leader>9` | Switch to session in quick slot N |
| `messages_page_up` / `_down` | `pageup,ctrl+alt+b` / `pagedown,ctrl+alt+f` | Scroll messages by one page |
| `messages_line_up` / `_down` | `ctrl+alt+y` / `ctrl+alt+e` | Scroll messages by one line |
| `messages_half_page_up` / `_down` | `ctrl+alt+u` / `ctrl+alt+d` | Scroll messages by half page |
| `messages_first` / `_last` | `ctrl+g,home` / `ctrl+alt+g,end` | First / last message |
| `messages_next` / `_previous` / `_last_user` | `none` | Message navigation |
| `messages_copy` | `<leader>y` | Copy message |
| `messages_undo` / `messages_redo` | `<leader>u` / `<leader>r` | Undo / redo message |
| `messages_toggle_conceal` | `<leader>h` | Toggle code block concealment in messages |
| `tool_details` | `none` | Toggle tool details visibility |
| `display_thinking` | `none` | Toggle thinking blocks visibility |
| `agent_list` / `agent_cycle` / `_reverse` | `<leader>a` / `tab` / `shift+tab` | Agents |
| `model_list` | `<leader>m` | List available models |
| `model_cycle_recent` / `_reverse` | `f2` / `shift+f2` | Recent models |
| `variant_cycle` | `ctrl+t` | Cycle model variants |
| `input_submit` | `return` | Submit input |
| `input_newline` | `shift+return,ctrl+return,alt+return,ctrl+j` | Insert newline in input |
| `input_paste` | `ctrl+v` (`preventDefault: false`) | Paste from clipboard |
| `input_clear` | `ctrl+c` | Clear input field |
| `history_previous` / `_next` | `up` / `down` | Prompt history |
| `dialog.select.prev` / `.next` | `up,ctrl+p` / `down,ctrl+n` | Dialog navigation |
| `dialog.select.submit` | `return` | Submit selected dialog item |
| `prompt.autocomplete.prev` / `.next` | `up,ctrl+p` / `down,ctrl+n` | Autocomplete navigation |
| `prompt.autocomplete.hide` / `.select` / `.complete` | `escape` / `return` / `tab` | Autocomplete |
| `permission.prompt.fullscreen` | `ctrl+f` | Toggle permission prompt fullscreen |
| `diff_close` / `diff_toggle` | `escape,q` / `enter,space` | Diff viewer |
| `diff_next_hunk` / `_previous_hunk` | `]` / `[` | Diff hunks |
| `diff_next_file` / `_previous_file` | `n` / `p` | Diff files |
| `diff_toggle_view` | `v` | Split or unified |
| `which_key_toggle` | `ctrl+alt+k` | Toggle which-key panel |
| `terminal_suspend` | `ctrl+z` | Suspend terminal |

Session-scoped commands wired to bindings (`routes/session/index.tsx:115`): `session.share`,
`session.rename`, `session.timeline`, `session.fork`, `session.compact`, `session.unshare`,
`session.undo`, `session.redo`, `session.sidebar.toggle`, `session.toggle.conceal`,
`session.toggle.timestamps`, `session.toggle.thinking`, `session.toggle.actions`,
`session.toggle.scrollbar`, `session.toggle.generic_tool_output`, `session.first`, `session.last`,
`session.messages_last_user`, `session.message.next`, `session.message.previous`, `messages.copy`,
`session.copy`, `session.export`, `session.child.first`, `session.parent`, `session.child.next`,
`session.child.previous`; scrolling commands are registered globally (:145).

In-prompt keys not in the table: **esc** interrupts (two presses, §2.1); permission/question prompts
bind `left/h`, `right/l`, `up/k`, `down/j`, `tab`/`shift+tab`, `1..n`, `return`, `escape` (§4.2, §4.3).

---

## 8. Animation and transition inventory

| Where | Spec | Cite |
|---|---|---|
| TUI inline/block spinner | frames `["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"]`, `interval={80}`; static `⋯ ` fallback when animations are off | `packages/tui/src/component/spinner.tsx:10,17,19` |
| TUI status spinner (prompt footer) | `interval={40}`, generated `blocks` frames/colours, `inactiveFactor: 0.6`, `minAlpha: 0.3`; `[⋯]` fallback | `packages/tui/src/component/prompt/index.tsx:1525,1329-1343,1524` |
| Web `TextShimmer` | `--text-shimmer-step: 45ms; --text-shimmer-duration: 1200ms; --text-shimmer-swap: 220ms; --text-shimmer-spread: 5.2ch; --text-shimmer-size: 360%`; base→peak colours `--text-weak`→`--text-strong`; opacity cross-fade `transition: opacity var(--text-shimmer-swap) ease-out` | `packages/ui/src/components/text-shimmer.css:1-60`, `text-shimmer.tsx:15` (`const swap = 220`) |
| Web `ToolStatusTitle` (Exploring→Explored) | measures widths, animates via one `requestAnimationFrame`, settles after `setTimeout(finish, 600)`; suffix mode when the shared prefix ≥ 2 chars | `packages/session-ui/src/components/tool-status-title.tsx:77,31` |
| Web collapsible tool body | `const SPRING = { type: "spring", visualDuration: 0.35, bounce: 0 }`, height `0px ↔ auto`, `overflow` flipped to `visible` on finish | `packages/session-ui/src/components/basic-tool.tsx:47,158-169` |
| Web shell submessage entry | width spring `visualDuration: 0.25, bounce: 0`; value `opacity 0→1` + `blur(2px)→blur(0)`, `duration: 0.32, ease: [0.16, 1, 0.3, 1]` | `message-part.tsx:102-106` |
| Web deferred tool mounting | heavy default-open bodies mount one per `requestAnimationFrame`, popped from the end ("viewport starts at the latest turn") | `basic-tool.tsx:51-70` |
| Web streaming text pacing | 24 ms ticks, word-boundary snapping, ≤512-char jumps applied instantly | `message-part.tsx:252-306` |
| Web v2 progress indicator | 5×5 dot grid, 2 px dots, 1 px gap, `--_duration: 1200ms`, `ease-out`, infinite, per-dot keyframes staggered in 12.5 % steps; `prefers-reduced-motion: reduce` stops all dots and pins dot 12 at opacity 1 | `session-progress-indicator-v2.tsx:4-12`, `session-progress-indicator-v2.css:1-14` and tail |
| Web `TextReveal` (thinking heading) | `travel={25}`, `duration={700}` | `session-turn.tsx:426-431` |
| Web followup dock chevron | `transform: rotate(180deg)` when collapsed | `session-followup-dock.tsx:54` |
| Copy-button feedback (everywhere) | reset to the idle icon after `2000` ms | `message-part.tsx:1246`, `:1727`, `:2103`; `markdown.tsx:313`; `tool-error-card.tsx:94` |
| Retry countdown | `setInterval(..., 1000)` | `session-retry.tsx:24`; `prompt/index.tsx:1550` |
| TUI scroll | `stickyScroll={true} stickyStart="bottom"`, acceleration from config | `routes/session/index.tsx:1193-1196` |
| Web auto-scroll | `createAutoScroll({ working, onUserInteracted, overflowAnchor: "dynamic" })` | `session-turn.tsx:379` |
