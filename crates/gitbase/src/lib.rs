//! Git object plumbing: writing new objects into a repo's pack store, ref protection's
//! gix-touching half, and merge-base — everything that walks or writes a `gix_odb::Handle`.
//!
//! Split out of the old single lib so the gix dependency stack does not
//! have to be pulled in by callers that only need `rustic-git-storage`.

pub(crate) use rustic_git_core::{err, Result};
pub(crate) use rustic_git_storage::store;

pub mod objects;
pub mod refs;
mod merge_base;
pub use merge_base::merge_base;
