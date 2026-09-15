//! The one assertion that survives a refactor: this binary contains no space-policy writer.
//!
//! Two writers of one object across a roll is the failure stage 1 exists to avoid, and a unit test
//! of one converge path only proves the path it calls. This reads the source of the two modules
//! that used to write the halves, so a re-introduction anywhere in them fails here — including in
//! a branch that never runs in the reconcile suite.
//!
//! The grants are `kloudlite-controller`'s (`bins/controller/src/space.rs`); the agent keeps the
//! `resolv.conf` render and the `Attached` condition, which are host-bound and stay host-bound.

const SPACE: &str = include_str!("../../src/controller/space.rs");
const ENVIRONMENT: &str = include_str!("../../src/controller/environment/mod.rs");

#[test]
fn the_agent_source_names_no_space_policy_builder() {
    for (file, src) in [("controller/space.rs", SPACE), ("controller/environment/mod.rs", ENVIRONMENT)] {
        for needle in ["space_egress", "space_ingress", "SPACE_EGRESS_POLICY", "space_ingress_name"] {
            assert!(
                !src.contains(needle),
                "{file} names `{needle}`: the space grants belong to kloudlite-controller — \
                 see bins/controller/src/space.rs and the spec at \
                 docs/superpowers/specs/2026-09-14-cluster-controller-design.md"
            );
        }
    }
}

/// And the module that used to hold them still does its own job, so this is not passing because
/// somebody deleted the file.
#[test]
fn the_agent_still_renders_the_resolv_conf() {
    assert!(SPACE.contains("write_resolv"), "the agent still owns the per-pod resolv.conf");
}
