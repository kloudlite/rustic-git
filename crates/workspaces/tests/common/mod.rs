//! The handful of helpers every `api_*.rs` integration test had its own copy of (2026-09-12).
//!
//! Integration tests are separate binaries, so this is a module each of them declares with
//! `mod common;` rather than a crate — the crate-level test surface the api tests share
//! (`Route`, `Recorder`, `mock_client`) already lives in `kloudlite_workspaces::kube_test`, and
//! these four are the pieces that could not go there: they mint against `kloudlite_core::jwt`
//! and speak HTTP to a test server, neither of which the kube mock knows about.
#![allow(dead_code)] // each test binary uses its own subset.

use kloudlite_core::jwt::Jwt;
use serde_json::Value;

/// An ordinary session token for `{username}@example.com`, handle `username`.
pub fn token(jwt: &Jwt, username: &str) -> String {
    jwt.mint(&format!("{username}@example.com"), "Test User", Some(username)).unwrap()
}

/// A session carrying the `superadmin` claim — what `refuse_without_claim` looks for.
pub fn admin_token(jwt: &Jwt) -> String {
    admin_token_as(jwt, "root")
}

/// The same, under a named handle: the audit rows record who wrote, so a test about attribution
/// cannot use everybody's `root`.
pub fn admin_token_as(jwt: &Jwt, username: &str) -> String {
    jwt.mint_admin(&format!("{username}@example.com"), "Root", Some(username), true).unwrap()
}

/// GET `base + path` as `tok`. The body is `Value::Null` when there is not one, so a test that
/// only cares about the status still gets a tuple it can ignore half of.
pub async fn get_json(base: &str, tok: &str, path: &str) -> (reqwest::StatusCode, Value) {
    let resp = reqwest::Client::new().get(format!("{base}{path}")).bearer_auth(tok).send().await.unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

pub async fn delete(base: &str, tok: &str, path: &str) -> reqwest::StatusCode {
    reqwest::Client::new().delete(format!("{base}{path}")).bearer_auth(tok).send().await.unwrap().status()
}
