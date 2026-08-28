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

#[test]
fn every_crd_has_a_status_subresource_and_the_right_node_selector() {
    // Both halves fail SILENTLY when dropped: without `status: {}` a status update folds into
    // spec (and the RBAC split becomes decorative); without `selectableFields` every node's
    // controller sees every node's work and two agents race the same subvolume.
    //
    // Which PATH is selectable is now the load-bearing part: placement is a fact the controllers
    // establish, so a parent's node lives in status, while a controller-written child's stays in
    // spec. `SnapshotRequest` has NO selector at all — it names no node, and every agent watches
    // every request, acting only when the named Volume is its own.
    for crd in all_crds() {
        let v = &crd.spec.versions[0];
        assert!(v.subresources.as_ref().is_some_and(|s| s.status.is_some()), "{}", crd.spec.names.kind);
        let want = match crd.spec.names.kind.as_str() {
            "OwnerBinding" | "Volume" => Some(".spec.nodeName"),
            "Workspace" | "Environment" => Some(".status.nodeName"),
            "SnapshotRequest" => None,
            other => panic!("unknown kind {other}"),
        };
        match want {
            None => assert!(
                v.selectable_fields.is_none(),
                "SnapshotRequest must have no selectableFields: it copies no node into spec"
            ),
            Some(path) => {
                let sel = v.selectable_fields.as_ref().expect("selectableFields");
                assert!(sel.iter().any(|f| f.json_path == path), "{} must select on {path}", crd.spec.names.kind);
                // Arrays cannot be selectable fields; `compatibleNodes` must never sneak in as one.
                assert!(!sel.iter().any(|f| f.json_path.contains("compatibleNodes")), "{}", crd.spec.names.kind);
            }
        }
    }
}

/// The five kinds, so a kind added to the group without a CRD entry cannot ship: `all_crds` is what
/// generates the manifest AND what the agent's startup precondition check reads.
#[test]
fn all_five_kinds_are_generated() {
    let kinds: Vec<String> = all_crds().into_iter().map(|c| c.spec.names.kind).collect();
    for k in ["Volume", "Workspace", "Environment", "OwnerBinding", "SnapshotRequest"] {
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
        if crd.spec.names.kind == "OwnerBinding" {
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

/// Release 1 is ADDITIVE. `storage` arrives; the two legacy spec fields stay, optional, because a
/// cluster-wide prune before a per-node agent roll destroys the pointer an unmigrated object needs.
/// Task 11 is what removes them.
#[test]
fn release_one_adds_storage_and_keeps_the_legacy_spec_fields() {
    use kube::CustomResourceExt;
    use rustic_git_workspaces::crd::{Environment, Workspace};
    for crd in [Workspace::crd(), Environment::crd()] {
        let v = &crd.spec.versions[0];
        let root = v.schema.as_ref().unwrap().open_api_v3_schema.as_ref().unwrap();
        let spec = &root.properties.as_ref().unwrap()["spec"];
        let props = spec.properties.as_ref().unwrap();
        assert!(props.contains_key("storage"), "{} spec needs storage", crd.spec.names.kind);
        assert!(props.contains_key("nodeName"), "{}: do not prune nodeName in release 1", crd.spec.names.kind);
        assert!(props.contains_key("volumeRef"), "{}: do not prune volumeRef in release 1", crd.spec.names.kind);
        // Optional, though: the API stops writing them this release, so a required field would
        // reject every new object.
        let required = spec.required.clone().unwrap_or_default();
        assert!(!required.contains(&"nodeName".to_string()), "{}", crd.spec.names.kind);
        assert!(!required.contains(&"volumeRef".to_string()), "{}", crd.spec.names.kind);
        // `storage` is optional too, and for the mirror-image reason: an object created BEFORE the
        // field existed must still deserialize, or every legacy parent 422s on its next write.
        assert!(!required.contains(&"storage".to_string()), "{}: storage must be optional in release 1", crd.spec.names.kind);
        // The credential Secret path is deleted outright, not deprecated: nobody ever wrote that
        // Secret and no object carries it, so there is nothing to lose by pruning it.
        let schema = serde_json::to_string(&v.schema).unwrap();
        // camelCase: that is what serde emits, so the snake_case spelling never appears and
        // asserting on it proves nothing.
        assert!(!schema.contains("credentialSecret"), "{} still names credentialSecret", crd.spec.names.kind);
    }
}

/// `lastPush` is gone and NOTHING replaces it on the Volume. "The latest snapshot" is a query over
/// SnapshotRequests by volume label — two controllers force-applying one status object under one
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
    assert_eq!(ws_namespace("alice", "acme"), "ws-acme-alice");
    assert_eq!(ws_namespace("Alice", "ACME"), "ws-acme-alice");
    assert_ne!(ws_namespace("alice", "acme"), ws_namespace("alice", "globex"));
    // Two 39-character handles overflow the 63-character label limit; the result must still be
    // a valid label, and two different pairs that share a long prefix must not collide.
    let long = "a".repeat(39);
    let a = ws_namespace(&long, &format!("{}x", "b".repeat(38)));
    let b = ws_namespace(&long, &format!("{}y", "b".repeat(38)));
    assert!(a.len() <= 63 && b.len() <= 63, "{a} {b}");
    assert_ne!(a, b);
    assert!(!a.contains("--") && !a.ends_with('-'), "{a}");
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
///
/// Additive and optional, like everything else in release 1: an Environment written before this
/// existed must still round-trip, and a controller that force-applies a spec without `restore`
/// must not have the API server reject it.
#[test]
fn the_in_place_restore_wish_is_optional_on_the_parent_and_the_child() {
    use kube::CustomResourceExt;
    use rustic_git_workspaces::crd::{Environment, Volume, Workspace};
    for crd in [Environment::crd(), Workspace::crd()] {
        let v = &crd.spec.versions[0];
        let root = v.schema.as_ref().unwrap().open_api_v3_schema.as_ref().unwrap();
        let spec = &root.properties.as_ref().unwrap()["spec"];
        assert!(spec.properties.as_ref().unwrap().contains_key("restore"), "{}", crd.spec.names.kind);
        assert!(!spec.required.clone().unwrap_or_default().contains(&"restore".to_string()), "{}", crd.spec.names.kind);
    }
    let v = &Volume::crd().spec.versions[0];
    let root = v.schema.as_ref().unwrap().open_api_v3_schema.as_ref().unwrap();
    let props = root.properties.as_ref().unwrap();
    // The child's copy of the wish is spec (controller-written), and what it produced is status —
    // the pair the whole "already done" test reads.
    assert!(props["spec"].properties.as_ref().unwrap().contains_key("restoreTo"));
    assert!(props["status"].properties.as_ref().unwrap().contains_key("restoredTo"));
}
