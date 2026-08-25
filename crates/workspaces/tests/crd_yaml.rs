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
