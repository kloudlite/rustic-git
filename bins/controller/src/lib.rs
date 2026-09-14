//! `kloudlite-controller`: the cluster's single elected writer of every object that is shared
//! across nodes or derived purely from spec. See
//! `docs/superpowers/specs/2026-09-14-cluster-controller-design.md`.

pub mod lease;
