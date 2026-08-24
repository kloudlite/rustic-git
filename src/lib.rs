// `Result<T, axum::Response>` is the handler idiom here: the Err is an early-return response,
// unwrapped exactly once per request by `?`. Boxing it to please the size lint would add an
// allocation per refusal for no measurable gain.
#![allow(clippy::result_large_err)]

pub mod auth;

pub use rustic_git_api as api;
pub use rustic_git_api::gpg;
pub use rustic_git_registry as registry;

pub use rustic_git_core::{err, hex, require_jwt_secret, require_jwt_secret_from_env, Error, Result};
pub use rustic_git_core::{jwt, pktline};
pub use rustic_git_storage::{cache, config, events, index, ownership, pool, refmeta, store};
pub use rustic_git_gitbase::{objects, refs};
pub use rustic_git_git::{browse, gc, protocol, proxy, ssh};
pub use rustic_git_pulls::{directory, merge_worker, pulls};
pub use rustic_git_app::{App, AddrOf, Patience, RECOVERY_ASK_EVERY};
