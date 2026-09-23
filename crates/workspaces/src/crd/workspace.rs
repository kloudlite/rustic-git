//! `Workspace`: spec (image, packages and their locks, resources, storage source, attachment) and
//! the status the node controller writes back, with the condition names both sides share.

use super::*;


/// What the user asked of a parent object's storage. This is what the API used to author directly
/// as a `VolumeSpec`; the parent's reconciler is what turns it into a `Volume` now.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStorage {
    pub quota_gb: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<VolumeSource>,
}


#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite.io",
    version = "v1alpha1",
    kind = "Workspace",
    plural = "workspaces",
    shortname = "ws",
    status = "WorkspaceStatus",
    // Placement is a FACT the controllers establish, so it lives in status — and a status path is
    // a legal selectable field (only metadata is forbidden, and arrays are not allowed). An empty
    // value is what the unplaced watch selects on.
    selectable = ".status.nodeName",
    // `parents_of_volume` asks "what is running on this volume" on every snapshot and volume
    // delete; without this it was two full-cluster lists per question. An unset value indexes as
    // the empty string, which is what makes "not placed yet" its own queryable set.
    selectable = ".status.volumeRef",
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"Node","type":"string","jsonPath":".status.nodeName"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSpec {
    pub owner: String,
    /// The team this workspace is made in, or empty for the owner's personal namespace. A
    /// workspace's Kubernetes namespace is one per (team, owner) pair — see `ws_namespace` — so
    /// the same person's work in two teams never shares a namespace, a NetworkPolicy or a Secret.
    #[serde(default)]
    pub team: String,
    pub name: String,
    pub region: String,
    pub image: String,
    /// Optional so an object created before this field existed still PARSES, rather than the
    /// controller 422ing every Workspace it tries to write. A missing one is a permanent
    /// `NoStorage` failure on the reconcile — nothing can build a disk without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<WorkspaceStorage>,
    pub desired_state: DesiredState,
    #[serde(default)]
    pub resources: PodResources,
    /// The package list, written by the API. Lives on `spec`, not a file in the workspace's own
    /// subvolume: one object, one list — a clone copies it for free along with the rest of spec,
    /// and a restore (which grafts onto a past snapshot of the volume) never touches it, because
    /// spec is not part of what a restore replaces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
    /// What each `name@version` entry in `packages` resolved to. Desired state on purpose: the
    /// agent has no internet, so the api resolves once and the answer rides in the object — see
    /// `Lock`. A bare entry (no `@`) has no lock.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locks: Vec<Lock>,
    /// RETIRED (2026-09-14): the environment is chosen per space now (`SpaceEnvironment`). Kept
    /// parseable one release: the api's migration beat copies it into a `SpaceEnvironment` and
    /// clears it, and the agent falls back to it only while the space cache is known and empty.
    /// Nothing else writes it.
    ///
    /// The environment whose services this workspace resolves by bare name, or `None`.
    ///
    /// One, not a list: bare-name resolution has to be unambiguous, and two attached environments
    /// both exposing `db` would let search-domain order silently pick the winner.
    ///
    /// Written only by `/v1` — the agent's admission policy forbids it writing spec, and a stale
    /// id here is not an error: the reconciler treats a missing or wrong-region environment as
    /// unattached rather than leaving a grant behind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached_environment: Option<String>,
    /// `Some` = this workspace IS the owner's bench in `team`. `is_bench` is the only predicate
    /// for that anywhere — never the name prefix, a label or the container list, all of which a
    /// restored or hand-edited object can carry without being one. Written only by `/v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bench: Option<BenchOptions>,
    /// Membership pause, on EVERY workspace (it was `Bench.spec.access`): `Paused` means the owner
    /// may not use it, so no pod. The api's membership beat writes it; the gateway and `/v1` read
    /// it.
    #[serde(default)]
    #[schemars(schema_with = "access_schema")]
    pub access: Access,
    /// The subagent trees asked for on this workspace, written ONLY by `/v1` — the agent's
    /// admission policy forbids it writing spec, and `status.trees` is its half of the pair.
    /// A tree is a writable nested btrfs subvolume at `{ws}/.agents/{name}`, so it costs no
    /// quota (bytes on a volume the owner already pays for) and never travels: `btrfs send`
    /// skips nested subvolumes, which is why a replica or a restore starts with none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trees: Vec<TreeSpec>,
}


/// One asked-for tree. There is no `from`: a tree is cut from the workspace's own working
/// directory and there is nothing else to cut from — a second source would be a clone, which is a
/// different verb with a different cost.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TreeSpec {
    pub name: String,
    /// RFC 3339, stamped by `/v1` at the ask. Creation order is what gives each tree its port
    /// block (spec §4.6), so it is written once and never touched again.
    pub created: String,
}


/// What the node has actually cut, written only by the agent through `/status`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TreeStatus {
    pub name: String,
    pub path: String,
    pub ready: bool,
    /// Why it is not ready, when it is not. A snapshot failure is retried on the next pass —
    /// this is what the person reads in the meantime, never a reason to give up on the ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}


/// The one charset a tree name may use: it becomes a directory under `.agents/` and a segment of a
/// URL, so `.`, `/` and the empty string are all path traversal wearing a name.
pub fn tree_name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}


/// The directory nested tree subvolumes live in, relative to the workspace root. In the global
/// gitignore, and excluded from the main tree's own confinement — the one place under its root
/// the main session may not look.
pub const TREES_DIR: &str = ".agents";


/// Where a tree lives inside the POD, which is the path a caller can act on: the worktree volume
/// IS the home (2026-09-22 ruling), so its `.agents` sits at `~/.agents` in every workspace. The
/// path once carried the workspace's name and reporting the id instead produced a path that did
/// not exist (R-D21, 2026-09-18); with one home per pod there is no name left to get wrong.
///
/// Note this is NOT where the agent cuts the subvolume: that is under the pool
/// (`Engine::tree_dir`, `{pool}/vol/{volume}/live/{ws-id}/.agents/{name}`). The node writes the
/// path a person or a tool server would use, never its own.
pub fn tree_path(name: &str) -> String {
    format!("{}/{TREES_DIR}/{name}", crate::k8s::HOME_DIR)
}


/// The compiled-in ceiling on live trees per workspace, the floor under
/// `ClusterSettings.trees_per_workspace`. Eight: a person reviewing eight parallel agents' diffs
/// is already past what anyone reads, and each one costs a build's worth of CPU in a pod sized
/// for one.
pub const TREES_PER_WORKSPACE: u32 = 8;


/// The bench-only half of a workspace's spec.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BenchOptions {
    #[serde(default)]
    pub model: String,
    /// RFC 3339, written by `/v1` when a client asks for a tunnel to a sleeping bench. A pod is
    /// wanted again only while this is later than `status.idleSince`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_at: Option<String>,
}


/// Whether the owner may use this workspace at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Access {
    #[default]
    Full,
    /// `readOnly` is the retired departed state; stored objects still carry it and parse as this.
    #[serde(alias = "readOnly")]
    Paused,
}


/// The PUBLISHED schema carries a third value the Rust type does not: `readOnly`, the retired
/// departed state. `serde(alias)` fixes the READER, but the API server validates the whole object
/// on every write — a `/status` patch included — so a stored object still carrying `readOnly`
/// would be rejected and wedge; it must stay writable until every region is confirmed to hold
/// none, and then this goes. Deliberately NOT a third Rust variant: every reader compares against
/// `Full`/`Paused`, and a new arm is one more place for a value to fall through to "full".
/// The value list is read off the derived schema rather than hand-written, so a variant added to
/// `Access` still publishes; the flattening to one `enum` is what kube does to schemars'
/// per-variant `oneOf` anyway, so the published shape is unchanged apart from the extra value.
pub(crate) fn access_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let derived = <Access as JsonSchema>::json_schema(generator);
    let branches = derived.get("oneOf").and_then(serde_json::Value::as_array).expect("a unit-variant enum");
    let mut values: Vec<serde_json::Value> = branches
        .iter()
        .map(|b| match (b.get("const"), b.get("enum").and_then(|e| e.as_array()).and_then(|e| e.first())) {
            (Some(v), _) | (None, Some(v)) => v.clone(),
            _ => panic!("a unit-variant enum branch names one value"),
        })
        .collect();
    values.push("readOnly".into());
    serde_json::from_value(serde_json::json!({"type": "string", "enum": values})).expect("static schema literal")
}


/// `spec.bench.is_some()` — THE predicate for "this workspace is a bench".
pub fn is_bench(w: &Workspace) -> bool {
    w.spec.bench.is_some()
}


/// Whether a pod should exist now. An ordinary workspace runs while it is asked to; a bench also
/// sleeps when idle, and a wake only counts if it came after the sleep it is meant to end.
pub fn wants_pod(w: &Workspace) -> bool {
    if w.spec.desired_state != DesiredState::Running {
        return false;
    }
    let Some(bench) = w.spec.bench.as_ref() else {
        return true;
    };
    if w.spec.access == Access::Paused {
        return false;
    }
    let Some(idle_since) = w.status.as_ref().and_then(|s| s.idle_since.as_deref()) else {
        return true;
    };
    let Ok(idle_since) = chrono::DateTime::parse_from_rfc3339(idle_since) else {
        return true;
    };
    match bench.wake_at.as_deref().and_then(|w| chrono::DateTime::parse_from_rfc3339(w).ok()) {
        Some(wake_at) => wake_at > idle_since,
        None => false,
    }
}


/// `Ready=False` reason: a bench slept because nothing used it. Not a failure — the next
/// connection wakes it.
pub const BENCH_IDLE: &str = "Idle";

/// `Ready=False` reason: another node still holds this owner's bench folder.
pub const FOLDER_LOCKED: &str = "FolderLocked";

/// `Ready=False` reason: the bench's sessions container keeps dying and the kubelet is backing
/// off. The MESSAGE carries what it died of, which is the only place the cause appears — without
/// this the workspace sat in `creating` for its whole ceiling and then reported only that it
/// never came up (2026-09-18).
pub const BENCH_CRASH_LOOPING: &str = "BenchCrashLooping";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatus {
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
    /// Where this object runs NOW. Empty means unplaced, which is exactly what the placement
    /// watch's `status.nodeName=` field selector matches.
    #[serde(default)]
    pub node_name: String,
    /// The child `Volume`, reported rather than wished for: the reconciler creates it and then
    /// says so here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    /// The package profile actually converged, reported rather than wished for — `spec.packages`
    /// carries the list the reconciler last saw; this is what building it produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages: Option<PackagesStatus>,
    /// The pod's SSH public host key, reported by the node once sshd's key exists. The CLI pins
    /// it in `known_hosts`, so an absent one means "no session yet", never "trust on first use".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_host_key: Option<String>,
    /// The snapshot id this worktree is checked out on right now. Written ONLY by the node actually
    /// running the pod — no other node can observe it, and a stale value here is exactly what
    /// "the pod moved and hasn't reconciled yet" looks like, never a fact anyone else may act on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Bench only: the `finishedAt` of the pod that exited idle; cleared when a pod is created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_since: Option<String>,
    /// The trees the node has actually cut, reported rather than wished for. A row with no
    /// matching `spec.trees` entry is one the agent still has to delete; a spec entry with no row
    /// here is one it still has to cut.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trees: Vec<TreeStatus>,
}


/// What the reconciler last saw and built from `spec.packages`: `observed` and `observed_hash`
/// are the LIST as of the last pass (the hash is the idempotency key — a rebuild is skipped when
/// it still matches), while `profile` is the Nix store path the profile on disk actually
/// resolved to.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PackagesStatus {
    /// The platform's base set the profile was built with, on top of `observed`. Reported so a
    /// page can show what every workspace gets without asking the node which env it runs with.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nixpkgs: Option<String>,
    /// What `spec.locks` was when this profile was built — the observed half of the lock, so the
    /// web can show `nodejs@20 -> 20.20.2` without reading spec and guessing whether the node has
    /// caught up with it yet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locked: Vec<LockedStatus>,
}


/// The reported shape of a `Lock`: what the person asked for, what it became, and where from.
/// Deliberately narrower than `Lock` — a store path is a build detail nobody reads off status.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LockedStatus {
    pub entry: String,
    pub version: String,
    pub rev: String,
}


/// Condition type set once `status.packages` reflects a successful build of `spec.packages` —
/// named here, not in the agent, because it describes a status field this file owns rather than
/// a controller-local fact like `NAMESPACE_READY`.
pub const PACKAGES_READY: &str = "PackagesReady";


/// `PackagesReady=False` reason: a lock's store path is not in `cache.nixos.org`. The agent never
/// builds from source, so this is terminal until somebody picks a version that has a binary.
pub const PKG_NOT_CACHED: &str = "NotCached";


/// `PackagesReady=False` reason: an `@` entry arrived with no lock. `/v1` never writes one — this
/// is an operator having edited spec with kubectl, and the agent refuses to guess a version.
pub const PKG_UNRESOLVED: &str = "Unresolved";


/// Condition type carrying the environment a workspace is attached to, in its MESSAGE (the bare
/// id). Named here rather than in the agent because `/v1` reads it back too: it is the only record
/// of which environment's namespace holds a workspace's ingress half once `spec` has been cleared.
pub const ATTACHED: &str = "Attached";


/// The environment whose namespace holds this workspace's ingress half: what the spec asks for, or
/// — once a detach has already cleared it — what the last converged pass recorded. Both, because a
/// detach on a STOPPED workspace never reaches a reconcile, so `/v1` is the only thing left that
/// can collect the grant.
pub fn attached_environment(w: &Workspace) -> Option<String> {
    if let Some(env) = w.spec.attached_environment.clone() {
        return Some(env);
    }
    w.status
        .as_ref()?
        .conditions
        .iter()
        .find(|c| c.type_ == ATTACHED && c.status == "True")
        .map(|c| c.message.clone())
        .filter(|m| !m.is_empty())
}


#[cfg(test)]
mod tests {
    use super::*;

    fn ws(bench: bool) -> Workspace {
        let mut spec: WorkspaceSpec = serde_json::from_value(serde_json::json!({
            "owner": "alice", "name": "w", "region": "r", "image": "i", "desiredState": "running"
        }))
        .unwrap();
        if bench {
            spec.bench = Some(BenchOptions::default());
        }
        Workspace::new("ws-1", spec)
    }

    /// A spec written before any of this existed must still parse — an object that 422s on read is
    /// a workspace no controller can reconcile again.
    #[test]
    fn a_spec_without_bench_parses_as_an_ordinary_full_access_workspace() {
        let w = ws(false);
        assert!(!is_bench(&w));
        assert_eq!(w.spec.access, Access::Full);
        assert!(w.spec.bench.is_none());
        assert!(wants_pod(&w));
    }

    #[test]
    fn a_stored_readonly_access_parses_as_paused_and_stays_writable() {
        let schema = serde_json::to_value(access_schema(&mut schemars::SchemaGenerator::default())).unwrap();
        assert_eq!(schema["enum"], serde_json::json!(["full", "paused", "readOnly"]), "the legacy value stays writable");
        let s: WorkspaceSpec = serde_json::from_value(serde_json::json!({
            "owner": "alice", "name": "w", "region": "r", "image": "i", "desiredState": "running", "access": "readOnly"
        }))
        .unwrap();
        assert_eq!(s.access, Access::Paused);
        assert_eq!(serde_json::to_value(s.access).unwrap(), "paused");
    }

    /// The truth table `bench_wants_pod` shipped with, now on the Workspace side.
    #[test]
    fn a_bench_pod_is_wanted_while_running_and_awake_or_woken_after_it_slept() {
        let mut w = ws(true);
        assert!(wants_pod(&w), "never slept");
        w.status = Some(WorkspaceStatus { idle_since: Some("2026-09-13T10:00:00Z".into()), ..Default::default() });
        assert!(!wants_pod(&w), "asleep and nobody asked");
        let wake = |w: &mut Workspace, at: &str| w.spec.bench.as_mut().unwrap().wake_at = Some(at.into());
        wake(&mut w, "2026-09-13T09:59:59Z");
        assert!(!wants_pod(&w), "a wake from before it slept is spent");
        wake(&mut w, "2026-09-13T10:00:01Z");
        assert!(wants_pod(&w));
        w.spec.access = Access::Paused;
        assert!(!wants_pod(&w), "a wake never starts a paused bench");
        w.spec.access = Access::Full;
        w.spec.desired_state = DesiredState::Stopped;
        assert!(!wants_pod(&w), "stopped refuses a wake");
    }

    /// An ordinary workspace has no idle clock: a stale `idleSince` (from a bench that stopped
    /// being one) must never stop it starting.
    #[test]
    fn an_ordinary_workspace_ignores_the_idle_clock() {
        let mut w = ws(false);
        w.status = Some(WorkspaceStatus { idle_since: Some("2026-09-13T10:00:00Z".into()), ..Default::default() });
        assert!(wants_pod(&w));
        w.spec.desired_state = DesiredState::Stopped;
        assert!(!wants_pod(&w));
    }

    /// `is_bench` is the only predicate, so no other minter may produce a name that reads like one:
    /// `api::rid("ws")` and the probes' run names are all `ws-`/`run-`, never `bench-`.
    #[test]
    fn no_workspace_name_generator_yields_a_bench_prefix() {
        assert!(bench_id("alice", "acme").starts_with("bench-"));
        for id in ["ws-0123456789abcdef", "run-fast-1-clone", "env-abc"] {
            assert!(!id.starts_with("bench-"), "{id}");
        }
    }
}
