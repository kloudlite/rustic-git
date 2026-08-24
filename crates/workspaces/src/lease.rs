//! Requeue sweep — spec §Scheduler "Requeue sweep": leased jobs past their lease, and all leased
//! jobs belonging to an agent whose heartbeat has aged out, go back to `queued` (or `failed` past
//! the retry budget). Runs on a 30s beat per known region, from the API tier (see `bins/api`).

use crate::model::{AgentDoc, JobState};
use crate::scheduler::schedule;
use crate::store::MetaStore;
use std::collections::HashSet;

/// Same threshold as the "alive" definition in the design doc: 3x the poll hold.
const AGENT_DEAD_AFTER_SECS: i64 = 90;
const MAX_ATTEMPTS: u32 = 3;

pub async fn sweep(meta: &dyn MetaStore, region: &str) -> Result<(), crate::store::StoreErr> {
    let now = chrono::Utc::now();
    let agents = meta.agents_in(region).await?;
    let dead: HashSet<String> = agents
        .iter()
        .filter(|a: &&AgentDoc| (now - a.heartbeat_at).num_seconds() > AGENT_DEAD_AFTER_SECS)
        .map(|a| a.id.clone())
        .collect();

    for (job, etag) in meta.leased_jobs(region).await? {
        let expired = job.lease_until.is_some_and(|until| until < now);
        let agent_dead = job.agent.as_deref().is_some_and(|id| dead.contains(id));
        if !expired && !agent_dead {
            continue;
        }
        let mut j = job;
        j.attempts += 1;
        j.lease_until = None;
        j.agent = None;
        j.state = if j.attempts > MAX_ATTEMPTS { JobState::Failed } else { JobState::Queued };
        // Lost the CAS race to a poller finishing/leasing it in the meantime: fine, leave it.
        let _ = meta.replace_job(&j, &etag).await;
    }

    // Retry placement for every still-queued job, not just ones just requeued above — a job
    // that had no candidate at creation time (region briefly agent-less, etc) gets another shot
    // on each sweep beat instead of waiting forever.
    for (job, _) in meta.queued_jobs(region).await? {
        if job.agent.is_none() {
            let _ = schedule(meta, &job).await?;
        }
    }
    Ok(())
}
