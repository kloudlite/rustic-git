//! The ordered stage list per suite. The last stage is always teardown, and the runner runs it
//! even after a panic — everything else is what the design's "Stages" paragraph names.

use futures::future::BoxFuture;
use kloudlite_git_workspaces::slo::catalogue::Suite;

use crate::ctx::Ctx;
use crate::stages;

pub struct Stage {
    /// "5 · Workspace" — stored verbatim as the run's and every step's `stage`, so a failed run
    /// reads as a place in the journey.
    pub name: &'static str,
    pub run: fn(&mut Ctx) -> BoxFuture<'_, ()>,
}

/// The fast stages, which every suite runs: weekly and monthly are the fast journey PLUS their
/// own extra stages, never a different journey — an SLO whose only samples came from a monthly
/// run would have nothing to compare against.
fn fast() -> Vec<Stage> {
    vec![Stage { name: "0 · Boot", run: |c| Box::pin(stages::boot(c)) }]
}

pub fn suite(kind: Suite) -> Vec<Stage> {
    let mut stages = fast();
    // Weekly and monthly are the fast journey plus their own stages, which the stage tasks append
    // here; today they add nothing, so every suite is the fast one.
    let _ = kind;
    stages.push(Stage { name: "11 · Teardown", run: |c| Box::pin(stages::teardown(c)) });
    stages
}
