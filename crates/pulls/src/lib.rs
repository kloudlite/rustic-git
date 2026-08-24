//! Pull requests, the directory (people and teams), and the merge worker.
//!
//! Split out of the root crate (see task-5-report.md): the pull-request model is what the
//! worker links, the `check` feature is the gix-touching mergeability walk the worker must
//! never pull in, and the directory + merge worker cohabit here because pull requests and
//! merge jobs are the reason both exist.

pub mod directory;
pub mod merge_worker;
pub mod pulls;
