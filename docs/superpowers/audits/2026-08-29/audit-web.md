# Web app audit — `web/apps/web` (Next.js 16.3.1, React 19.2, app router)

Scope: every file under `src/` plus `public/install.sh`, `next.config.ts`, `tsconfig.json`, `eslint.config.mjs`,
`deploy/kloudlite-git-web.yaml`, and the Rust handlers the web calls where a claim needed checking
(`bins/server/src/browse_api/repo.rs`, `crates/api/src/teams.rs`). Toolchain state at audit time:
`bun run lint` clean, `bun test` 35/35 pass (7 files), `tsc --noEmit` fails on two stale `.next/**/types` entries (see W-25).

Paths below are relative to `web/apps/web/src/` unless they start with `/`.

---

## High

### [W-1] Stale-timer bug: env snapshots page re-enters "uploading…" and later fires a false "has not landed" alert
- **Severity:** high
- **Location:** `components/app/env-snapshots.tsx:299-305`, `:312`
- **What:** `asked` (the in-flight push marker) is never cleared once the record lands; `waiting = history.length <= asked.had` becomes true again whenever the history later *shrinks* (deleting any snapshot). The phantom "Taking a snapshot / uploading…" row and `FastRefresh` reappear, and after the 5-minute ceiling the "has not landed" alert fires for a push that landed long ago. Compounding it: `had` is read from the render that carries the action result, but `pushEnvironment` already `revalidatePath`s (`app/(shell)/[owner]/(org)/environments/actions.ts:59`), so a push that lands during the action counts itself and `waiting` never clears.
- **Fix:** Capture `had` at submit time (ref set before `pushAction`, or have the action return the pre-push length from its own `volumeHistory` read); in the render-time block `if (asked && history.length > asked.had) setAsked(null)` and derive `waiting` from `asked !== null` alone.
- **Effort:** S

### [W-2] Personal access token survives dialog close and re-shows on the next open
- **Severity:** high
- **Location:** `components/app/new-token-dialog.tsx:23`, `:31`, `:73`, `:85`
- **What:** `revealed = Boolean(state?.token)` reads `useActionState` state, which outlives the dialog. Close after the reveal, click "Generate token" again → the dialog opens straight into "Token created" showing the *same* secret, contradicting "will not be shown again"; the secret sits in React state for the page's lifetime. `showCloseButton={!revealed}` gates nothing — Escape and overlay click still close.
- **Fix:** Remount the dialog body on close so action state (and the token) is dropped: `const [gen, setGen] = useState(0); <Inner key={gen} onClose={() => setGen(g => g + 1)} />`. Drop `showCloseButton` or block `onEscapeKeyDown/onPointerDownOutside` while revealed.
- **Effort:** S

### [W-3] Next data cache serves object-store blobs/trees across visibility changes, forever, keyed by bearer token
- **Severity:** high
- **Location:** `lib/browse.ts:43-51` (`cache: "force-cache"` for `tree`, `blob`, `log`, `commit`, `files`, `lastmod`)
- **What:** Oid-keyed reads are stored in Next's persistent data cache with no `revalidate` and no tag. Two consequences: (a) a repo flipped from public to private keeps serving its cached trees/blobs to *anonymous* callers (`token` undefined → same cache key) for as long as the standalone pod's `.next/cache` lives — the visibility check is bypassed for any oid already seen; (b) the cache key includes the `Authorization` header, so every user's token is part of an on-disk cache key (hashed, but unbounded growth — one entry per (token, oid, path)). `setVisibility` in `app/(shell)/[owner]/[repo]/settings/actions.ts:30` only `revalidatePath`s, which does not purge fetch-cache entries.
- **Fix:** Tag the fetches (`next: { tags: [\`repo:${owner}/${repo}\`] }`) and call `revalidateTag` from `setVisibility` and `destroyRepo`; or drop `force-cache` for anonymous reads (`immutable && token`) so a public→private flip is honoured; add a `revalidate` ceiling (e.g. 1 day) so the cache is bounded.
- **Effort:** M

---

## Medium

### [W-4] "Change email" / "Use a different email" shows a validation error instead of the empty form
- **Severity:** medium
- **Location:** `components/auth/login-form.tsx:60`, `:78`; `app/(auth)/login/actions.ts:22-26`
- **What:** Both buttons submit `email=""` to `continueWithEmail`, whose first check is the email regex → returns `{ step: "email", error: "Enter a valid email address." }`. Every user who clicks "Change" lands on the email step with a red error they did not cause.
- **Fix:** In `continueWithEmail`, `if (email === "") return { step: "email" };` before the regex (or give the buttons `name="reset" value="1"` and branch on it).
- **Effort:** S

### [W-5] Browse `log` sends `?page=` but the server reads `?n=` — the pagination arg is silently ignored
- **Severity:** medium
- **Location:** `lib/browse.ts:84-86` (`log(..., page = 1)` → `?page=${page}`); callers `components/repo/commits.tsx:46` (`PAGE + 1`), `lib/repo-rail.ts:21` (`50`); server `/bins/server/src/browse_api/repo.rs:118-124` (`q.get("n")`, default 50, clamp 1..200)
- **What:** The web thinks it is passing a count (41, 50); the server ignores `page` and always returns 50. The commits page only works because 50 > 41; `next` cursor logic (`page.value[PAGE]`) is correct by accident. Any future change to `PAGE` above 49 breaks paging silently.
- **Fix:** Rename the parameter to `n` in `lib/browse.ts` (`?n=${n}`), document the 200 clamp, and pass exactly `PAGE + 1`.
- **Effort:** S

### [W-6] Team `website` is stored and rendered as `href` with no scheme validation
- **Severity:** medium
- **Location:** `components/app/team-settings.tsx:139` (`type="url"` only), `app/(shell)/[owner]/(org)/settings/actions.ts:149`, `components/app/team-profile.tsx:77`; api `/crates/api/src/teams.rs:505-511` stores it verbatim
- **What:** Any team admin can save `javascript:…` or `data:…` and it is rendered on the **public** profile page. React 19's `sanitizeURL` (react-dom-client.production.js:1412) turns `javascript:` into a throwing stub, so this is not a live XSS today — but it relies entirely on the framework, the api has no check, and the `<a>` has no `rel="noopener noreferrer"`.
- **Fix:** In `saveProfile`: `try { const u = new URL(website); if (!/^https?:$/.test(u.protocol)) throw 0 } catch { return { error: "Website must start with http:// or https://" } }`; mirror in `crates/api/src/teams.rs`; add `rel="noopener noreferrer"` at `team-profile.tsx:77`.
- **Effort:** S

### [W-7] Every `<form action>` in the app wipes the user's input on a *failed* action
- **Severity:** medium
- **Location:** `components/app/add-key-dialog.tsx:49,63`; `new-token-dialog.tsx:42`; `new-repo-form.tsx:73,91`; `team-settings.tsx:89,100,131-143,210`; `components/repo/repo-settings.tsx:40,107`; `components/repo/new-pull-form.tsx`
- **What:** React 19 resets uncontrolled fields after a form action completes, success or not. A rejected 50-line SSH key paste, a taken repo name (name *and* description), a failed invite email — all blank. `components/onboarding/username-form.tsx:30` already does it right (`defaultValue={state?.suggestion}`).
- **Fix:** Return the submitted values in the error state (`{ error, values }`) and feed them back as `defaultValue`; or make the long fields controlled.
- **Effort:** M

### [W-8] Single un-confirmed click removes an SSH key, signing key, passkey, or protection rule
- **Severity:** medium
- **Location:** `components/app/user-settings.tsx:115,194`; `components/app/passkeys-section.tsx:78`; `components/repo/repo-settings.tsx:93`
- **What:** `DeleteForm` is used without `confirm`, while the sibling rows (revoke invite `team-settings.tsx:256`, revoke CLI login `cli-tokens.tsx:38`) do confirm. Removing the only passkey locks a person out.
- **Fix:** `confirm={\`Remove ${name}?\`}` on all four.
- **Effort:** S

### [W-9] Whole-app `router.refresh()` every 10 s re-runs every server component for every open tab
- **Severity:** medium
- **Location:** `components/app/auto-refresh.tsx:27-41` (mounted in `app/(shell)/layout.tsx:26`); `components/app/fast-refresh.tsx` (2 s); `next.config.ts` `staleTimes.dynamic: 30`
- **What:** The shell layout polls unconditionally. On a blob page that means shiki re-highlights up to 200 KB (`lib/highlight.ts:92`) every 10 s per viewer; on `/settings` it re-reads 3 + 3×owners api lists every 10 s; on the repo home it re-fetches refs + tree + rail. `staleTimes: 30` cannot help because `refresh()` drops the client cache. Only the workspace/environment lists (and PR merge state) actually change on their own.
- **Fix:** Mount `AutoRefresh` only in the layouts that watch external state (`(org)/workspaces`, `(org)/environments`, `pulls/[number]`), or pass a per-route interval; keep `FastRefresh`'s "only while transitional" rule as the model.
- **Effort:** S

### [W-10] Restore intent decided by comparing two client-supplied hidden fields
- **Severity:** medium
- **Location:** `components/app/env-snapshots.tsx:149-156`; `app/(shell)/[owner]/(org)/environments/actions.ts:104,125-128`
- **What:** `name === currentName` picks in-place vs new-environment restore. A tampered `currentName` flips an in-place restore into a new environment (or the reverse) with no explicit signal. `restoreEnvironmentInPlace` action (`:66-83`) has no callers.
- **Fix:** One explicit `mode=inplace|new` hidden field set by the dialog's own choice; delete `currentName` and the unused action.
- **Effort:** S

### [W-11] `/settings` page is a 6-deep sequential fetch waterfall
- **Severity:** medium
- **Location:** `app/(shell)/settings/page.tsx:20-37`
- **What:** `ownersFor` → `listPasskeys` → `listCliTokens` → `platformKey` run one after another, then per owner `listKeys` → `listKeys(signing)` → `listTokens` sequentially inside the `map`. With three owners that is 4 + 3 round trips on the critical path (and re-run every 10 s by W-9). `platformKey` GET is also a write ("reading it is what generates it") re-issued on every poll.
- **Fix:** `Promise.all` the independent calls; inside the map `Promise.all([keys, signing, tokens])`.
- **Effort:** S

### [W-12] `wsenv-state-badge` throws on any state string the api adds later
- **Severity:** medium
- **Location:** `components/app/wsenv-state-badge.tsx:20-22`
- **What:** `LOOK[state]` is undefined for an unknown state; `look.cls` throws and takes the whole list/home page to the error boundary. JSON from the api is never narrowed at runtime.
- **Fix:** `const look = LOOK[state] ?? { cls: "…muted", label: state }`.
- **Effort:** S

### [W-13] Hydration mismatch on every snapshot row from server-TZ `toLocaleString`
- **Severity:** medium
- **Location:** `components/app/env-snapshots.tsx:424,487`; also `components/repo/commit-meta.ts:16-24` (`dayBucket` uses the pod's TZ for "Today/Yesterday")
- **What:** `new Date(...).toLocaleString("en")` in `title` renders in the pod's TZ on the server and the viewer's on the client — React 19 reports an attribute mismatch. `dayBucket` is server-only so it does not mismatch, but groups commits by UTC day, which is wrong for most viewers.
- **Fix:** Add an absolute formatter to `lib/time.ts` with a fixed `timeZone: "UTC"` and use it for both; or `suppressHydrationWarning` on the spans.
- **Effort:** S

### [W-14] Start/stop errors are invisible except as a tooltip
- **Severity:** medium
- **Location:** `components/app/env-actions.tsx:25`; `components/app/workspace-list.tsx:62,75`
- **What:** A refused start/stop puts `state.error` only in `title`; keyboard, touch and screen-reader users see the button silently re-enable.
- **Fix:** Render `{state?.error && <p role="alert" className="text-caption text-destructive">…</p>}` beside the button (the dialogs already do).
- **Effort:** S

### [W-15] Text inputs with placeholder only, no accessible name
- **Severity:** medium
- **Location:** `components/app/env-actions.tsx:55,117`; `components/app/workspace-list.tsx:44,98,129`; `components/app/restore-dialog.tsx:38`; `components/repo/file-editor.tsx:84-99` (radios without `name`, so arrow-key grouping is broken); `components/repo/repo-settings.tsx:53-67` (visibility radios lack `fieldset/legend`, unlike `new-repo-form.tsx:21`)
- **What:** Placeholder is not a label; unnamed radios are not a group.
- **Fix:** `aria-label` matching the placeholder; `name="target"` on the radios (keep the hidden input or drop it); `fieldset` + `sr-only legend` on the visibility group.
- **Effort:** S

### [W-16] Two push entry points on the snapshots page; only one owns the pending row
- **Severity:** medium
- **Location:** `components/app/env-actions.tsx:36` (header PushDialog) vs `components/app/env-snapshots.tsx:440` (inline form)
- **What:** A push from the header returns `requestId` to a component that does not track `asked`, so no pending row and no `FastRefresh`; the landed record arrives on the 10 s poll the comment at `env-snapshots.tsx:403` calls too slow.
- **Fix:** Hide the header Push on the snapshots route, or lift `asked` into the shell context the header dialog can set.
- **Effort:** S

### [W-17] Server action blocks up to 60 s polling for a snapshot to land
- **Severity:** medium
- **Location:** `app/(shell)/[owner]/(org)/environments/actions.ts:110-123` (`restoreEnvironmentFrom`, ponytail-marked); client ceiling is 5 min at `env-snapshots.tsx:312`
- **What:** 30 × 2 s `setTimeout` inside a server action holds a request open for a minute; behind Cloudflare/ingress timeouts this can be cut mid-flight and the user is told nothing was restored when the restore may still run. Two different ceilings and messages for the same push.
- **Fix:** Return after the push is *requested* and let the existing client poll gate the Restore button; or have `/v1` project the SnapshotRequest status by id (the ponytail upgrade path) and poll that from the client.
- **Effort:** M

### [W-31] `RefPicker` keeps stale state when `?ref=` changes under it
- **Severity:** medium
- **Location:** `components/repo/ref-picker.tsx:36`
- **What:** `useState(current)` never resyncs with the prop. Back/forward, or the `?ref=<sha>` links from `pull-commits.tsx:77`, re-render the same instance with a new `current` while the trigger and tick still show the old ref.
- **Fix:** Derive the label/tick from `current`; keep state only for `open` and the filter text.
- **Effort:** S

### [W-32] PR "Files" tab ships every hunk of every file and hides the truncation notice
- **Severity:** medium
- **Location:** `components/repo/pull-files.tsx:77-101`; `components/repo/diff-files.tsx:37`
- **What:** A >300-line file is wrapped in a closed `<details>` but its rows are still in the HTML; `lib/diff.ts` sets `truncated` at the 4 MiB api cap yet `PullFiles` never shows it (the commit page does, `diff.tsx:87`).
- **Fix:** Render `FileHunks` lazily for opened files (client toggle) or cap lines per file with a "show full file" link; add the `truncated` notice.
- **Effort:** M

### [W-33] Home-rolled menus/listboxes without keyboard support
- **Severity:** medium
- **Location:** `components/repo/ref-picker.tsx:86-112` (`role="listbox"` > `li[role=option]` > `<button>`: nested interactive, arrows do nothing); `components/repo/pull-actions.tsx:165-200` (strategy popover: `aria-expanded` without `aria-controls`, no Escape, no outside-click, no focus move)
- **Fix:** Use `DropdownMenu`/`DropdownMenuRadioItem` (as `clone-menu.tsx` does) or `Command` from `components/ui`.
- **Effort:** S

### [W-34] Raw email printed as PR author on the list; unencoded image href
- **Severity:** medium
- **Location:** `components/repo/pulls.tsx:47` (`{p.author}` — every sibling goes through `lib/person.ts` `displayName`); `components/app/team-profile.tsx:207` (`/${slug}/registries/${img.name}` unencoded, while `image-list.tsx:80` encodes)
- **Fix:** `displayName(p.author)`; `encodeURIComponent(img.name)`.
- **Effort:** S

### [W-35] Landing page runs a 150 ms `setInterval` re-rendering ~100 inline-styled nodes forever
- **Severity:** medium
- **Location:** `components/marketing/environment-panel.tsx:165-167,186`; `components/marketing/landing.tsx:27` (Radix `ScrollArea` around the whole marketing page)
- **What:** The heaviest client bundle sits on the lightest route, ticks for the page's lifetime, and the `ScrollArea` defeats native scroll restoration/anchors.
- **Fix:** CSS keyframes for the typewriter, `requestAnimationFrame` gated on visibility for phase changes, or `next/dynamic` with `ssr: false`; native scroll on the landing page.
- **Effort:** M

---

## Low

### [W-18] `curl | sh` installer is not wrapped in a function
- **Severity:** low
- **Location:** `/web/apps/web/public/install.sh:6-65`
- **What:** A partial download executes a partial script (`set -eu` does not help). Checksum is best-effort (warns and installs unverified when `sha256sums` is missing), and `REPO` is overridable from the environment — fine for dev, but the script has no way to pin a version (`KL_VERSION`).
- **Fix:** Wrap the body in `main() { … }; main "$@"`; add `VERSION=${KL_VERSION:-latest}`; consider failing closed on a missing checksum once every release ships one.
- **Effort:** S

### [W-19] No `next` return-to on most `redirect("/login")` calls
- **Severity:** low
- **Location:** `app/(shell)/[owner]/[repo]/guard.ts:28,32`; `registries/[image]/guard.ts:16,20`; `invite/[token]/page.tsx:18` (ponytail-marked); every `(org)` page's guard
- **What:** `safeNext` and `?next=` exist and are used by `/cli/authorize` and `/login`, but a signed-out deep link to a repo, image, or invite lands on `/` after sign-in.
- **Fix:** One `requireToken(next: string)` helper in `lib/session.ts` that redirects to `/login?next=${encodeURIComponent(next)}`; replace the ~15 copies of the 6-line guard.
- **Effort:** S

### [W-20] `x-forwarded-host` trusted for the WebAuthn relying party
- **Severity:** low
- **Location:** `lib/passkey.ts:24-34`
- **What:** `rpID`/`origin` come from `x-forwarded-host` first. The ingress sets it, but a direct-to-pod request (or a mis-set proxy) with a forged header makes the server verify against a different origin. WebAuthn's own binding means the attacker gains nothing without the private key, so this is defence-in-depth only.
- **Fix:** Derive from `AUTH_URL` when set (it is required in production per `auth.ts:123`), fall back to headers only in dev.
- **Effort:** S

### [W-21] Hard-coded production hostnames as env fallbacks
- **Severity:** low
- **Location:** `lib/clone.ts:19-20` (`dev.kloudlite.io`, `git.khost.dev`); `app/(shell)/[owner]/(org)/registries/page.tsx:26` and `registries/[image]/page.tsx:13` (`cr.khost.dev`, duplicated)
- **What:** A deployment that forgets the env var hands out clone/pull commands pointing at someone else's hosts — the exact failure `lib/clone.ts`'s own comment warns about.
- **Fix:** Fail loudly like `AUTH_URL` does, or fall back to the request host; hoist the registry host into `lib/clone.ts`.
- **Effort:** S

### [W-22] `Number(formData.get("number"))` / `Number(number)` never checked for NaN
- **Severity:** low
- **Location:** `app/(shell)/[owner]/[repo]/pulls/actions.ts:40,60,78`; `pulls/[number]/pull-data.ts:19`; `activity-actions.ts:18` (`Math.min(limit, 100)` with an untyped client `limit`)
- **What:** `/pulls/abc` → api `/pulls/NaN` → 404 (acceptable) but `revalidatePath("/…/pulls/NaN")`; `moreActivity(owner, -5)` sends `limit=-5`.
- **Fix:** `Number.isInteger(n) && n > 0` guard, clamp `limit` to `1..100`.
- **Effort:** S

### [W-23] Hand-rolled README renderer
- **Severity:** low
- **Location:** `components/repo/code.tsx:23-49`
- **What:** Handles `#`, `##`, `- `, fences and inline code only; links, images, `###`, numbered lists, tables, blockquotes render as raw text. No XSS (React text), but READMEs look broken. Also `blob(...README.md)` is speculatively fetched on every directory even when the listing shows none.
- **Fix:** Either accept and document the subset, or use a small server-only markdown parser with a strict sanitizer (no raw HTML) — but only if READMEs matter to the product.
- **Effort:** M

### [W-24] Duplicated code across the app
- **Severity:** low
- **Location:** "Your session has expired. Sign in again." ×40 across every `actions.ts` (`(org)/settings/actions.ts:29` already has `tokenOr()`); identical `Saved` in `team-settings.tsx:22` and `repo-settings.tsx:16` (a third inline at `image-settings.tsx:87`); `StartForm/StopForm` in `workspace-list.tsx:56-80` vs `ToggleForm` in `env-actions.tsx:18-31`; initials logic in `user-menu.tsx:19` vs `components/app/initials.tsx:17`; title+owner grid copy-pasted in `add-key-dialog.tsx:51-58` / `new-token-dialog.tsx:44-51`; the session/token/`loadEnvPage`/`notFound` triplet in env layout + 3 pages; `FileView` decodes the blob twice (`file-view.tsx:50-51`).
- **Fix:** Export `tokenOr` from `lib/api-token.ts`; move `Saved` to `settings-section.tsx`; use `Initials` in `UserMenu`; one `requireEnvPage(params)` in `lib/env-page.ts`.
- **Effort:** M

### [W-25] `tsc --noEmit` is red locally because `tsconfig` includes `.next/**/types`
- **Severity:** low
- **Location:** `/web/apps/web/tsconfig.json:29-30`; errors reference `.next/dev/types/validator.ts:377` (`dev-graph/page`) and `.next/types/validator.ts:71` (a removed `verify/[token]/page`)
- **What:** Stale generated route validators from deleted pages fail typecheck until `.next` is wiped; CI is clean only because it starts from an empty tree. The project note "editor diagnostics are stale; trust tsc" is undermined when tsc is stale too.
- **Fix:** `rm -rf .next` in the `typecheck` script, or drop `.next/dev/types/**` from `include`.
- **Effort:** S

### [W-26] No root `error.tsx` / `global-error.tsx`; no security headers
- **Severity:** low
- **Location:** `app/` (only `(shell)`, `(auth)`, `(onboarding)` have `error.tsx`); `next.config.ts` has no `headers()`; ingress yaml sets none
- **What:** A throw in `app/layout.tsx` (fonts, ThemeProvider) has no boundary. No CSP, `X-Frame-Options`/`frame-ancestors`, or `Referrer-Policy` — the `/invite/{token}` and `/verify/{token}` URLs carry bearer tokens in the path and leak via `Referer` to `kloudlite.io` links on the auth pages.
- **Fix:** Add `app/global-error.tsx`; `headers()` in `next.config.ts` with `Referrer-Policy: strict-origin-when-cross-origin`, `X-Content-Type-Options: nosniff`, `frame-ancestors 'none'`.
- **Effort:** S

### [W-27] Design-convention slips
- **Severity:** low
- **Location:** `rounded-full` at `components/app/env-snapshots.tsx:415`, `environments/[id]/snapshots/loading.tsx:11,23`, `components/ui/scroll-area.tsx:52`; `not-found.tsx:8` and both auth layouts mount a client `ScrollArea` for a static page; `components/app/shell-nav.tsx:20` ponytail (`new-repo`, `new-team`, `invite` are legal handles and would get the wrong crumb); 25 of 44 `page.tsx` files export no `metadata` (every repo/image/env page shows the bare "kloudlite" tab title)
- **Fix:** Square dots (or one shared `Dot` if round is the accepted exception); native `overflow-y-auto` on static pages; add the three words to the server's reserved-handle list; `generateMetadata` in the repo/image/env layouts.
- **Effort:** S

### [W-28] Untested logic
- **Severity:** low
- **Location:** tests exist only for `utils`, `slug`, `ssh-config`, `assertion`, `destination`, `shell-nav.place`, `view-as`
- **What:** `lib/diff.ts` (parser, line numbering, truncation), `lib/highlight.ts` `langFor/fenceLang/blockLines`, `lib/time.ts` `when/size`, `lib/languages.ts` `breakdown`, `lib/browse.ts` `resolveRef/defaultBranch/decodeBlob`, `lib/env-page.ts` `provenanceOf`, `file-search.tsx` `fuzzy`, and the snapshot tree layout in `env-snapshots.tsx:317-399` are pure and untested; W-1 and W-5 would have been caught by a test.
- **Fix:** One `*.test.ts` per pure module above, starting with `diff.ts` and `browse.ts`.
- **Effort:** M

### [W-29] Minor perf
- **Severity:** low
- **Location:** `components/app/env-snapshots.tsx:317-399` (O(n²·depth) tree layout recomputed every 2 s render — `useMemo`); `app/(shell)/[owner]/(org)/environments/page.tsx:38-51` (N+1 `volumeHistory`, ponytail-marked); `app/layout.tsx:21-27` (Hubot Sans 712 KB TTF, ponytail-marked — `woff2` is ~40% smaller); `components/repo/code.tsx:116` ships up to 5000 paths per page load (ponytail-marked).
- **Fix:** As marked; the font conversion is a one-off `fonttools` command.
- **Effort:** S

### [W-30] Small correctness nits
- **Severity:** low
- **Location:** `components/app/search-dialog.tsx:40-42` (`r.json()` trusted as an array; a 502 body `[]` is fine but a proxy HTML page would throw in `mine.map`); `components/app/env-snapshots.tsx:124,183-189` (`keep` is one-way and survives Cancel); `:125` restore title falls back to the literal "snapshot" while delete uses `id.slice(0,8)`; `components/app/workspace-list.tsx:98` clone name lacks `required`; `components/app/team-settings.tsx:318` `RoleSelect` finds its form via `document.getElementById` — use a ref; `team-settings.tsx:82` dead `disabled` prop; `team-switcher.tsx:15` `personal?: true`; `new-token-dialog.tsx:73` `state!.token!`; `env-snapshots.tsx:474` `f.node!`; `env-settings.tsx:26-34` read-only "General" section that only says rename is unsupported.
- **Fix:** As listed; each is a one-line change.
- **Effort:** S

### [W-36] Six copies of the copy-to-clipboard toggle; `CopyLine` exported from a list component
- **Severity:** low
- **Location:** `components/repo/clone-menu.tsx:69-95`, `remote-picker.tsx:47-60`, `copy-button.tsx`, `command-block.tsx:23-34`, `file-actions.tsx:18-30`, `components/app/image-list.tsx:103-114`
- **Fix:** Give `CopyButton` `size`/`className` and use it everywhere; move `CopyLine` to its own file.
- **Effort:** S

### [W-37] Accessibility/markup nits
- **Severity:** low
- **Location:** `components/app/repo-list.tsx:19` (`aria-label` on an `aria-hidden` lucide svg — never read); `components/app/skeleton.tsx:17` (`aria-busy` on a div without `role="status"`); `components/repo/pull-page.tsx:58` (`<span>` wrapping a `<form>`); `components/repo/pull-conversation.tsx:58` and `diff-files.tsx:92,109` (index keys — safe while comments are append-only); `components/repo/pull-files.tsx:44` (dead ternary); `components/app/home.tsx:184` (`when(r.createdAt)` lacks the `typeof` guard `repo-list.tsx:123` keeps for rollout skew); `components/app/nav-tabs.tsx:81` (effect keyed on a fresh `tabs` array rebuilds the ResizeObserver every render); `lib/time.ts:24` / `recent-activity.tsx:16` (`Date.now()` during SSR can differ at hydration); `components/marketing/environment-panel.tsx:46,1670,1702` (`mode: string`, `as "running" | "paused"` casts, `borderRadius: "50%"` dots).
- **Fix:** As listed.
- **Effort:** S

---

## Verified good

- **Open redirects:** every `redirectTo`/`next` passes through `safeNext` (`app/(auth)/login/destination.ts:7-13`, tested), including the emailed magic link and the OAuth form field; `//` and `/\` are refused.
- **Session/token handling:** the api bearer token lives only in the encrypted Auth.js JWT and is read server-side via `getToken` (`lib/api-token.ts`); it is deliberately absent from the `session()` callback (`auth.ts:186-196`), never placed in a URL (`welcome/actions.ts:25-32`), and re-minted a minute before expiry (`auth.ts:169-183`). `AUTH_URL` is required in production so `Secure` cookies cannot silently drop (`auth.ts:120-127`).
- **Credentials providers:** passkey and email-link providers accept only an HMAC assertion signed with `AUTH_SECRET` (`lib/assertion.ts`, `timingSafeEqual`, 60 s expiry, tested); the preview password uses a constant-time compare and an allow-list.
- **WebAuthn:** challenge in an httpOnly, `sameSite=strict`, single-use cookie (`lib/passkey.ts:41-59`); counter recorded on every success; unknown credential and bad signature give the same answer.
- **Server actions:** every action re-reads the token server-side, never trusts the form for authorization, and every `owner`/`repo`/`image` that reaches `revalidatePath` is `safeSegment`-checked (`lib/slug.ts`, tested); typed-confirm deletes are enforced in the action, not just by a disabled button.
- **XSS:** the only `dangerouslySetInnerHTML` is shiki's own output (`components/repo/code-block.tsx:12`); all user strings (names, messages, comments, diffs, SSH host keys) render as JSX text; `sshConfigBlock` refuses names that could inject ssh keywords (`lib/ssh-config.ts:14-21`, tested); `pathHref`/`filePath` escape every path segment (tested).
- **Enumeration resistance:** magic-link and passkey flows answer identically for unknown addresses/credentials; `/v2`-style 404-for-forbidden is preserved end to end (`lib/browse.ts:56-58`, `lib/api.ts:87-89`); the `unauthorized` kind is never collapsed into an empty list (`lib/require-api.ts`).
- **Server/client split:** pages, layouts, settings sections, lists are server components; the 63 `"use client"` files are leaves with real interactivity; ⌘K palette and its cmdk chunk load on first open (`global-search.tsx:12`); shiki runs server-side with lazy grammar loading and a 200 KB cap.
- **Caching discipline:** `react.cache` dedupes `guardRepo`, `guardImage`, `listRepos`, `getRepo`, `loadEnvPage`, `pullData` within a render; the api tier is `no-store`; the rail is streamed via `Suspense` on the file page.
- **Error boundaries:** `(shell)`, `(auth)`, `(onboarding)` boundaries log the real error client-side and show only the digest; the health probe is a bodyless 204.
- **Accessibility baseline:** every error is `role="alert"`, icon-only buttons in the settings and repo surfaces carry `aria-label`, the file search is a proper `combobox/listbox`, state badges pair colour with text, dialogs are Radix.
- **Design tokens:** no raw Tailwind palette colours anywhere in `src/`; `--radius: 0` is honoured by the shadcn primitives' `rounded-*` (they resolve to 0); type scale is registered with `tailwind-merge` (`lib/utils.ts`).
- **Lint/typecheck/tests in CI:** `web.yml` runs typecheck, lint, test, build on every web change; lint is clean; 35 tests pass.
