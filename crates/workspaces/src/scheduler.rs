//! Dedicated-node-per-owner scheduler — spec §Scheduler. Runs at job creation and from the
//! requeue sweep.
//!
//! One agent VM serves exactly one owner: every workspace/environment/clone an owner has lands
//! on the same node, so their subvolumes and lineage never need to migrate machines. There is no
//! capacity scoring across an owner's own jobs — it's their node, they get all of it — only a
//! one-time binding step picks which free agent an owner's first job claims.

use crate::model::{AgentDoc, Job, JobKind, JobState};
use crate::store::{MetaStore, StoreErr};

/// Same "alive" threshold as the requeue sweep (3x the poll hold).
const AGENT_ALIVE_SECS: i64 = 90;
const RETRIES: u32 = 3;

fn ws_owner_id(job: &Job) -> Option<(String, String)> {
    match job.kind {
        JobKind::WsCreate | JobKind::WsPush | JobKind::WsClone | JobKind::WsRestore | JobKind::WsDelete => {
            let owner = job.payload.get("owner")?.as_str()?.to_string();
            let id = job.payload.get("workspace")?.as_str()?.to_string();
            Some((owner, id))
        }
        _ => None,
    }
}

fn is_alive(a: &AgentDoc, now: chrono::DateTime<chrono::Utc>) -> bool {
    (now - a.heartbeat_at).num_seconds() <= AGENT_ALIVE_SECS
}

/// Claim a free (undedicated) alive agent for `owner`, CAS-looping over the candidate list so a
/// race with another owner's claim just moves on to the next candidate rather than giving up.
/// Returns the claimed agent's id, or `None` if every free agent was lost to a race or none
/// existed.
async fn claim_free_agent(
    meta: &dyn MetaStore,
    region: &str,
    owner: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<String>, StoreErr> {
    let free: Vec<String> = meta
        .agents_in(region)
        .await?
        .into_iter()
        .filter(|a| is_alive(a, now) && a.dedicated_owner.is_none())
        .map(|a| a.id)
        .collect();
    for id in free {
        let Some((mut a, etag)) = meta.get_agent(region, &id).await? else { continue };
        if a.dedicated_owner.is_some() {
            continue; // claimed by someone else between the listing and here
        }
        a.dedicated_owner = Some(owner.to_string());
        match meta.replace_agent(&a, &etag).await {
            Ok(()) => return Ok(Some(id)),
            Err(StoreErr::CasFailed) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// Place `job` on the owner's dedicated agent, returning the chosen agent id, or `None` (job
/// stays `Queued`) if the owner has no dedicated agent yet and none is free to claim, if the
/// owner's dedicated agent is dead, or if the job-placement CAS race is lost three times running.
pub async fn schedule(meta: &dyn MetaStore, job: &Job) -> Result<Option<String>, StoreErr> {
    let now = chrono::Utc::now();
    let Some(owner) = job.payload.get("owner").and_then(|v| v.as_str()).map(str::to_string) else {
        return Ok(None);
    };

    let agents = meta.agents_in(&job.region).await?;
    let dedicated = agents.iter().find(|a| a.dedicated_owner.as_deref() == Some(owner.as_str()));

    let chosen = match dedicated {
        Some(a) if is_alive(a, now) => a.id.clone(),
        // Owner already has a node, but it's dead: re-homing an owner is a data migration
        // (their subvolumes live on that box), not something the scheduler decides — leave the
        // job queued until an operator (or a future migration job) resolves it.
        Some(_) => return Ok(None),
        None => match claim_free_agent(meta, &job.region, &owner, now).await? {
            Some(id) => id,
            None => return Ok(None),
        },
    };

    let ws_key = ws_owner_id(job);
    for _ in 0..RETRIES {
        let Some((mut j, jetag)) = meta.get_job(&job.region, &job.id).await? else {
            return Ok(None);
        };
        if j.state != JobState::Queued || j.agent.is_some() {
            // Already scheduled (or picked up/terminal) by the time we got here — nothing to do.
            return Ok(None);
        }
        j.agent = Some(chosen.clone());
        match meta.replace_job(&j, &jetag).await {
            Ok(()) => {
                if let Some((owner, id)) = &ws_key {
                    place_ws(meta, owner, id, &chosen).await?;
                }
                return Ok(Some(chosen));
            }
            Err(StoreErr::CasFailed) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// Best-effort: the job is already scheduled by the time this runs, so a lost placement CAS
/// just means the workspace loses this round's warmth hint, not the job.
async fn place_ws(meta: &dyn MetaStore, owner: &str, id: &str, agent: &str) -> Result<(), StoreErr> {
    for _ in 0..RETRIES {
        let Some((mut ws, wetag)) = meta.get_ws(owner, id).await? else { return Ok(()) };
        ws.placement = Some(agent.to_string());
        match meta.replace_ws(&ws, &wetag).await {
            Ok(()) => return Ok(()),
            Err(StoreErr::CasFailed) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Capacity, WsState};
    use crate::store::MemStore;

    fn agent(id: &str, region: &str, disk_gb: u64) -> AgentDoc {
        AgentDoc {
            id: id.into(),
            region: region.into(),
            hostname: id.into(),
            pool: "/mnt/wspool".into(),
            capacity: Capacity { cpu: 4, mem_mb: 8192, disk_gb },
            used: Capacity { cpu: 0, mem_mb: 0, disk_gb: 0 },
            heartbeat_at: chrono::Utc::now(),
            status: "alive".into(),
            dedicated_owner: None,
        }
    }

    fn ws(owner: &str, id: &str, region: &str, placement: Option<&str>) -> crate::model::Workspace {
        crate::model::Workspace {
            id: id.into(),
            owner: owner.into(),
            name: "web".into(),
            region: region.into(),
            state: WsState::Creating,
            image: "nginx:alpine".into(),
            placement: placement.map(|s| s.to_string()),
            volume: None,
            quota_gb: 20,
            live_state: serde_json::Value::Null,
        }
    }

    fn ws_job(region: &str, owner: &str, ws_id: &str) -> Job {
        Job {
            id: "job-1".into(),
            region: region.into(),
            agent: None,
            kind: JobKind::WsCreate,
            payload: serde_json::json!({"workspace": ws_id, "owner": owner}),
            state: JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        }
    }

    #[tokio::test]
    async fn first_job_claims_sole_free_agent() {
        let store = MemStore::new();
        store.upsert_agent(&agent("a1", "r1", 500)).await.unwrap();
        store.create_ws(&ws("A", "ws-1", "r1", None)).await.unwrap();

        let job = ws_job("r1", "A", "ws-1");
        store.create_job(&job).await.unwrap();
        let placed = schedule(&store, &job).await.unwrap();
        assert_eq!(placed, Some("a1".into()));

        let (a, _) = store.get_agent("r1", "a1").await.unwrap().unwrap();
        assert_eq!(a.dedicated_owner, Some("A".into()));
        let (got_ws, _) = store.get_ws("A", "ws-1").await.unwrap().unwrap();
        assert_eq!(got_ws.placement, Some("a1".into()));
    }

    #[tokio::test]
    async fn second_owner_stays_queued_when_no_free_agent() {
        let store = MemStore::new();
        store.upsert_agent(&agent("a1", "r1", 500)).await.unwrap();
        store.create_ws(&ws("A", "ws-1", "r1", None)).await.unwrap();
        store.create_ws(&ws("B", "ws-1", "r1", None)).await.unwrap();

        let job_a = ws_job("r1", "A", "ws-1");
        store.create_job(&job_a).await.unwrap();
        assert_eq!(schedule(&store, &job_a).await.unwrap(), Some("a1".into()));

        let job_b = Job { id: "job-2".into(), ..ws_job("r1", "B", "ws-1") };
        store.create_job(&job_b).await.unwrap();
        let placed = schedule(&store, &job_b).await.unwrap();
        assert_eq!(placed, None);
        let (got, _) = store.get_job("r1", "job-2").await.unwrap().unwrap();
        assert_eq!(got.state, JobState::Queued);
        assert_eq!(got.agent, None);
    }

    #[tokio::test]
    async fn second_owner_claims_newly_added_agent_then_pins_regardless_of_disk() {
        let store = MemStore::new();
        store.upsert_agent(&agent("a1", "r1", 500)).await.unwrap();
        store.create_ws(&ws("A", "ws-1", "r1", None)).await.unwrap();
        let job_a = ws_job("r1", "A", "ws-1");
        store.create_job(&job_a).await.unwrap();
        assert_eq!(schedule(&store, &job_a).await.unwrap(), Some("a1".into()));

        // a1 now has way more free disk than a2, but it belongs to A.
        store.upsert_agent(&agent("a2", "r1", 5)).await.unwrap();
        store.create_ws(&ws("B", "ws-1", "r1", None)).await.unwrap();
        let job_b = Job { id: "job-2".into(), ..ws_job("r1", "B", "ws-1") };
        store.create_job(&job_b).await.unwrap();
        assert_eq!(schedule(&store, &job_b).await.unwrap(), Some("a2".into()));

        // B's next job pins to a2 even though a1 (A's node) reports far more free disk.
        store.create_ws(&ws("B", "ws-2", "r1", None)).await.unwrap();
        let job_b2 = Job { id: "job-3".into(), ..ws_job("r1", "B", "ws-2") };
        store.create_job(&job_b2).await.unwrap();
        assert_eq!(schedule(&store, &job_b2).await.unwrap(), Some("a2".into()));
    }

    #[tokio::test]
    async fn concurrent_owners_race_one_free_agent_exactly_one_claims() {
        let store = std::sync::Arc::new(MemStore::new());
        store.upsert_agent(&agent("a1", "r1", 500)).await.unwrap();
        store.create_ws(&ws("A", "ws-1", "r1", None)).await.unwrap();
        store.create_ws(&ws("B", "ws-1", "r1", None)).await.unwrap();
        let job_a = ws_job("r1", "A", "ws-1");
        let job_b = Job { id: "job-2".into(), ..ws_job("r1", "B", "ws-1") };
        store.create_job(&job_a).await.unwrap();
        store.create_job(&job_b).await.unwrap();

        let s1 = store.clone();
        let s2 = store.clone();
        let (r1, r2) = tokio::join!(
            tokio::spawn(async move { schedule(&*s1, &job_a).await.unwrap() }),
            tokio::spawn(async move { schedule(&*s2, &job_b).await.unwrap() }),
        );
        let (r1, r2) = (r1.unwrap(), r2.unwrap());
        let wins = [r1, r2].into_iter().filter(|r| r.is_some()).count();
        assert_eq!(wins, 1, "exactly one of the two owners should claim the sole free agent");

        let (a, _) = store.get_agent("r1", "a1").await.unwrap().unwrap();
        assert!(a.dedicated_owner == Some("A".into()) || a.dedicated_owner == Some("B".into()));
    }

    #[tokio::test]
    async fn dead_dedicated_agent_leaves_job_queued_without_reclaim() {
        let store = MemStore::new();
        let mut dead = agent("a1", "r1", 500);
        dead.dedicated_owner = Some("A".into());
        dead.heartbeat_at = chrono::Utc::now() - chrono::Duration::seconds(200);
        store.upsert_agent(&dead).await.unwrap();
        store.create_ws(&ws("A", "ws-1", "r1", None)).await.unwrap();

        let job = ws_job("r1", "A", "ws-1");
        store.create_job(&job).await.unwrap();
        let placed = schedule(&store, &job).await.unwrap();
        assert_eq!(placed, None);
        let (got_job, _) = store.get_job("r1", "job-1").await.unwrap().unwrap();
        assert_eq!(got_job.state, JobState::Queued);
        assert_eq!(got_job.agent, None);

        // Still dedicated to A, dead or not — no silent re-homing.
        let (a, _) = store.get_agent("r1", "a1").await.unwrap().unwrap();
        assert_eq!(a.dedicated_owner, Some("A".into()));
    }

    #[tokio::test]
    async fn no_candidates_leaves_job_queued() {
        let store = MemStore::new();
        store.create_ws(&ws("karthik", "ws-1", "r1", None)).await.unwrap();
        let job = ws_job("r1", "karthik", "ws-1");
        store.create_job(&job).await.unwrap();
        let placed = schedule(&store, &job).await.unwrap();
        assert_eq!(placed, None);
        let (got_job, _) = store.get_job("r1", "job-1").await.unwrap().unwrap();
        assert_eq!(got_job.state, JobState::Queued);
    }
}
