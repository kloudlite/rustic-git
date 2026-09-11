//! `Snapshot`: a push (kept until deleted) or a sync point (transient, owned by the working copy),
//! the frozen `SnapshotState` a restore defaults to, and the package locks it carries. The names
//! and labels the agent and the api agree on for stop cuts and sync points live here too.

use super::*;


/// One immutable cut — a snapshot or a sync point: a btrfs RO subvolume, recorded as a CR before the snapshot is cut so a
/// retry finds the object and continues rather than orphaning a subvolume.
///
/// Never patched once `status.phase == Ready` — a `Snapshot` is a fact about the past, and the
/// only two things that ever remove one are an explicit delete and GC, same discipline as a
/// registry blob. Replaces the older `SnapshotRequest` push-as-annotation kind, which the cutover
/// task has since deleted — this is the only push record left.
#[derive(CustomResource, Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "kloudlite.io",
    version = "v1alpha1",
    kind = "Snapshot",
    plural = "snapshots",
    shortname = "snp",
    status = "SnapshotStatus",
    selectable = ".spec.volume",
    printcolumn = r#"{"name":"Volume","type":"string","jsonPath":".spec.volume"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#,
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotSpec {
    pub volume: String,
    pub owner: String,
    /// The Workspace/Environment id whose worktree this cut is taken FROM — a volume can have
    /// more than one worktree (a workspace plus a clone still attached, say), so `spec.volume`
    /// alone does not say which one to snapshot; the creator (`/push`) names it. The
    /// snapshot reconciler only acts when THIS field's worktree is the one running on its node.
    /// Required, no default: `Snapshot` is a brand-new, flag-gated kind — there are no stored
    /// objects predating this field, so the usual back-compat exemption does not apply here.
    pub worktree: String,
    /// The parent cut's name, or empty for a root. Order comes ONLY from this chain — nothing
    /// reads creation timestamps to reconstruct history.
    #[serde(default)]
    pub parent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// A sync point, not a push: cut by the agent's sync beat (or a stop) from a live worktree so a
    /// replica holds its latest state. Never a `parent` of anything, never a worktree's `head`, and
    /// retained ONE per worktree — see `snapshot::retain`. `push` never sets this, which is the
    /// whole distinction: `!transient` IS a snapshot (`Snapshot::is_snapshot`), and it is the only
    /// one — an older build wrote a second flag alongside it, which serde ignores on the objects
    /// stored while it existed.
    #[serde(default)]
    pub transient: bool,
    /// Absent only on a snapshot cut before 2026-09-03; every reader falls back for `None`.
    ///
    /// Schema is hand-written as free-form JSON (`preserve_unknown_state`), not `SnapshotState`'s
    /// own derived schema: kube-core's CRD generation flattens an internally-tagged enum's `oneOf`
    /// branches into one object and panics when a shared property (`kind`) carries a different
    /// `const` per branch — which is the entire point of a tag. A hand-written *discriminated*
    /// schema would drift from the type the moment a variant changes, so this stays a plain
    /// `x-kubernetes-preserve-unknown-fields` object instead of trying to describe the union.
    /// `serde`'s view of the Rust type is untouched, so round-tripping is exact; only the
    /// *published* OpenAPI schema for this field is permissive. Because the schema can't validate
    /// it, a value that doesn't parse as `SnapshotState` must not fail the whole `Snapshot` read —
    /// `lenient_state` drops it to `None` (with a `tracing::warn!`) rather than taking the agent's
    /// list/watch down with it.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "lenient_state")]
    #[schemars(schema_with = "preserve_unknown_state")]
    pub state: Option<SnapshotState>,
}


pub(super) fn preserve_unknown_state(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
    serde_json::from_value(serde_json::json!({
        "type": "object",
        "x-kubernetes-preserve-unknown-fields": true
    }))
    .expect("static schema literal")
}


/// `state` is unvalidatable by the published schema (see `preserve_unknown_state`), so a value
/// that doesn't parse as `SnapshotState` — hand-edited, or written by some future variant this
/// build doesn't know — must not fail the whole `Snapshot` read. Every reader already treats
/// `None` as "no frozen state, fall back to defaults", which is exactly right for "couldn't read
/// it" too. The trap this sets for tests: a PARTIAL value (`resources: {}` with `PodResources`'
/// required fields missing) also becomes `None`, silently — a fixture that meant "a workspace
/// state" reads as "no state", and a test asserting on the kind passes or fails for the wrong
/// reason. Build fixtures with every field.
pub(super) fn lenient_state<'de, D>(deserializer: D) -> Result<Option<SnapshotState>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw.and_then(|v| match serde_json::from_value(v.clone()) {
        Ok(state) => Some(state),
        Err(e) => {
            tracing::warn!(error = %e, value = %v, "snapshot.state.invalid");
            None
        }
    }))
}


/// The workspace quota when nothing names one (`FALLBACK_QUOTA_GB` in `api.rs`, the console's
/// default) and `SnapshotState::of_workspace`'s fallback for a legacy object. 50, not the 20 a
/// source tree needs: since 2026-09-11 build output lives INSIDE the tree (`{ws}/.cache/`,
/// `login_env`), so the number has to hold a Rust `target/` or a `node_modules/` as well.
pub const DEFAULT_WS_QUOTA_GB: u64 = 50;
/// `default_env_quota()`'s value in `api.rs` — both the `NewEnvironment.quota_gb` request-body
/// default (an environment created without one gets this) and `SnapshotState::of_environment`'s
/// fallback for a legacy `spec.storage`-less object; was already `20`, named here to share it.
pub const DEFAULT_ENV_QUOTA_GB: u64 = 20;


/// What a `name@version` package entry resolved to, frozen at the moment `/v1` wrote the spec.
///
/// DESIRED state, not observed: only the api may write spec, and it is the only tier with
/// outbound access — the agent has no internet and cannot ask an index what `nodejs@20` means.
/// So the answer travels in the object, and every node builds the same bytes forever after.
///
/// `store_path` empty means "mirror-resolved": the mirror index carries a revision but no store
/// path, so the agent evaluates `rev#attr_path` instead of substituting a path directly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Lock {
    /// The entry as the person typed it (`nodejs@20`) — the cache key and what the web shows.
    pub entry: String,
    /// The concrete version resolved to (`20.20.2`).
    pub version: String,
    /// The nixpkgs attribute (`nodejs_20`) at `rev`.
    pub attr_path: String,
    /// The nixpkgs revision that built this version.
    pub rev: String,
    /// The `/nix/store/...` output, or empty for a mirror lock — see the type doc. `default` so a
    /// lock written before this field existed still parses.
    #[serde(default)]
    pub store_path: String,
    /// When the index answered, RFC3339. Kept so an update pass can tell a stale lock from a fresh
    /// one without asking the index again.
    pub resolved_at: String,
    pub source: LockSource,
}


/// Which index answered. Recorded because the two differ in what they can give: Nixhub carries a
/// store path (substitute directly), the mirror does not (evaluate the revision).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LockSource {
    Nixhub,
    Mirror,
}


/// What the parent WAS when this cut was taken, frozen beside the bytes. A restore defaults to
/// it, which is the whole reason it exists: last month's files with today's image is not last
/// month's workspace. A copy, never a reference — later edits to the parent leave it alone.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SnapshotState {
    #[serde(rename_all = "camelCase")]
    Workspace {
        image: String,
        packages: Vec<String>,
        /// The locks frozen with `packages`, so a restore rebuilds the exact versions this cut
        /// ran, not whatever the index resolves to today. `default` — a cut taken before locks
        /// existed simply has none.
        #[serde(default)]
        locks: Vec<Lock>,
        resources: PodResources,
        quota_gb: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attached_environment: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Environment {
        services: Vec<crate::model::Service>,
        quota_gb: u64,
    },
}


impl SnapshotState {
    pub fn of_workspace(w: &Workspace) -> Self {
        SnapshotState::Workspace {
            image: w.spec.image.clone(),
            packages: w.spec.packages.clone(),
            locks: w.spec.locks.clone(),
            resources: w.spec.resources.clone(),
            quota_gb: w.spec.storage.as_ref().map(|s| s.quota_gb).unwrap_or(DEFAULT_WS_QUOTA_GB),
            attached_environment: w.spec.attached_environment.clone(),
        }
    }
    pub fn of_environment(e: &Environment) -> Self {
        SnapshotState::Environment {
            services: e.spec.services.clone(),
            quota_gb: e.spec.storage.as_ref().map(|s| s.quota_gb).unwrap_or(DEFAULT_ENV_QUOTA_GB),
        }
    }
}


#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotStatus {
    /// `Working` until the btrfs subvolume is actually cut; `Ready` is the point past which the
    /// object is immutable.
    pub phase: Phase,
    /// When `phase` became `Ready`, RFC3339 — `lastTransitionTime` on a condition would do, but a
    /// `Snapshot` has no conditions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<String>,
}


/// The message an older build stamped on a migration baseline, back when a baseline was written as
/// an ordinary record. Matched by shape rather than migrated: the records are already on the
/// cluster, and a migration job to rewrite them is more machinery than one predicate.
pub(super) const LEGACY_BASELINE_MESSAGE: &str = "migration baseline";


impl Snapshot {
    /// A push, as opposed to a sync point — the one distinction there is. Everything that keeps a
    /// record (retention, `cleanup_parent`, the volume listing, `delete_snapshot`) asks this.
    ///
    /// A MIGRATION BASELINE is not a push either, whoever wrote it: nobody asked for it, it exists
    /// only to seed replication of a pre-model volume, and treating one as a snapshot would keep
    /// its Volume alive forever after the workspace was deleted. Baselines are written as sync
    /// points now (`migrate_and_seed_baseline`); the shape match is for the ones already stored.
    pub fn is_snapshot(&self) -> bool {
        !self.spec.transient && !self.is_legacy_baseline()
    }

    // ponytail: a baseline is recognised by SHAPE — a root record of the volume's own worktree
    // carrying exactly that message — because the records are already stored and a rewrite job is
    // more machinery than a predicate. Ceiling: a person's very first push, on the volume's own
    // worktree, whose message is exactly "migration baseline", reads as one and would be deleted
    // with its parent. Upgrade path: a `baseline: true` field on new records, and this shape match
    // kept only for the pre-field ones.
    fn is_legacy_baseline(&self) -> bool {
        self.spec.parent.is_empty()
            && self.spec.worktree == self.spec.volume
            && self.spec.message.as_deref() == Some(LEGACY_BASELINE_MESSAGE)
    }
}


/// `{volume}-{8 hex}` — CR-first naming: minted before the btrfs snapshot is cut, so a retried
/// create finds this same object rather than a new one. Random, not sequential, because ORDER
/// comes only from `SnapshotSpec::parent`, never from the name.
pub fn snapshot_name(volume: &str) -> String {
    format!("{volume}-{}", short_hex())
}


/// Four random bytes as 8 lowercase hex characters — the same `rand`-backed shape `api::rid` uses
/// for every other object id in this crate, kept local because a `Snapshot` name is not prefixed.
pub fn short_hex() -> String {
    use rand::RngCore;
    let mut b = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut b);
    kloudlite_core::hex(&b)
}


/// Labels every `Snapshot`/`VolumeReplica` create site stamps: `spec.volume`/`spec.owner` restated
/// as labels so a watch or a list (the e2e's `-l kloudlite.io/volume=...`, `/v1`'s own reads) can
/// select on them — a label cannot be queried out of an arbitrary spec field. A VIEW, same rule as
/// every other label in this file: `spec` stays the truth, this is never read for authorization.
pub fn snapshot_labels(owner: &str, volume: &str) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        ("kloudlite.io/owner".to_string(), owner.to_string()),
        (VOLUME_LABEL.to_string(), volume.to_string()),
    ])
}


/// Names the Environment a `stop-{env}` request belongs to, so the environments controller can
/// watch only those instead of every push in the cluster. Also a view: the ownerReference is the
/// link the mapper reads, and this label exists only because a watch cannot select on one.
pub const STOP_LABEL: &str = "kloudlite.io/stop-of";


/// Has the Volume already granted this exact wish?
///
/// Both halves of the pair, and the ONE place that decides it — the Volume's own guard and its
/// parent's gate must never disagree about whether a restore is finished, or one scales services
/// back up while the other still means to swap the disk under them.
pub fn wish_granted(wish: &RestoreWish, restored_to: Option<&str>, restored_at: Option<&str>) -> bool {
    restored_to == Some(wish.snapshot_id.as_str()) && restored_at == Some(wish.requested_at.as_str())
}


/// The btrfs generation the sync beat replicated, stamped by the owner AFTER it cuts (taking a
/// read-only snapshot bumps the source subvolume's own generation, so the pre-cut value leaves
/// every idle worktree permanently "due"). It lives here, beside `Snapshot`, because both the
/// agent that writes it and `/v1` — which must order the same transients to pick a clone's parent
/// — read it, and two copies of the ordering key is how two tiers disagree about which cut is
/// newest.
pub const SYNCED_GENERATION: &str = "kloudlite.io/synced-generation";


/// Public so the replica writer can apply the SAME key to the subset it actually holds on disk.
pub fn transient_generation_of(s: &Snapshot) -> u64 {
    use kube::ResourceExt;
    s.annotations().get(SYNCED_GENERATION).and_then(|g| g.parse::<u64>().ok()).unwrap_or(0)
}


/// The newest Ready transient of `worktree` among `snaps` — ordered by `SYNCED_GENERATION`, never
/// by creation time, because the annotation is the btrfs generation actually replicated and it is
/// the one ordering that survives clock skew between the owner that cut it and the node that
/// pulled it. A stop or clone cut carries no annotation until the owner stamps it post-cut and so
/// reads as 0: it loses to any annotated one and still beats nothing. Ties break by NAME so two
/// nodes computing this independently never disagree.
pub fn newest_transient_of(snaps: &[Snapshot], worktree: &str) -> Option<String> {
    use kube::ResourceExt;
    snaps
        .iter()
        .filter(|s| {
            s.spec.transient && s.spec.worktree == worktree && s.status.as_ref().is_some_and(|st| st.phase == Phase::Ready)
        })
        .map(|s| (transient_generation_of(s), s.name_any()))
        .max()
        .map(|(_, name)| name)
}
