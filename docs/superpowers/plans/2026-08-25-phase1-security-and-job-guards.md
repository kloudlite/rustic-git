# Phase 1: Critical Security and Job-State Guards — Implementation Plan

> **For agentic workers:** execute this with `superpowers:subagent-driven-development` — one
> subagent per `### Task`, in order, each task's steps run in strict TDD sequence (failing test
> first, then the smallest implementation that makes it pass, then the commit).

## Goal

Close the one critical remote host-compromise in the workspaces subsystem (`C1`: an unvalidated
`Mount.folder` bind-mounts arbitrary host paths into a user-chosen container on a root agent), and
the four job-state defects that let work run on the wrong node, run twice, or resurrect a deleted
workspace (`H1`, `H4`, `H5`), plus the two one-line waste fixes (`P6`, `P7`).

Nothing here changes a data model or a wire contract other than adding one query parameter
(`?agent=` on the two job-report routes). No new dependency, no new crate.

## Architecture

The trust boundary for mount names is `crates/workspaces/src/api.rs::create_env` — the only place
user-authored `Service`/`Mount` values enter the system (`clone_env` copies an already-validated
`Environment.services`; no update/PATCH route for services exists). Validation lives in **one
shared helper** next to the type it validates:

```
crates/workspaces/src/model.rs::validate_mount(&Mount) -> Result<(), String>
                    │
   ┌────────────────┼─────────────────────────────┐
   │                │                             │
api.rs::create_env  bins/agent mkdir_env_mounts   engine/compose.rs::render
(trust boundary,    (defence in depth, root       (defence in depth, the
 400 to the user)    process, fails the job)       string that becomes a bind)
```

The helper deliberately sits in `model.rs`, not in `compose.rs`: a later plan replaces `compose.rs`
wholesale with a bollard runtime, and that rewrite must be able to call the same function without
depending on the compose module it deletes.

Job-state guards are all local edits inside `bins/server/src/vol_agent.rs`; the agent side changes
only to pass its own id when reporting.

## Tech Stack

Rust 2021 workspace (axum 0.8, tokio, serde, reqwest, chrono), `rustic_git_storage::store::valid_segment`
for the segment rule, `rustic_git_workspaces::store::MemStore` for tests, integration tests in
`tests/vol_agent.rs` (root package `rustic-git-tests`), unit tests in the crate modules.

## Audit findings covered

From `docs/superpowers/audit-2026-08-25.md`:

| ID | Severity | Summary |
|---|---|---|
| C1 | Critical | Unvalidated `Mount.folder`/`Mount.path` → host bind-mount escape as root |
| H1 | High | `agent: None` jobs leased by any agent → work runs on the wrong node |
| H4 | High | `job_done`/`job_failed` overwrite state with no lease-ownership check |
| H5 | High | `mark_ws_ready` resurrects a `Deleted` workspace |
| P6 | Perf | Requeue sweep runs on every replica |
| P7 | Perf | Agent 204 poll branch has no sleep floor |

Explicitly **out of scope** (later phases): H2 lease renewal, H3 env re-materialization, H6
lineage tmp+rename, H7 sweep-exhausted marks Error, P1–P5.

## Global Constraints

- `cargo clippy --workspace -- -D warnings` must pass. Test targets are excluded from the CI gate,
  but the bar there is still **no new warnings in files you touch**.
- `cargo test` must pass — the whole workspace, not just the file you edited. Task 4 changes
  existing tests in `tests/vol_agent.rs`; that is expected and called out step by step.
- Comments explain **why**, never what. Match the density of `bins/server/src/router/route.rs`.
- Deliberate shortcuts get a `// ponytail: <ceiling and upgrade path>` marker; keep existing
  markers when editing near them.
- Commit subjects are imperative sentence case, no tool attribution, no `Co-Authored-By`.
- One commit per task. Do not batch tasks into one commit.

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/workspaces/src/model.rs` | Modify (add `validate_mount` + `#[cfg(test)] mod tests`) | The single mount-validation rule, next to `Mount` |
| `crates/workspaces/src/api.rs` | Modify (`create_env`, ~line 536; add `#[cfg(test)] mod tests`) | Enforce at the trust boundary, 400 on refusal |
| `crates/workspaces/src/engine/compose.rs` | Modify (`render`, ~line 41; add `#[cfg(test)] mod tests`) | Defensive refusal before a bind string is written |
| `bins/agent/src/lib.rs` | Modify (`mkdir_env_mounts` ~line 564; 204 branch ~line 186; `report` ~line 318 and its caller ~line 215) | Defensive refusal in the root process; sleep floor; report its own agent id |
| `bins/server/src/vol_agent.rs` | Modify (`work` ~line 396, `mark_ws_ready` ~line 433, `job_done`/`job_failed` ~line 546–597, `spawn_sweep` ~line 603) | Lease/report/state guards, leader-gated sweep |
| `bins/server/src/boot.rs` | Modify (`build_jobs_state` ~line 19–42) | Take `is_leader` and gate the sweep spawn |
| `bins/server/src/main.rs` | Modify (~line 132) | Pass `app.is_leader()` |
| `tests/vol_agent.rs` | Modify (jobs module: `done_marks_job_done`, the two `failed_*` tests) + add 4 regression tests | HTTP-level proof for H1/H4/H5 |

---

### Task 1: The shared mount-validation helper (C1, part 1)

**Files:**
- Modify: `crates/workspaces/src/model.rs` (`Mount` is at line 197)
- Test: `crates/workspaces/src/model.rs` `#[cfg(test)] mod tests` (new)

**Interfaces:**
- Consumes: `rustic_git_storage::store::valid_segment` (already a dependency —
  `crates/workspaces/Cargo.toml:12`)
- Produces: `pub fn validate_mount(m: &Mount) -> Result<(), String>`

Steps:

- [ ] **Step 1: Write the failing test.** Append to `crates/workspaces/src/model.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::{validate_mount, Mount};

    fn m(folder: &str, path: &str) -> Mount {
        Mount { folder: folder.into(), path: path.into() }
    }

    #[test]
    fn a_mount_folder_is_one_safe_segment() {
        assert!(validate_mount(&m("data", "/data")).is_ok());
        assert!(validate_mount(&m("pg_data-1.2", "/var/lib/postgresql")).is_ok());

        // Every one of these bind-mounts something outside the environment's own subvolume,
        // because `Path::join` drops the base on an absolute component and `..` walks out.
        for bad in ["/", "..", "a/b", "", ".", "../../etc", "/etc", "a:b", "x\0y"] {
            assert!(validate_mount(&m(bad, "/data")).is_err(), "folder {bad:?} must be refused");
        }
    }

    #[test]
    fn a_mount_path_is_absolute_and_colon_free() {
        assert!(validate_mount(&m("data", "/data")).is_ok());
        for bad in ["", "data", "./data", "/data:ro", "/data:/etc:ro", "/data\0"] {
            assert!(validate_mount(&m("data", bad)).is_err(), "path {bad:?} must be refused");
        }
    }
}
```

- [ ] **Step 2: See it fail.** `cargo test -p rustic-git-workspaces model::tests` — expected:
      `error[E0432]: unresolved import ... validate_mount` (compile failure, the function does not
      exist yet).

- [ ] **Step 3: Minimal implementation.** Insert immediately after the `Mount` struct
      (`crates/workspaces/src/model.rs:201`):

```rust
/// A mount is bind-mounted by a ROOT agent, so both halves are a security boundary, not a
/// convenience check: `folder` is joined onto the environment's own subvolume and `path` is
/// concatenated into a `src:dst` string. `Path::join` discards the base when the component is
/// absolute and `..` walks out of it, so anything but a single safe segment hands the caller an
/// arbitrary host path; a `:` in either half splices extra fields (a second mapping, `:ro`) into
/// the bind string. Kept here rather than in `engine::compose` so the runtime that replaces
/// compose can call the same rule.
pub fn validate_mount(m: &Mount) -> Result<(), String> {
    if !rustic_git_storage::store::valid_segment(&m.folder) {
        return Err(format!("mount folder {:?} must be a single name of [A-Za-z0-9._-]", m.folder));
    }
    if !m.path.starts_with('/') || m.path.contains(':') || m.path.contains('\0') {
        return Err(format!("mount path {:?} must be an absolute path with no ':'", m.path));
    }
    Ok(())
}
```

- [ ] **Step 4: See it pass.** `cargo test -p rustic-git-workspaces model::tests` — 2 passed.
      Then `cargo clippy --workspace -- -D warnings`.

- [ ] **Step 5: Commit.**

```sh
git add crates/workspaces/src/model.rs
git commit -m "Add validate_mount, the one rule for a mount folder and path"
```

---

### Task 2: Enforce at the API trust boundary (C1, part 2)

**Files:**
- Modify: `crates/workspaces/src/api.rs` — `create_env`, the emptiness check at line 536
- Test: `crates/workspaces/src/api.rs` `#[cfg(test)] mod tests` (new)

**Interfaces:**
- Consumes: `crate::model::validate_mount`
- Produces: `fn check_mounts(services: &[Service]) -> Result<(), String>`, called by `create_env`

Note for the implementer: `create_env` is the ONLY route accepting user-authored services —
`clone_env` (line ~659) copies `src.services` from a stored, already-validated `Environment`, and
there is no update/PATCH route (`router()`, lines 96–113). Validate in `create_env`; do not invent
an update path.

Steps:

- [ ] **Step 1: Write the failing test.** Append to `crates/workspaces/src/api.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::check_mounts;
    use crate::model::{Mount, Service};

    fn svc(folder: &str, path: &str) -> Service {
        Service {
            name: "web".into(),
            image: "nginx".into(),
            command: vec![],
            env: Default::default(),
            mounts: vec![Mount { folder: folder.into(), path: path.into() }],
        }
    }

    #[test]
    fn create_env_refuses_a_traversing_mount() {
        assert!(check_mounts(&[svc("data", "/data")]).is_ok());
        // The C1 payload: `{"folder": "/", "path": "/host"}` bind-mounts the host root RW into a
        // container whose image the same caller chose.
        for bad in ["/", "..", "a/b", "", "../../root/.ssh", "a:b"] {
            assert!(check_mounts(&[svc(bad, "/host")]).is_err(), "folder {bad:?} must be refused");
        }
        assert!(check_mounts(&[svc("data", "/data:/etc")]).is_err(), "a ':' in path splices a mapping");
        assert!(check_mounts(&[svc("data", "relative")]).is_err());
    }
}
```

- [ ] **Step 2: See it fail.** `cargo test -p rustic-git-workspaces api::tests` — expected:
      `error[E0432]: unresolved import ... check_mounts`.

- [ ] **Step 3: Minimal implementation.** Replace the emptiness check in `create_env`
      (`crates/workspaces/src/api.rs:536-538`):

```rust
    if let Err(e) = check_mounts(&body.services) {
        return Err((StatusCode::BAD_REQUEST, e).into_response());
    }
```

and add above `create_env`:

```rust
/// The trust boundary for mounts: this is the only route that accepts caller-authored services
/// (`clone_env` copies an already-validated doc, and nothing updates services in place), so a
/// mount that gets past here is treated as trusted by a root agent from then on.
fn check_mounts(services: &[Service]) -> Result<(), String> {
    services.iter().flat_map(|s| &s.mounts).try_for_each(crate::model::validate_mount)
}
```

Delete the now-dead "mount folder name must not be empty" comment block above it and the old
`any(... m.folder.is_empty())` line — `validate_mount` subsumes emptiness.

- [ ] **Step 4: See it pass.** `cargo test -p rustic-git-workspaces api::tests` — 1 passed.
      Then `cargo clippy --workspace -- -D warnings`.

- [ ] **Step 5: Commit.**

```sh
git add crates/workspaces/src/api.rs
git commit -m "Refuse a traversing mount folder at the environment API boundary"
```

---

### Task 3: Defence in depth in the agent and the compose renderer (C1, part 3)

**Files:**
- Modify: `crates/workspaces/src/engine/compose.rs` — `render`, line 35–48
- Modify: `bins/agent/src/lib.rs` — `mkdir_env_mounts`, lines 564–574
- Test: `crates/workspaces/src/engine/compose.rs` `#[cfg(test)] mod tests` (new)

**Interfaces:**
- Consumes: `crate::model::validate_mount`
- Produces: nothing new; both functions already return `Result`

Steps:

- [ ] **Step 1: Write the failing test.** Append to `crates/workspaces/src/engine/compose.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::render;
    use crate::model::{EnvState, Environment, Mount, Service};
    use std::path::Path;

    fn env_with(folder: &str, path: &str) -> Environment {
        Environment {
            id: "env-1".into(),
            owner: "alice".into(),
            name: "dev".into(),
            region: "centralindia".into(),
            state: EnvState::Creating,
            placement: None,
            volume: None,
            services: vec![Service {
                name: "web".into(),
                image: "nginx".into(),
                command: vec![],
                env: Default::default(),
                mounts: vec![Mount { folder: folder.into(), path: path.into() }],
            }],
        }
    }

    #[test]
    fn render_refuses_a_mount_that_escapes_the_subvolume() {
        let live = Path::new("/mnt/wspool/vol/env-1/live");
        let ok = render(&env_with("data", "/data"), live).unwrap();
        assert!(ok.contains("/mnt/wspool/vol/env-1/live/volumes/data:/data"));

        // A doc written before the API validated mounts (or a store edited out of band) must not
        // become a host bind here.
        for bad in ["/", "..", "a/b", ""] {
            assert!(render(&env_with(bad, "/host"), live).is_err(), "folder {bad:?} must be refused");
        }
        assert!(render(&env_with("data", "/data:ro"), live).is_err());
    }
}
```

- [ ] **Step 2: See it fail.** `cargo test -p rustic-git-workspaces compose::tests` — expected:
      `assertion failed: render(&env_with("/", "/host"), live).is_err()` (render currently
      succeeds and emits `/:/host`).

- [ ] **Step 3: Minimal implementation.** In `render`, inside the mount loop, before the join:

```rust
        for m in &svc.mounts {
            // Belt to the API's braces: this is the last place before a string becomes a host
            // bind, and the agent that acts on it runs as root.
            crate::model::validate_mount(m).map_err(EngErr::other)?;
            let vol_dir = live.join("volumes").join(&m.folder);
```

Then the same guard in `bins/agent/src/lib.rs::mkdir_env_mounts` (line 568):

```rust
            if seen.insert(m.folder.clone()) {
                // `create_dir_all` on an unvalidated folder is itself the escape — it would
                // happily mkdir -p outside the subvolume before compose ever ran.
                rustic_git_workspaces::model::validate_mount(m)?;
                std::fs::create_dir_all(live.join("volumes").join(&m.folder)).map_err(|e| e.to_string())?;
            }
```

(`mkdir_env_mounts` already returns `Result<(), String>` and `validate_mount` returns
`Result<(), String>`, so `?` needs no mapping.)

- [ ] **Step 4: See it pass.** `cargo test -p rustic-git-workspaces compose::tests` — 1 passed.
      Then `cargo build -p rustic-git-agent && cargo clippy --workspace -- -D warnings`.

- [ ] **Step 5: Commit.**

```sh
git add crates/workspaces/src/engine/compose.rs bins/agent/src/lib.rs
git commit -m "Refuse an escaping mount again in the renderer and the agent"
```

---

### Task 4: Lease only jobs addressed to the polling agent (H1)

**Files:**
- Modify: `bins/server/src/vol_agent.rs` — `work`, line 396
- Test: `tests/vol_agent.rs` (jobs module) — new test

**Interfaces:**
- Consumes: `Job.agent`, `AgentWorkQuery.agent`
- Produces: no signature change

Steps:

- [ ] **Step 1: Write the failing test.** Add to the jobs module in `tests/vol_agent.rs`, after
      `queued_job_is_leased_exactly_once_across_two_pollers`:

```rust
    #[tokio::test]
    async fn an_unassigned_job_is_never_leased_by_a_poller() {
        let (base, store) = setup().await;
        let a1 = register(&base, "vm-1").await;
        // `agent: None` is not "anyone may run it": the scheduler leaves it None when the owner's
        // bound agent is dead, and the sweep clears it on expiry. Running it here would snapshot
        // subvolumes that live on another node.
        store.create_job(&job("job-1")).await.unwrap();

        let resp = reqwest::Client::new()
            .get(format!("{base}/vol-agent/work?agent={a1}"))
            .header(WS_AGENT_HEADER, TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204, "an unassigned job must not be handed out");
        let (got, _) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
        assert_eq!(got.state, JobState::Queued);
        assert!(got.agent.is_none());
    }

    #[tokio::test]
    async fn a_job_assigned_to_another_agent_is_not_leased() {
        let (base, store) = setup().await;
        let a1 = register(&base, "vm-1").await;
        let mut j = job("job-1");
        j.agent = Some("agent-somebody-else".into());
        store.create_job(&j).await.unwrap();

        let resp = reqwest::Client::new()
            .get(format!("{base}/vol-agent/work?agent={a1}"))
            .header(WS_AGENT_HEADER, TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
        let (got, _) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
        assert_eq!(got.state, JobState::Queued);
    }
```

  `queued_job_is_leased_exactly_once_across_two_pollers` must now assign the job first — change
  its `store.create_job(&job("job-1"))` to:

```rust
        let mut j = job("job-1");
        j.agent = Some(a1.clone());
        store.create_job(&j).await.unwrap();
```

  and its final assertion to `assert_eq!(leased.agent.as_deref(), Some(a1.as_str()));` (a2 must
  now get the 204). Keep the "exactly one 200, one 204" assertions as they are — they still hold,
  and they now also pin that the CAS race is between one eligible poller and one ineligible one.

- [ ] **Step 2: See it fail.** `cargo test --test vol_agent an_unassigned_job_is_never_leased`
      — expected: `assertion `left == right` failed: an unassigned job must not be handed out
      left: 200 right: 204`.

- [ ] **Step 3: Minimal implementation.** In `work` (`bins/server/src/vol_agent.rs:396`), replace:

```rust
        let mine = queued.into_iter().find(|(j, _)| j.agent.as_deref().is_none_or(|a| a == q.agent));
```

with:

```rust
        // `agent: None` means "not placed", NOT "free for anyone": the scheduler clears it when
        // the owner's bound agent is dead and the sweep clears it on every expiry, so handing it
        // to whoever polls first runs the job on a node that does not hold the subvolumes. An
        // unplaced job waits for `lease::sweep`'s re-`schedule` pass to bind it (≤30s).
        let mine = queued.into_iter().find(|(j, _)| j.agent.as_deref() == Some(q.agent.as_str()));
```

- [ ] **Step 4: See it pass.** `cargo test --test vol_agent` — the whole file green, including the
      amended `queued_job_is_leased_exactly_once_across_two_pollers`. Then
      `cargo clippy --workspace -- -D warnings`.

- [ ] **Step 5: Commit.**

```sh
git add bins/server/src/vol_agent.rs tests/vol_agent.rs
git commit -m "Lease only jobs addressed to the polling agent"
```

---

### Task 5: Job reports must come from the lease holder (H4)

**Files:**
- Modify: `bins/server/src/vol_agent.rs` — `RegionHint` (line ~438), `job_done` (~546),
  `job_failed` (~570)
- Modify: `bins/agent/src/lib.rs` — `report` (line 318) and its call site (line 215)
- Test: `tests/vol_agent.rs` — new test plus amendments to three existing ones

**Interfaces:**
- Consumes: `?agent=` query parameter on `POST /vol-agent/jobs/{id}/done|failed`
- Produces: both routes become a no-op 200 unless `state == Leased && job.agent == Some(reporter)`

Steps:

- [ ] **Step 1: Write the failing test.** Add to the jobs module in `tests/vol_agent.rs`:

```rust
    #[tokio::test]
    async fn a_report_from_a_stale_lease_holder_is_ignored() {
        let (base, store) = setup().await;
        store.create_job(&job("job-1")).await.unwrap();
        let (mut j, etag) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
        // Attempt 2 is running on agent-b; attempt 1 (agent-a) reports late.
        j.state = JobState::Leased;
        j.agent = Some("agent-b".into());
        store.replace_job(&j, &etag).await.unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/vol-agent/jobs/job-1/failed?agent=agent-a"))
            .header(WS_AGENT_HEADER, TOKEN)
            .json(&json!({"error": "late failure from attempt 1"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "a stale report is a no-op, not an error");
        let (got, _) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
        assert_eq!(got.state, JobState::Leased, "attempt 2 still owns the lease");
        assert_eq!(got.attempts, 0);
        assert!(got.error.is_none());

        // Same for a late `done` — it must not mark a job nobody finished.
        let resp = client
            .post(format!("{base}/vol-agent/jobs/job-1/done?agent=agent-a"))
            .header(WS_AGENT_HEADER, TOKEN)
            .json(&json!({"result": {}}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let (got, _) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
        assert_eq!(got.state, JobState::Leased);
    }

    #[tokio::test]
    async fn a_report_on_a_requeued_job_is_ignored() {
        let (base, store) = setup().await;
        store.create_job(&job("job-1")).await.unwrap(); // Queued, no agent
        let resp = reqwest::Client::new()
            .post(format!("{base}/vol-agent/jobs/job-1/done?agent=agent-a"))
            .header(WS_AGENT_HEADER, TOKEN)
            .json(&json!({"result": {}}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let (got, _) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
        assert_eq!(got.state, JobState::Queued, "a report cannot complete a job that is not leased");
    }
```

  Amend the three existing tests so they report as the lease holder:
  - `done_marks_job_done`: URL becomes `.../jobs/job-1/done?agent=agent-x` (it already sets
    `j.agent = Some("agent-x")` and `state = Leased`).
  - `failed_requeues_until_attempts_exceed_three`: add `j.agent = Some("agent-x".into());` next to
    the existing `j.state = JobState::Leased;`, and use `.../failed?agent=agent-x`.
  - `failed_under_the_retry_budget_goes_back_to_queued`: the job is created `Queued` with no
    agent, so lease it first —

```rust
        let (mut j, etag) = store.get_job("centralindia", "job-1").await.unwrap().unwrap();
        j.state = JobState::Leased;
        j.agent = Some("agent-x".into());
        store.replace_job(&j, &etag).await.unwrap();
```

    and use `.../failed?agent=agent-x`.

- [ ] **Step 2: See it fail.** `cargo test --test vol_agent a_report_from_a_stale_lease_holder`
      — expected: `assertion `left == right` failed: attempt 2 still owns the lease
      left: Queued right: Leased`.

- [ ] **Step 3: Minimal implementation.** In `bins/server/src/vol_agent.rs`, extend the query
      struct (rename `RegionHint` → `ReportQuery`, updating both handlers' signatures):

```rust
#[derive(serde::Deserialize)]
struct ReportQuery {
    #[serde(default)]
    region: Option<String>,
    /// Which agent is reporting. Without it a completion report is unattributable, and an
    /// unattributable report is exactly the stale one we must drop.
    #[serde(default)]
    agent: Option<String>,
}
```

  Add one shared guard next to the handlers:

```rust
/// A completion report is only believable from the agent currently holding the lease. After a
/// lease expiry and requeue, attempt 1's late `failed` would otherwise flip a job attempt 2 has
/// already finished back to `Queued` — a THIRD run of, say, `WsDelete` — and a late `done` would
/// mark a requeued job finished that nobody ran.
fn holds_the_lease(job: &Job, reporter: Option<&str>) -> bool {
    job.state == JobState::Leased && reporter.is_some() && job.agent.as_deref() == reporter
}
```

  Then in `job_done`, right after the `get_job`:

```rust
    if !holds_the_lease(&job, q.agent.as_deref()) {
        // 200, not 409: the reporter is done either way, and a retry would not help it.
        return Ok(Json(job).into_response());
    }
```

  and the identical block in `job_failed` (before `job.attempts += 1`). Keep the existing
  `job_failed` requeue body as it is — note in a comment that clearing `agent` there leaves the
  job unplaced until `lease::sweep`'s re-`schedule` pass binds it, which is the H1 contract:

```rust
        job.state = JobState::Queued;
        // Unplaced, deliberately: the next `lease::sweep` beat re-`schedule`s it onto a live
        // agent for the owner. `work` no longer hands out an unplaced job (see its comment).
        job.agent = None;
```

  On the agent side, `bins/agent/src/lib.rs`, thread the id through `report`:

```rust
async fn report(client: &reqwest::Client, api: &str, token: &str, agent: &str, job_id: &str, outcome: Result<serde_json::Value, String>) {
    let (path, body) = match outcome {
        Ok(result) => (format!("{api}/vol-agent/jobs/{job_id}/done?agent={agent}"), json!({"result": result})),
        Err(error) => {
            eprintln!("agent: job {job_id} failed: {error}"); // ponytail: eprintln
            (format!("{api}/vol-agent/jobs/{job_id}/failed?agent={agent}"), json!({"error": error}))
        }
    };
```

  and at the call site (line ~215), clone the id into the spawned task next to `cfg_api`/`cfg_tok`:

```rust
        let cfg_agent = agent_id.clone();
```

  then `report(&client, &cfg_api, &cfg_tok, &cfg_agent, &job_id, outcome).await;`

- [ ] **Step 4: See it pass.** `cargo test --test vol_agent` (all green, including the three
      amended tests) and `cargo build -p rustic-git-agent`. Then
      `cargo clippy --workspace -- -D warnings`.

- [ ] **Step 5: Commit.**

```sh
git add bins/server/src/vol_agent.rs bins/agent/src/lib.rs tests/vol_agent.rs
git commit -m "Accept a job report only from the agent holding the lease"
```

---

### Task 6: A deleted workspace is never resurrected (H5)

**Files:**
- Modify: `bins/server/src/vol_agent.rs` — `mark_ws_ready` (line 433) and, for the same reason,
  `mark_ws_stopped`
- Test: `tests/vol_agent.rs` — new test

**Interfaces:**
- Consumes: `WsState::Deleted`
- Produces: no signature change

Steps:

- [ ] **Step 1: Write the failing test.** Add to the jobs module in `tests/vol_agent.rs` (needs
      `use rustic_git_workspaces::model::{Workspace, WsState};` added to the module's imports —
      check the exact `Workspace` field list in `crates/workspaces/src/model.rs` and fill every
      field; `region: "centralindia"`, `owner: "alice"`, `id: "ws-1"` to match `job()`'s payload):

```rust
    #[tokio::test]
    async fn a_deleted_workspace_is_not_resurrected_by_an_in_flight_job() {
        let (base, store) = setup().await;
        let mut w = ws_doc("alice", "ws-1");
        w.state = WsState::Deleted; // user deleted it while WsCreate was leased
        store.create_ws(&w).await.unwrap();

        let mut j = job("job-1");
        j.payload = json!({"workspace": "ws-1", "owner": "alice"});
        j.state = JobState::Leased;
        j.agent = Some("agent-x".into());
        store.create_job(&j).await.unwrap();

        let resp = reqwest::Client::new()
            .post(format!("{base}/vol-agent/jobs/job-1/done?agent=agent-x"))
            .header(WS_AGENT_HEADER, TOKEN)
            .json(&json!({"result": {}}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let (got, _) = store.get_ws("alice", "ws-1").await.unwrap().unwrap();
        assert_eq!(got.state, WsState::Deleted, "delete wins, same rule as mark_env_state");
    }
```

  Add the `ws_doc` fixture next to `job()` in the same module, mirroring `job()`'s style.

- [ ] **Step 2: See it fail.** `cargo test --test vol_agent a_deleted_workspace_is_not_resurrected`
      — expected: `assertion `left == right` failed: delete wins, same rule as mark_env_state
      left: Ready right: Deleted`.

- [ ] **Step 3: Minimal implementation.** In `mark_ws_ready`, inside the CAS loop after the
      `get_ws`:

```rust
        // Delete wins — the same rule `mark_env_state` already states. A create/clone leased when
        // the user deleted the workspace would otherwise flip the doc back to Ready with no
        // volume behind it, and it reappears in listings as live.
        if w.state == WsState::Deleted {
            return;
        }
```

  Add the identical guard to `mark_ws_stopped` (a `WsStop` landing after a delete has the same
  problem, and it is two lines).

- [ ] **Step 4: See it pass.** `cargo test --test vol_agent` — all green. Then
      `cargo clippy --workspace -- -D warnings`.

- [ ] **Step 5: Commit.**

```sh
git add bins/server/src/vol_agent.rs tests/vol_agent.rs
git commit -m "Refuse to resurrect a deleted workspace from a job report"
```

---

### Task 7: Run the requeue sweep on the leader only (P6)

**Files:**
- Modify: `bins/server/src/vol_agent.rs` — `spawn_sweep` (line 603)
- Modify: `bins/server/src/boot.rs` — `build_jobs_state` (lines 19–42)
- Modify: `bins/server/src/main.rs` — line 132

**Interfaces:**
- Consumes: `App::is_leader()` (`crates/app/src/lib.rs:214`)
- Produces: `pub async fn build_jobs_state(is_leader: bool) -> Result<Arc<JobsState>>`

No test: this is leadership-by-name plumbing with no branch worth freezing, matching how
`lanes.rs:81` gates `prune_once`. Verification is the compile plus the existing suite.

Steps:

- [ ] **Step 1: Implementation.** In `boot.rs`:

```rust
pub async fn build_jobs_state(is_leader: bool) -> Result<Arc<JobsState>> {
```

and the spawn site:

```rust
    // One sweeper for the fleet. The sweep is a CAS race by construction, so N replicas running it
    // is safe but pure waste — same leader-by-name gate `lanes.rs` puts on `prune_once`.
    if let (Some(s), true) = (&store, is_leader) {
        crate::vol_agent::spawn_sweep(s.clone());
    }
```

  In `main.rs:132`:

```rust
    let jobs = rustic_git_server::boot::build_jobs_state(app.is_leader()).await?;
```

  (`app` is constructed just above, so the ordering already works.)

- [ ] **Step 2: Verify.** `cargo build --workspace && cargo test` — nothing should change; then
      `cargo clippy --workspace -- -D warnings`.

- [ ] **Step 3: Commit.**

```sh
git add bins/server/src/boot.rs bins/server/src/main.rs
git commit -m "Run the requeue sweep on the leader only"
```

---

### Task 8: Sleep floor on the agent's empty poll (P7)

**Files:**
- Modify: `bins/agent/src/lib.rs` — the 204 branch, line ~186

**Interfaces:** none.

No test: a one-line sleep with no branch to assert. Verification is the compile.

Steps:

- [ ] **Step 1: Implementation.** Replace:

```rust
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            continue;
        }
```

with:

```rust
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            // The server normally holds this poll for its full window, but any proxy with a
            // shorter idle timeout turns an unfloored 204 loop into a busy-spin against Cosmos.
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            continue;
        }
```

- [ ] **Step 2: Verify.** `cargo build -p rustic-git-agent && cargo clippy --workspace -- -D warnings`.

- [ ] **Step 3: Commit.**

```sh
git add bins/agent/src/lib.rs
git commit -m "Floor the agent's empty work poll at one second"
```

---

## Final verification

- [ ] `cargo clippy --workspace -- -D warnings`
- [ ] `cargo test`
- [ ] `git log --oneline -8` — eight commits, imperative sentence case, no tool attribution.
