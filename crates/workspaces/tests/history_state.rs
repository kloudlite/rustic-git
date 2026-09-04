//! `history` is optional state: a process without ClickStack must still build, route and answer —
//! the console renders a flat placeholder, never an error page.

use kloudlite_git_core::jwt::Jwt;
use kloudlite_git_workspaces::api::ApiState;
use kloudlite_git_workspaces::history::History;
use std::sync::Arc;

fn jwt() -> Arc<Jwt> {
    Arc::new(Jwt::new("test-secret-at-least-32-bytes-long!!").unwrap())
}

#[test]
fn history_is_absent_by_default() {
    assert!(ApiState::new(jwt()).history.is_none());
}

#[test]
fn with_history_attaches_it() {
    let h = Arc::new(History::new("http://127.0.0.1:8123", "default", ""));
    assert!(ApiState::new(jwt()).with_history(h).history.is_some());
}
