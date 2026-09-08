# Branches: a list page, and deleting a branch on the server

Status: approved in chat 2026-09-08 ("write spec and plan and implement it").

## Why

A repo's branches are visible only through the branch picker on the Code tab, and the only way
to delete one is `git push origin --delete` from a machine that has the repo. People merge a pull
request in the web and are left with a stale branch they cannot remove from where they are.

## Decisions taken

| Question | Answer |
| --- | --- |
| Who may delete | anyone who may act under the owner (own handle or team member) — the same gate that merges a pull request (`settings_caller`) |
| The default branch | never deletable from the web (409). `main` today (`refs::DEFAULT_BRANCH`, fixed) |
| A protected branch | refused by the existing rule (`Protection.no_delete`) with the rule's own sentence, 409 |
| A branch with an open pull request as its head | refused, 409 "close or merge pull request #N first" — deleting the head of an open PR makes it unmergeable and hides why |
| Concurrency | the client sends the oid it saw; the delete is a compare-and-swap on it (`RefUpdate{old: Some(oid), new: None}`); a moved branch is a 409 "the branch moved; reload" |
| Tags | out of scope; the page lists branches only |
| Last-commit metadata per branch | out of scope for this pass (one `log` call per branch); the row shows name, short oid, default/protected/open-PR marks |

## Design

### 1. Server (`bins/server`)

`POST /api/{owner}/{name}/branchdelete?branch=<name>&oid=<hex>` on the peer listener, beside
`protect`. It opens the repo like every other write here (the api tier has already decided the
caller may act under the owner and forwards the owner header), refuses `branch == DEFAULT_BRANCH`
(409), then runs `update_refs(store, repo, [RefUpdate { name: "refs/heads/<branch>", old: Some(oid),
new: None }])` — the one entry point every ref write goes through, so the protection rules, the
ref-name rule and the cache invalidation all apply unchanged. A verdict is a 409 carrying the
rule's sentence; a compare-and-swap miss is a 409 "the branch moved; reload"; a missing branch
is a 404. `branchdelete` joins `BROWSE_TAILS`, and `every_browse_route_is_routable` holds it.

### 2. Api tier (`crates/api`)

`DELETE /v1/repos/{owner}/{name}/branches/{branch}?oid=<hex>`. `settings_caller` gates it. Before
forwarding it lists this repo's OPEN pull requests from the owning node's `/api/{o}/{n}/pulls?state=open`
(pull requests live in the repo's own database) and refuses (409, naming
the number) any whose `head` is the branch. Then `ask_owner` to the server route; 2xx → 204,
404 → 404 "no such branch", 409 → 409 with the server's sentence verbatim, else 502.

There is no new list route: the web already reads `/api/{owner}/{repo}/refs` through the browse
proxy, and the protection rules through `/v1/repos/{owner}/{name}/protection`. The page derives
"protected" from `Protection.matches(branch) && no_delete`; that predicate moves nowhere — the web
mirrors the pattern grammar the server already documents (glob `*` only).

### 3. Web (`web/apps/web`)

A `Branches` tab between Code and Pull requests (`REPO_TABS`, suffix `/branches`). The page,
`[owner]/[repo]/branches/page.tsx`, reads refs + protection + open pulls in parallel, filters
`refs/heads/`, orders the default branch first then alphabetically, and renders one row per branch
in the `repo-list.tsx` shape: name (links to `/{owner}/{repo}/tree?ref=`), short oid, pills
(`default`, `protected`, `PR #N` when an open pull has it as head), and a Delete button. The button
is disabled with the reason as its title for default/protected/open-PR branches; otherwise it opens
the same confirm dialog the settings page uses for destructive actions ("Delete branch `x`? The
commits stay reachable only while something else points at them."). The action
(`branches/actions.ts` `deleteBranch`) posts to `api.deleteBranch(token, owner, repo, branch, oid)`
and shows a 409's message verbatim; on success it revalidates the page.

### 4. Probe

Two hourly ids in stage "2 · Git"'s owner file (the run already pushes a branch there):

| id | sli | target |
| --- | --- | --- |
| `git.branch.delete` | A branch pushed by this run is deleted through the web API and no longer listed by `refs` | `avail(99.9)` |
| `git.branch.delete.refused` | Deleting the default branch answers 409 and it is still listed | `avail(99.9)` |

`deploy/slo.md` and the web fixture carry the same rows.

### 5. Failure modes

| Failure | Behaviour |
| --- | --- |
| branch moved between list and delete | 409 "the branch moved; reload"; nothing deleted |
| protected | 409 with the rule's sentence; nothing deleted |
| default branch | 409 at the api tier AND the server; nothing deleted |
| open PR head | 409 naming the PR; nothing deleted |
| server unreachable | 502; the page shows the message |

## Files

`bins/server/src/browse_api/admin.rs` (handler), `bins/server/src/browse_api/mod.rs` (route),
`bins/server/src/router/route.rs` (`BROWSE_TAILS`), `tests/browse_http.rs`; `crates/api/src/repos.rs`
(handler + PR check), `crates/api/src/lib.rs` (route), its tests; `web/apps/web/src/lib/api.ts`
(`deleteBranch`), `components/app/app-shell.tsx` (tab), `app/(shell)/[owner]/[repo]/branches/{page.tsx,actions.ts}`,
`components/repo/branches.tsx`; `bins/slo/src/stages/git.rs`, `crates/workspaces/src/slo/catalogue.rs`,
`deploy/slo.md`, `web/apps/web/src/lib/fixtures/superadmin.ts`; `CLAUDE.md` (one sentence under
"Two namespaces, one server").
