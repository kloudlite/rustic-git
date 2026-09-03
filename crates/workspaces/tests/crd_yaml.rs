//! `deploy/k3s/crds.yaml` is a GENERATED artifact — Phase 2A installs exactly what the Rust
//! types say. This test is the generator (`CRD_REGEN=1 cargo test -p rustic-git-workspaces
//! --test crd_yaml`) and the drift check in one, so a field added to a spec struct cannot ship
//! without the manifest moving with it.
use rustic_git_workspaces::crd::all_crds;

#[test]
fn generated_crds_match_the_committed_manifest() {
    // A `v1 List` of JSON, written to a `.yaml` path on purpose: YAML is a superset of JSON, so
    // `kubectl apply -f` accepts it verbatim, and this keeps the archived `serde_yaml`
    // (RUSTSEC-2024-0320) out of the tree. ponytail: unreadable diffs; swap to `serde-saphyr`
    // if a human ever has to review this file by eye.
    let doc = serde_json::json!({"apiVersion": "v1", "kind": "List", "items": all_crds()});
    let want = format!("{}\n", serde_json::to_string_pretty(&doc).unwrap());
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/k3s/crds.yaml");
    if std::env::var("CRD_REGEN").is_ok() {
        std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
        std::fs::write(path, &want).unwrap();
    }
    let got = std::fs::read_to_string(path).unwrap_or_default();
    assert_eq!(got, want, "run CRD_REGEN=1 cargo test --test crd_yaml to regenerate");
}

/// The status field placement stopped reading is gone from the SCHEMA too, not merely unwritten:
/// a schema that still advertises it invites the next reader to trust it. Old stored objects keep
/// parsing because the Rust struct tolerates the field on read (`#[serde(default)]`) and the CRD
/// prunes what it does not declare — which is exactly the wanted behaviour: the value disappears
/// on the first write of an old object, and nothing ever reads it again.
#[test]
fn compatible_nodes_is_gone_from_the_published_schema() {
    let yaml = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/k3s/crds.yaml")).unwrap();
    assert!(!yaml.contains("compatibleNodes"), "regenerate deploy/k3s/crds.yaml");
}

#[test]
fn every_crd_has_a_status_subresource_and_the_right_node_selector() {
    // Both halves fail SILENTLY when dropped: without `status: {}` a status update folds into
    // spec (and the RBAC split becomes decorative); without `selectableFields` every node's
    // controller sees every node's work and two agents race the same subvolume.
    //
    // Which PATH is selectable is now the load-bearing part: placement is a fact the controllers
    // establish, so a parent's node lives in status, while a controller-written child's stays in
    // spec.
    for crd in all_crds() {
        let v = &crd.spec.versions[0];
        assert!(v.subresources.as_ref().is_some_and(|s| s.status.is_some()), "{}", crd.spec.names.kind);
        let want: &[&str] = match crd.spec.names.kind.as_str() {
            "OwnerBinding" => &[],
            "Volume" => &[".spec.nodeName"],
            "Workspace" | "Environment" => &[".status.nodeName"],
            "Snapshot" => &[".spec.volume"],
            // `.spec.volume`: `replicated_condition` and `pull_volume` both filtered client-side
            // because a selector on it was a 400. Dropping it makes both a full-cluster scan again.
            "VolumeReplica" => &[".spec.node", ".status.phase", ".spec.volume"],
            other => panic!("unknown kind {other}"),
        };
        if want.is_empty() {
            assert!(v.selectable_fields.is_none(), "{} must have no selectableFields", crd.spec.names.kind);
        } else {
            let sel = v.selectable_fields.as_ref().expect("selectableFields");
            for path in want {
                assert!(sel.iter().any(|f| &f.json_path == path), "{} must select on {path}", crd.spec.names.kind);
            }
            // Arrays cannot be selectable fields; `compatibleNodes` must never sneak in as one.
            assert!(!sel.iter().any(|f| f.json_path.contains("compatibleNodes")), "{}", crd.spec.names.kind);
        }
    }
}

/// `SnapshotSpec::state` can't be validated by the schema (kube-core cannot flatten an
/// internally-tagged enum's `oneOf` into one object — see the comment on the field), so it must
/// be published as `x-kubernetes-preserve-unknown-fields: true`, not left with no `type` at all:
/// a structural-schema apply rejects a specified object property with no `type`, and if it were
/// somehow accepted, pruning would strip every child and silently store `state: {}`.
#[test]
fn snapshot_state_preserves_unknown_fields() {
    let crd = all_crds().into_iter().find(|c| c.spec.names.kind == "Snapshot").unwrap();
    let schema = crd.spec.versions[0].schema.as_ref().unwrap().open_api_v3_schema.as_ref().unwrap();
    let props = schema.properties.as_ref().unwrap()["spec"].properties.as_ref().unwrap();
    let state = &props["state"];
    assert_eq!(state.type_.as_deref(), Some("object"), "state must have type: object");
    let ext = &state.x_kubernetes_preserve_unknown_fields;
    assert_eq!(*ext, Some(true), "state must set x-kubernetes-preserve-unknown-fields: true");
}

/// The six kinds, so a kind added to the group without a CRD entry cannot ship: `all_crds` is
/// what generates the manifest AND what the agent's startup precondition check reads.
/// `SnapshotRequest` — the object-store push request — is gone (Task 8).
#[test]
fn all_six_kinds_are_generated() {
    let kinds: Vec<String> = all_crds().into_iter().map(|c| c.spec.names.kind).collect();
    for k in ["Volume", "Workspace", "Environment", "OwnerBinding", "Snapshot", "VolumeReplica"] {
        assert!(kinds.iter().any(|g| g == k), "{k} missing from all_crds(): {kinds:?}");
    }
}

/// `phase` must be a schema `enum`, not a free-form string.
///
/// A typo in a phase does not fail today: `api::phase` falls back to a default on an unknown
/// string, so the controller wrote `running`, `WsState` spells that state `Ready`, and a healthy
/// workspace showed "Creating" in the UI forever. Nothing failed and nothing logged. An `enum` in
/// the schema turns that class of bug into a 422 at the API server.
#[test]
fn every_phase_is_a_schema_enum() {
    for crd in all_crds() {
        // OwnerBinding has no phase and needs none: `NamespaceReady` is its whole state.
        // VolumeReplica's phase is deliberately a plain string, not `Phase` — it must be a
        // `selectableField`, and the API server only accepts a string type there.
        if crd.spec.names.kind == "OwnerBinding" || crd.spec.names.kind == "VolumeReplica" {
            continue;
        }
        let status = crd.spec.versions[0]
            .schema
            .as_ref()
            .unwrap()
            .open_api_v3_schema
            .as_ref()
            .unwrap()
            .properties
            .as_ref()
            .unwrap()["status"]
            .clone();
        let phase = &status.properties.as_ref().unwrap()["phase"];
        assert!(
            phase.enum_.as_ref().is_some_and(|e| !e.is_empty()),
            "{}'s status.phase is a free-form string, not an enum",
            crd.spec.names.kind
        );
    }
}

/// The legacy spec pointers are GONE, and `storage` is what every parent builds its disk from.
///
/// They stayed one release only because a cluster-wide prune before a per-node agent roll would
/// have destroyed the pointer an unmigrated object needed; nothing carries them any more, so the
/// schema must not offer them a place to come back to.
#[test]
fn the_legacy_spec_pointers_are_pruned_and_storage_is_optional() {
    use kube::CustomResourceExt;
    use rustic_git_workspaces::crd::{Environment, Workspace};
    for crd in [Workspace::crd(), Environment::crd()] {
        let v = &crd.spec.versions[0];
        let root = v.schema.as_ref().unwrap().open_api_v3_schema.as_ref().unwrap();
        let spec = &root.properties.as_ref().unwrap()["spec"];
        let props = spec.properties.as_ref().unwrap();
        assert!(props.contains_key("storage"), "{} spec needs storage", crd.spec.names.kind);
        assert!(!props.contains_key("nodeName"), "{}: placement is status only", crd.spec.names.kind);
        assert!(!props.contains_key("volumeRef"), "{}: the child pointer is status only", crd.spec.names.kind);
        // `storage` is optional, and for the mirror-image reason: an object created BEFORE the
        // field existed must still deserialize, or every legacy parent 422s on its next write.
        let required = spec.required.clone().unwrap_or_default();
        assert!(!required.contains(&"storage".to_string()), "{}: storage must stay optional", crd.spec.names.kind);
        // The credential Secret path is deleted outright, not deprecated: nobody ever wrote that
        // Secret and no object carries it, so there is nothing to lose by pruning it.
        let schema = serde_json::to_string(&v.schema).unwrap();
        // camelCase: that is what serde emits, so the snake_case spelling never appears and
        // asserting on it proves nothing.
        assert!(!schema.contains("credentialSecret"), "{} still names credentialSecret", crd.spec.names.kind);
    }
}

/// `lastPush` is gone and NOTHING replaces it on the Volume. "The latest snapshot" is a query over
/// `Snapshot` CRs by volume label — two controllers force-applying one status object under one
/// field manager prune each other's fields, which is what a second writer here would be.
#[test]
fn the_volume_status_has_no_push_pointer() {
    use kube::CustomResourceExt;
    let schema = serde_json::to_string(&rustic_git_workspaces::crd::Volume::crd().spec.versions[0].schema).unwrap();
    assert!(!schema.contains("lastPush"), "lastPush must be dropped");
    assert!(!schema.contains("lastSnapshot"), "and not replaced by a second writer's field");
}

/// Environment ids are minted as `env-{hex}`, so a namespace helper that prefixes unconditionally
/// yields `env-env-{hex}`. Valid Kubernetes, wrong every time a human reads it.
#[test]
fn env_namespace_does_not_double_its_prefix() {
    use rustic_git_workspaces::crd::env_namespace;
    assert_eq!(env_namespace("env-abc123"), "env-abc123");
    // An id without the prefix still gets one — the namespace should say what it holds.
    assert_eq!(env_namespace("abc123"), "env-abc123");
    // Namespaces are RFC-1123: lowercase only.
    assert_eq!(env_namespace("ENV-ABC"), "env-abc");
}

/// One namespace per (team, owner) pair. The same person in two teams must land in two
/// namespaces — that is the isolation boundary, since NetworkPolicies and the git-key Secret are
/// per namespace — and their personal namespace stays what it always was.
#[test]
fn workspace_namespace_is_per_team_per_owner() {
    use rustic_git_workspaces::crd::ws_namespace;
    assert_eq!(ws_namespace("alice", ""), "ws-alice");
    // A team equal to the owner is personal, not "alice-alice".
    assert_eq!(ws_namespace("alice", "alice"), "ws-alice");
    assert!(ws_namespace("alice", "acme").starts_with("wt-alice-"), "{}", ws_namespace("alice", "acme"));
    assert_eq!(ws_namespace("Alice", "ACME"), ws_namespace("alice", "acme"));
    assert_ne!(ws_namespace("alice", "acme"), ws_namespace("alice", "globex"));
}

/// The collision that shared one person's private git key with another: handles and team slugs
/// both allow `-`, so any join of the two by `-` has two readings. Every distinct `(team, owner)`
/// pair — personal ones included — must land in its own namespace, and every namespace must be a
/// label the API server accepts.
#[test]
fn no_two_owner_team_pairs_share_a_namespace() {
    use rustic_git_workspaces::crd::{binding_name, ws_namespace};
    use std::collections::HashMap;
    let handles = ["a", "b", "c", "a-b", "b-c", "acme", "bob", "acme-bob", "x", "att", "x-att", &"a".repeat(39), &"b".repeat(39)];
    let teams = handles.iter().copied().chain([""]);
    let mut seen: HashMap<String, (String, String)> = HashMap::new();
    for owner in handles {
        for team in teams.clone() {
            // A team equal to the owner IS the personal pair — same namespace by definition.
            if team == owner {
                continue;
            }
            let ns = ws_namespace(owner, team);
            let label = ns.len() <= 63
                && ns.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !ns.starts_with('-')
                && !ns.ends_with('-');
            assert!(label, "{ns:?} is not an RFC 1123 label");
            if let Some(prev) = seen.insert(ns.clone(), (team.into(), owner.into())) {
                panic!("{ns} is both {prev:?} and {:?}", (team, owner));
            }
        }
    }
    // The audit's named cases, spelled out.
    assert_ne!(ws_namespace("bob", "acme"), ws_namespace("acme-bob", ""));
    assert_ne!(ws_namespace("c", "b"), ws_namespace("b-c", ""));
    assert_ne!(ws_namespace("a-b", "c"), ws_namespace("a", "b-c"));
    assert_ne!(binding_name("centralindia-x", "att"), binding_name("centralindia", "x-att"));
    assert!(binding_name("centralindia", &"a".repeat(39)).len() <= 63);
}

/// `Phase::as_str` and the serde wire form must be the same word. Two spellings of one state is the
/// exact bug the enum exists to kill — a projection matching on `as_str` while the API server holds
/// the serde spelling would silently never match.
#[test]
fn phase_as_str_matches_the_wire_form() {
    use rustic_git_workspaces::crd::Phase::*;
    for p in [Pending, Creating, Ready, Running, Stopped, Working, Done, Error] {
        assert_eq!(serde_json::to_value(p).unwrap(), serde_json::json!(p.as_str()), "{p:?}");
    }
}

/// The in-place restore is expressible in the SCHEMA, on both halves of the parent/child pair.
/// The Environment is the only parent that takes one — the Workspace's twin was never read.
///
/// Additive and optional, like everything else in release 1: an Environment written before this
/// existed must still round-trip, and a controller that force-applies a spec without `restore`
/// must not have the API server reject it.
#[test]
fn the_in_place_restore_wish_is_optional_on_the_parent_and_the_child() {
    use kube::CustomResourceExt;
    use rustic_git_workspaces::crd::{Environment, Volume};
    let crd = Environment::crd();
    let v = &crd.spec.versions[0];
    let root = v.schema.as_ref().unwrap().open_api_v3_schema.as_ref().unwrap();
    let spec = &root.properties.as_ref().unwrap()["spec"];
    assert!(spec.properties.as_ref().unwrap().contains_key("restore"), "{}", crd.spec.names.kind);
    assert!(!spec.required.clone().unwrap_or_default().contains(&"restore".to_string()), "{}", crd.spec.names.kind);
    let v = &Volume::crd().spec.versions[0];
    let root = v.schema.as_ref().unwrap().open_api_v3_schema.as_ref().unwrap();
    let props = root.properties.as_ref().unwrap();
    // The child's copy of the wish is spec (controller-written), and what it produced is status —
    // the pair the whole "already done" test reads.
    assert!(props["spec"].properties.as_ref().unwrap().contains_key("restoreTo"));
    assert!(props["status"].properties.as_ref().unwrap().contains_key("restoredTo"));
}

/// A `VolumeSpec` with no restore wish must not SERIALIZE one. `ensure` server-side-applies the
/// child Volume on every pass under one field manager: a `restoreTo: null` in that body would
/// claim the field and prune the wish the parent's gate just wrote.
#[test]
fn a_volume_spec_without_a_wish_carries_no_restore_to() {
    let spec = rustic_git_workspaces::crd::VolumeSpec {
        owner: "alice".into(),
        team: String::new(),
        node_name: "node-a".into(),
        region: "r1".into(),
        quota_gb: 10,
        replicas: rustic_git_workspaces::crd::DEFAULT_REPLICAS,
        source: None,
        restore_to: None,
    };
    let json = serde_json::to_value(&spec).unwrap();
    assert!(json.get("restoreTo").is_none(), "{json}");
}

/// The field is optional in the schema: a Workspace written before attachment existed must still
/// validate, and `/v1` creates workspaces without it.
#[test]
fn the_attached_environment_is_an_optional_string() {
    use kube::CustomResourceExt;
    use rustic_git_workspaces::crd;
    let crd = crd::Workspace::crd();
    let schema = crd.spec.versions[0].schema.as_ref().unwrap().open_api_v3_schema.as_ref().unwrap();
    let props = schema.properties.as_ref().unwrap()["spec"].properties.as_ref().unwrap();
    let field = props.get("attachedEnvironment").expect("attachedEnvironment in the schema");
    assert_eq!(field.type_.as_deref(), Some("string"));
    let required = schema.properties.as_ref().unwrap()["spec"].required.clone().unwrap_or_default();
    assert!(!required.contains(&"attachedEnvironment".to_string()), "must not be required");
}

/// The bug this exists for: a naming rule lives in Rust and the fence that depends on it lives in
/// CEL, so only a cluster apply connects them — team workspaces (`wt-`) were denied at namespace
/// create, at the host-key Secret and at both RoleBindings for as long as the policy said `ws-`
/// and `env-` only.
#[test]
fn every_namespace_the_code_makes_is_admitted() {
    use rustic_git_workspaces::crd::{env_namespace, ws_namespace};
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/k3s/agent-admission.yaml");
    let policy = std::fs::read_to_string(path).unwrap();
    // Only the Namespace arm of policy 2's expression: the Secret and RoleBinding arms test
    // `metadata.namespace` and are asserted separately below.
    let start = policy.find("object.kind == 'Namespace'").expect("policy 2 has a Namespace branch");
    let end = policy[start..].find("object.kind == 'Secret'").expect("…followed by the Secret branch") + start;
    let prefixes: Vec<String> = policy[start..end]
        .match_indices("startsWith('")
        .map(|(i, m)| {
            let rest = &policy[start + i + m.len()..];
            rest[..rest.find('\'').expect("a closed CEL string literal")].to_string()
        })
        .collect();
    assert!(!prefixes.is_empty(), "no startsWith literals found — did the expression change shape?");

    let mut made = vec![env_namespace("env-abc123")];
    for (owner, team) in [("alice", ""), ("alice", "alice"), ("alice", "acme"), ("a-b", "b-c")] {
        made.push(ws_namespace(owner, team));
    }
    for ns in &made {
        assert!(
            prefixes.iter().any(|p| ns.starts_with(p.as_str())),
            "namespace {ns} is denied by agent-admission.yaml (admits {prefixes:?})"
        );
    }
    // The Secret and RoleBinding arms gate on `metadata.namespace`, so every workspace prefix must
    // appear in each of them too — a team workspace's `ws-ssh-{id}` Secret is denied otherwise.
    let secret_arm = &policy[end..policy.find("object.kind == 'RoleBinding'").expect("a RoleBinding branch")];
    for p in ["ws-", "wt-"] {
        assert!(secret_arm.contains(&format!("startsWith('{p}')")), "Secret branch must admit {p} namespaces");
    }
}
