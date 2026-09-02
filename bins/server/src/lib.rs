// `Result<T, axum::Response>` is the handler idiom here: the Err is an early-return response,
// unwrapped exactly once per request by `?`. Boxing it to please the size lint would add an
// allocation per refusal for no measurable gain.
#![allow(clippy::result_large_err)]

// `pub`, not `pub(crate)`: this crate also has a `[[bin]]` (`src/main.rs`), which — unlike a
// module of this same lib — is a SEPARATE crate that only sees `pub` items. Every module here
// (`boot`, `lanes`, `listeners`, `router`, `browse_api`) reaches these the same way either way,
// so widening the visibility costs nothing internally and is what lets `main()` call
// `rustic_git_server::{store, App, ...}` directly instead of duplicating these re-exports.
pub use rustic_git_core::pktline;
pub use rustic_git_core::{err, hex, require_jwt_secret_from_env, Error, Result};
pub use rustic_git_storage::{auth, cache, config, events, index, ownership, pool, store};
pub use rustic_git_gitbase::{objects, refs};
pub use rustic_git_pulls::{directory, merge_worker, pulls};
pub use rustic_git_app::{App, AddrOf};
pub use rustic_git_git::{browse, gc, protocol, proxy, ssh};
pub use rustic_git_registry as registry;

pub mod boot;
pub mod browse_api;
pub mod lanes;
pub mod listeners;
pub mod router;

/// `may_create` decides, before authentication, which routes may claim a name that does not exist
/// yet. Exported for the routing integration test — the exempt set is the security property, so it
/// is asserted from outside rather than only in the module that writes it.
pub mod router_test {
    pub use crate::router::route::may_create;
}
