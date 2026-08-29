//! Branch protection's gix-touching half, and the `update_refs` entry point.
//!
//! Everything else that used to live here (ref CRUD, repo metadata, `Protection`'s storage, the
//! ref-update transaction) moved to `crates/storage/src/refmeta.rs` along with `Store` itself —
//! Rust's orphan rule forbids an inherent `impl Store` outside the crate that defines `Store`.
//! What stays here is `protection_verdict`/`is_ancestor`, which walk history via `gix-traverse` —
//! a dependency `storage` must not carry — and `update_refs`, the seam between the two: it
//! computes verdicts (the gix-touching step) and hands them to `Store::update_refs_txn` (the
//! transactional compare-and-swap, gix-free, in `storage`). See task-3-report.md.

pub use rustic_git_storage::refmeta::{Protection, RefUpdate, RepoMeta};

use crate::store::{Repo, Store};
use crate::Result;
use gix_hash::ObjectId;

/// ponytail: fixed default branch; store per-repo when it becomes configurable
pub const DEFAULT_BRANCH: &str = "main";

/// `Some(reason)` if a rule refuses this update. The reason is shown to the
/// person pushing, so it says which rule and which branch.
fn protection_verdict(rules: &[Protection], odb: Option<&gix_odb::Handle>, u: &RefUpdate) -> Option<String> {
    let branch = branch_of(&u.name)?;
    let rule = rules.iter().find(|r| r.matches(branch))?;

    if u.new.is_none() {
        return rule
            .no_delete
            .then(|| format!("{branch} is protected: it cannot be deleted"));
    }
    // Creating a branch is not a rewrite; only a move from an existing tip can
    // be one.
    let (Some(old), Some(new)) = (u.old, u.new) else { return None };
    if !rule.no_force {
        return None;
    }
    // No odb means the check cannot be made, and a rule that cannot be checked
    // must refuse rather than wave the push through.
    let Some(odb) = odb else {
        return Some(format!("{branch} is protected: its history could not be verified"));
    };
    (!is_ancestor(odb, old, new, ANCESTRY_BUDGET))
        .then(|| format!("{branch} is protected: force pushes are not allowed"))
}

/// `refs/heads/x` -> `x`. Only branches are protectable; a tag is not a line of
/// development and `refs/tags/` is already immutable by convention.
fn branch_of(refname: &str) -> Option<&str> {
    refname.strip_prefix("refs/heads/")
}

/// Is `old` reachable from `new`? That is what makes a push a fast-forward.
///
/// Bounded: a walk that has not found the old tip within `budget` commits is
/// treated as NOT an ancestor, so an enormous rewrite is refused rather than
/// allowed by exhaustion. Refusing is the safe direction — the push fails loudly
/// and a person can turn the rule off, where the reverse silently loses history.
fn is_ancestor(odb: &gix_odb::Handle, old: ObjectId, new: ObjectId, budget: usize) -> bool {
    old == new
        || gix_traverse::commit::Simple::new(Some(new), odb.clone())
            .take(budget)
            .any(|info| info.is_ok_and(|i| i.id == old))
}

/// How far back a fast-forward check will look before giving up and refusing.
const ANCESTRY_BUDGET: usize = 50_000;

/// git's ref-name rules (git-check-ref-format), enough of them to keep hostile names out of
/// pkt-line output (control chars, newlines) and out of other repos via fork's ref copy.
pub fn valid_ref_name(name: &str) -> bool {
    if !name.starts_with("refs/")
        || name.len() > 512
        || name.ends_with('/')
        || name.ends_with(".lock")
        // `refs/heads` is a legal git name and an illegal one here: listings and protection
        // rules treat each namespace as a directory, and a ref AT the namespace shadows them.
        || name.splitn(3, '/').count() < 3
    {
        return false;
    }
    if name.contains("..") || name.contains("//") || name.contains("@{") || name.contains("\\") {
        return false;
    }
    if name
        .split('/')
        .any(|c| c.is_empty() || c.starts_with('.') || c.ends_with(".lock"))
    {
        return false;
    }
    name.bytes()
        .all(|b| b > 0x20 && b != 0x7f && !b"~^:?*[".contains(&b))
}

/// All-or-nothing compare-and-swap of refs in one serializable txn.
///
/// Enforced HERE rather than in the push path, so ssh and http and every future caller are
/// covered by one check — the same reasoning as the cache invalidation in
/// `Store::update_refs_txn`. Loaded once per batch; a repo with no rules pays one empty scan. The
/// verdicts are decided on a blocking thread: a no-force rule walks up to `ANCESTRY_BUDGET`
/// commits, which is not work for a runtime worker that every other request on this node shares.
pub async fn update_refs(store: &Store, repo: &Repo, updates: &[RefUpdate]) -> Result<Vec<Option<String>>> {
    let rules = store.protections(&repo.owner, &repo.name).await?;
    let mut verdicts: Vec<Option<String>> = if rules.is_empty() {
        vec![None; updates.len()]
    } else {
        let odb = repo.odb().ok();
        let ups: Vec<RefUpdate> = updates.to_vec();
        tokio::task::spawn_blocking(move || {
            ups.iter().map(|u| protection_verdict(&rules, odb.as_ref(), u)).collect()
        })
        .await?
    };
    // The name rule lives here for the same reason the protection rules do: receive-pack checks
    // early to fail before the pack, but the merge/patch routes format names from a request body,
    // and a ref written under `a\n<oid> refs/heads/main` corrupts every later advertisement.
    // Debug-formatted so the reason cannot carry the control bytes it is refusing.
    for (v, u) in verdicts.iter_mut().zip(updates) {
        if !valid_ref_name(&u.name) {
            *v = Some(format!("{:?} is not a valid ref name", u.name));
        }
    }
    store.update_refs_txn(repo, updates, verdicts).await
}

/// `store.update_refs(repo, updates)` method-call sugar over the free function above, so callers
/// (git push, the merge/rebase HTTP routes, every integration test) keep the call syntax they had
/// before `update_refs` split across two crates. An extension trait, not an inherent `impl Store`,
/// for the same orphan-rule reason as `gc::RepackExt`/`registry::store::ImageExt` — import it
/// wherever `.update_refs(...)` is called.
#[allow(async_fn_in_trait)]
pub trait UpdateRefsExt {
    async fn update_refs(&self, repo: &Repo, updates: &[RefUpdate]) -> Result<Vec<Option<String>>>;
}

impl UpdateRefsExt for Store {
    async fn update_refs(&self, repo: &Repo, updates: &[RefUpdate]) -> Result<Vec<Option<String>>> {
        update_refs(self, repo, updates).await
    }
}

#[cfg(test)]
mod ref_name_tests {
    use super::valid_ref_name;

    #[test]
    fn valid_ref_name_table() {
        for ok in ["refs/heads/main", "refs/tags/v1.0", "refs/heads/feature/x-y_z", "refs/notes/commits"] {
            assert!(valid_ref_name(ok), "{ok} should be accepted");
        }
        for bad in [
            "refs/heads",          // the namespace itself; a ref here shadows every branch
            "refs/tags",
            "refs",
            "refs/",
            "refs/heads/",
            "heads/main",          // not under refs/
            "refs/heads/.hidden",
            "refs/heads/a..b",
            "refs/heads/a.lock",
            "refs/heads/a b",
            "refs/heads/a~b",
            "refs/heads/a^b",
            "refs/heads/a:b",
            "refs/heads/a?b",
            "refs/heads/a*b",
            "refs/heads/a[b",
            "refs/heads/a\\b",
            "refs/heads/a@{b",
            "refs/heads//x",
            "refs/heads/a\x7fb",
            "refs/heads/a\nb",
        ] {
            assert!(!valid_ref_name(bad), "{bad:?} should be refused");
        }
        assert!(!valid_ref_name(&format!("refs/heads/{}", "a".repeat(600))), "too long");
    }
}
