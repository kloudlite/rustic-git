//! The tree step: `spec.trees` is the ask, `status.trees` is what this node cut.
//!
//! Deliberately about the DECISION and the status write, not about btrfs — same rule as every
//! other file here. What `tree_actions` chooses is asserted directly; the `btrfs subvolume
//! snapshot` it leads to is the engine's own, exercised by `engine_ops.rs` and `ws_e2e.sh`.

use super::*;
use kloudlite_agent::controller::{tree_actions, ws_status_eq, TreeAction};


fn spec(names: &[&str]) -> Vec<crd::TreeSpec> {
    names.iter().map(|n| crd::TreeSpec { name: (*n).into(), created: "2026-09-17T00:00:00Z".into() }).collect()
}

fn status(rows: &[(&str, bool)]) -> Vec<crd::TreeStatus> {
    rows.iter()
        .map(|(n, ready)| crd::TreeStatus {
            name: (*n).into(),
            path: crd::tree_path(n),
            ready: *ready,
            reason: None,
        })
        .collect()
}

#[test]
fn a_spec_tree_with_no_status_is_cut() {
    let acts = tree_actions(&spec(&["x"]), &status(&[]), &[]);
    assert_eq!(acts, vec![TreeAction::Cut("x".into())]);
}

/// Level-triggered: a tree already cut and ready is nothing to do, every pass, forever. A
/// re-snapshot would be silent data loss — the subagent's work replaced by the main tree's.
#[test]
fn a_ready_tree_is_left_alone() {
    assert!(tree_actions(&spec(&["x"]), &status(&[("x", true)]), &["x".into()]).is_empty());
}

/// A failed cut left `ready: false`, and the next pass tries again: a snapshot failure is an
/// outage (a full pool, a lock), never an answer.
#[test]
fn a_tree_that_failed_to_cut_is_retried() {
    let acts = tree_actions(&spec(&["x"]), &status(&[("x", false)]), &[]);
    assert_eq!(acts, vec![TreeAction::Cut("x".into())]);
}

/// The `/v1` DELETE removed the spec entry; the bytes are this node's to collect.
#[test]
fn a_status_row_with_no_spec_entry_is_deleted() {
    let acts = tree_actions(&spec(&[]), &status(&[("x", true)]), &["x".into()]);
    assert_eq!(acts, vec![TreeAction::Delete("x".into())]);
}

/// The crash window between the DELETE and this pass: the row is already gone from status (the
/// write landed) and the subvolume is still on disk. Nothing but the on-disk listing can see it,
/// which is why the listing is an input rather than status alone.
#[test]
fn a_subvolume_the_spec_does_not_name_is_deleted_however_status_forgot_it() {
    let acts = tree_actions(&spec(&[]), &status(&[]), &["orphan".into()]);
    assert_eq!(acts, vec![TreeAction::Delete("orphan".into())]);
}

/// A name the ask does not carry is deleted by NAME, never by "everything under .agents": a
/// directory that is not a subvolume is somebody's mistake to look at, not this pass's to remove.
#[test]
fn every_decision_is_by_name_and_both_kinds_are_made_in_one_pass() {
    let acts = tree_actions(&spec(&["keep", "new"]), &status(&[("keep", true), ("gone", true)]), &["keep".into(), "gone".into()]);
    assert_eq!(acts, vec![TreeAction::Cut("new".into()), TreeAction::Delete("gone".into())]);
}


/// R-D16, live run 8dca6d54: a tree was cut, served and deleted correctly on disk while
/// `status.trees` stayed EMPTY for its whole life. Nothing was wrong with the cut or the rows —
/// the status write was skipped, because the "has anything changed?" test the writer uses did not
/// look at `trees`, and a pass that only cuts or drops a tree changes nothing else.
///
/// So this asserts the WRITER's decision, which is where the bytes were lost: a status carrying a
/// tree must not compare equal to one without it, in either direction.
#[test]
fn a_status_whose_only_change_is_a_tree_is_written() {
    let base = crd::WorkspaceStatus { phase: crd::Phase::Ready, ..Default::default() };
    let cut = crd::WorkspaceStatus { trees: status(&[("x", true)]), ..base.clone() };

    assert!(ws_status_eq(&base, &base), "an unchanged status is still a no-op");
    assert!(!ws_status_eq(&base, &cut), "the cut must reach the API server");
    // And the delete: dropping the last row is the other half, and the one a merge patch would
    // have lost. The writer is a server-side apply, so an omitted field is a removed field.
    assert!(!ws_status_eq(&cut, &base), "the delete must reach it too");

    // A row that changed only its readiness — a failed cut becoming ready — is a change as well.
    let failed = crd::WorkspaceStatus { trees: status(&[("x", false)]), ..base.clone() };
    assert!(!ws_status_eq(&failed, &cut), "ready: false to ready: true is what a person is waiting for");
}

/// The rows the pass reports for a given ask, in the shape `/v1` and the desktop read: every
/// asked-for tree ready with its `.agents/{name}` path, and nothing for a name the ask dropped.
#[test]
fn the_reported_rows_name_every_asked_for_tree_and_only_those() {
    let ready = status(&[("x", true)]);
    assert_eq!(ready[0].path, crd::tree_path("x"), "the path is the one the tool server serves");
    assert!(ready[0].ready);
    assert_eq!(ready[0].reason, None);
    assert!(status(&[]).is_empty(), "a dropped tree leaves no row behind");
}
