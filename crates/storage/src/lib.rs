#![allow(clippy::result_large_err)]
pub(crate) use rustic_git_core::{err, hex, Error, Result};
pub mod auth;
pub mod cache;
pub mod config;
pub mod events;
pub mod index;
pub mod ownership;
pub mod pool;
pub mod refmeta;
pub mod store;
