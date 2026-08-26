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
fn every_crd_has_a_status_subresource_and_a_nodename_field_selector() {
    // Both are load-bearing and both fail SILENTLY if dropped: without `status: {}` a status
    // update is folded into spec (and Phase 2A's RBAC split becomes decorative); without
    // `selectableFields` every node's controller sees every node's work and two agents race
    // the same subvolume.
    for crd in all_crds() {
        let v = &crd.spec.versions[0];
        assert!(v.subresources.as_ref().is_some_and(|s| s.status.is_some()), "{}", crd.spec.names.kind);
        if crd.spec.names.kind == "OwnerBinding" {
            continue; // not watched per-node
        }
        let sel = v.selectable_fields.as_ref().expect("selectableFields");
        assert!(sel.iter().any(|f| f.json_path == ".spec.nodeName"), "{}", crd.spec.names.kind);
    }
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
