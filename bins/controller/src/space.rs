//! The owned reconcilers. Task 5 fills this in; until then the process is a lease and a probe.

use std::sync::Arc;

pub async fn run(_ctx: Arc<crate::Ctx>) {
    std::future::pending::<()>().await
}
