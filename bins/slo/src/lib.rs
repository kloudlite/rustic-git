//! `kloudlite-git-slo`: a synthetic user that walks the whole product every five minutes and
//! reports each step as one SLO sample.
//!
//! It is a client and nothing else — it holds no object-store credential, opens no database, and
//! reaches every tier the way a person does. That is the point: an SLO judged from inside the
//! system it measures is an SLO that passes while the front door is shut.

pub mod config;
pub mod crane;
pub mod ctx;
pub mod report;
pub mod stages;
pub mod step;
pub mod suite;
pub mod tools;

#[cfg(test)]
mod testkit;

pub use config::Config;
pub use ctx::Ctx;
pub use suite::{suite, Stage};
