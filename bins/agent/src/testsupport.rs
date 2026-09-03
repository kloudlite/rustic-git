//! The one test `Ctx`. Six copies of these thirty lines had drifted apart — a route list here, a
//! different pool root there — which is how two tests of the same rule end up asserting against
//! two different worlds.

use crate::controller::Ctx;
use rustic_git_workspaces::engine::{Engine, Pool as EnginePool};
use rustic_git_workspaces::kube_test::{mock_client, Recorder, Route};
use std::sync::Arc;

pub(crate) struct NoopNix;

#[async_trait::async_trait]
impl crate::nix::Nix for NoopNix {
    async fn build(&self, _expr: &str, _timeout: std::time::Duration) -> Result<std::path::PathBuf, String> {
        Ok(std::path::PathBuf::from("/tmp"))
    }
    async fn ping(&self) -> Result<(), String> {
        Ok(())
    }
    async fn collect_garbage(&self) -> Result<u64, String> {
        Ok(0)
    }
}

pub(crate) fn test_ctx(pool: &std::path::Path, node: &str, routes: Vec<Route>) -> (Arc<Ctx>, Recorder) {
    let (client, rec) = mock_client(routes);
    let engine = Engine::new(EnginePool::new(pool));
    // Set, not read from the environment: a pod spec built without an image is a reconcile error,
    // and every test in this binary shares one process env — hence `--test-threads=1`.
    std::env::set_var("WS_DEFAULT_IMAGE", "ghcr.io/kloudlite/rustic-git-workspace:deadbeef");
    (
        Arc::new(Ctx::new(
            client,
            Arc::new(engine),
            node.into(),
            pool.to_string_lossy().into(),
            "r1".into(),
            vec![],
            Some("test:/".into()),
            Arc::new(NoopNix),
            pool.join("profiles"),
        )),
        rec,
    )
}
