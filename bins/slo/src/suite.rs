//! The ordered stage list per suite.
//!
//! Teardown is NOT in this list. The release profile is `panic = "abort"`, so nothing in-process
//! can survive a panicking stage — the journey runs in a child process and the parent runs
//! teardown after it, whatever the child did. `suite()` is therefore the child's list only.

use futures::future::BoxFuture;
use kloudlite_workspaces::slo::catalogue::Suite;

use crate::ctx::Ctx;
use crate::stages;

pub struct Stage {
    /// "5 · Workspace" — stored verbatim as the run's and every step's `stage`, so a failed run
    /// reads as a place in the journey.
    pub name: &'static str,
    pub run: fn(&mut Ctx) -> BoxFuture<'_, ()>,
}

/// The stage name the parent files teardown's own steps under.
pub const TEARDOWN: &str = "11 · Teardown";

/// Set to `1` to insert a stage that panics, which is how the out-of-process split is tested at
/// all: nothing else in the binary can be made to abort on demand. Never set in a deployment.
const PANIC_ENV: &str = "KLOUDLITE_SLO_TEST_PANIC";

/// The fast stages, which every suite runs: weekly and monthly are the fast journey PLUS their
/// own extra stages, never a different journey — an SLO whose only samples came from a monthly
/// run would have nothing to compare against.
fn fast() -> Vec<Stage> {
    vec![
        Stage { name: "0 · Boot", run: |c| Box::pin(stages::boot(c)) },
        Stage { name: stages::IDENTITY, run: |c| Box::pin(stages::identity::run(c)) },
        Stage { name: stages::GIT, run: |c| Box::pin(stages::git::run(c)) },
        Stage { name: stages::PULL_REQUEST, run: |c| Box::pin(stages::pr::run(c)) },
        Stage { name: stages::REGISTRY, run: |c| Box::pin(stages::registry::run(c)) },
        Stage { name: stages::WORKSPACE, run: |c| Box::pin(stages::workspace::run(c)) },
        Stage { name: stages::ENVIRONMENT, run: |c| Box::pin(stages::environment::run(c)) },
        Stage { name: stages::LIFECYCLE, run: |c| Box::pin(stages::lifecycle::run(c)) },
        Stage { name: stages::ADMIN, run: |c| Box::pin(stages::admin::run(c)) },
        Stage { name: stages::SECURITY, run: |c| Box::pin(stages::security::run(c)) },
        Stage { name: stages::EDGE, run: |c| Box::pin(stages::edge::run(c)) },
    ]
}

pub fn suite(kind: Suite) -> Vec<Stage> {
    let mut stages = fast();
    // Weekly and monthly are the fast journey PLUS their own stage, appended in that order —
    // monthly is weekly plus one, never a third journey, which is the same rule `journey()` in the
    // catalogue is built on.
    if matches!(kind, Suite::Weekly | Suite::Monthly) {
        stages.push(Stage { name: stages::WEEKLY, run: |c| Box::pin(stages::weekly::run(c)) });
    }
    if kind == Suite::Monthly {
        stages.push(Stage { name: stages::MONTHLY, run: |c| Box::pin(stages::monthly::run(c)) });
    }
    if std::env::var(PANIC_ENV).as_deref() == Ok("1") {
        stages.push(Stage { name: "· Panic", run: |_| Box::pin(async { panic!("test panic") }) });
    }
    stages
}
