//! Pull requests, in the repo's own database.
//!
//! No HTTP and no Mongo here: this is the key encoding and the numbering sequence over a
//! SlateDB handle, so it is testable without a fleet.
//!
//! Timestamps are milliseconds since epoch, not `bson::DateTime`: a bson type survives a
//! non-bson serializer only by accident of its `Serialize` impl, and repo-local truth should
//! not carry a MongoDB-shaped value once Mongo is gone. The serde names still say `createdAt`
//! and friends, because those are the wire names the web app already reads.

pub mod model;
pub use model::*;
mod jobs;
pub use jobs::*;
#[cfg(feature = "check")]
mod check;
#[cfg(feature = "check")]
pub use check::*;
pub use crate::directory::{MergeState, MergeableState};
