//! Shared-node owner binding scheduler — spec §Scheduler. Runs at job creation and from the
//! requeue sweep.
//!
//! Every owner (a user or a team slug) is bound to exactly one agent VM per region — all their
//! workspaces/environments/clones land on that node, so their subvolumes and lineage never need
//! to migrate machines — but one node hosts many owners (unlike the old exclusive
//! dedicated-node-per-owner model). The binding is a one-time claim: an owner's first job in a
//! region picks (and persists) the least-loaded alive agent; every later job just looks the
//! binding up and pins there.

use crate::model::{AgentDoc, Binding, Job, JobKind, JobState};
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

/// Free capacity on an agent: `capacity - used`. Mem is the primary axis (the resource that most
/// often exhausts a shared multi-tenant VM first in practice), disk gb the tiebreak.
fn free(a: &AgentDoc) -> (u64, u64) {
    (
        a.capacity.mem_mb.saturating_sub(a.used.mem_mb),
        a.capacity.disk_gb.saturating_sub(a.used.disk_gb),
    )
}

/// Pick the least-loaded alive agent (max free mem, disk tiebreak) and try to bind `owner` to it.
/// A lost `create_binding` race (`Conflict`) means another job for the same owner won first —
/// re-read the binding it created and pin to that instead of retrying the pick. Returns `None`
/// only when there is no alive agent at all.
async fn bind_owner(
    meta: &dyn MetaStore,
    region: &str,
    owner: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<String>, StoreErr> {
    let best = meta
        .agents_in(region)
        .await?
        .into_iter()
        .filter(|a| is_alive(a, now))
        .max_by_key(free)
        .map(|a| a.id);
    let Some(agent) = best else { return Ok(None) };

    let binding = Binding { id: owner.to_string(), region: region.to_string(), agent: agent.clone() };
    match meta.create_binding(&binding).await {
        Ok(()) => Ok(Some(agent)),
        Err(StoreErr::Conflict) => {
            // Someone raced us to the first binding for this owner — adopt theirs rather than
            // erroring, so both callers converge on the same node.
            match meta.get_binding(region, owner).await? {
                Some(b) => Ok(Some(b.agent)),
                None => Ok(Some(agent)), // binding vanished between the conflict and the re-read; retry-safe fallback
            }
        }
        Err(e) => Err(e),
    }
}

/// Place `job` on the owner's bound agent, returning the chosen agent id, or `None` (job stays
/// `Queued`) if there is no alive agent to bind to yet, if the owner's bound agent is dead, or if
/// the job-placement CAS race is lost three times running.
pub async fn schedule(meta: &dyn MetaStore, job: &Job) -> Result<Option<String>, StoreErr> {
    let now = chrono::Utc::now();
    let Some(owner) = job.payload.get("owner").and_then(|v| v.as_str()).map(str::to_string) else {
        return Ok(None);
    };

    let existing = meta.get_binding(&job.region, &owner).await?;
    let chosen = match existing {
        Some(b) => {
            let agents = meta.agents_in(&job.region).await?;
            match agents.iter().find(|a| a.id == b.agent) {
                Some(a) if is_alive(a, now) => b.agent,
                // The owner's node is dead (or gone): re-homing an owner is a migration (their
                // subvolumes live on that box), not something the scheduler decides — leave the
                // job queued until an operator (or a future migration job) resolves it.
                _ => return Ok(None),
            }
        }
        None => match bind_owner(meta, &job.region, &owner, now).await? {
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

    fn agent(id: &str, region: &str, mem_mb: u64) -> AgentDoc {
        AgentDoc {
            id: id.into(),
            region: region.into(),
            hostname: id.into(),
            pool: "/mnt/wspool".into(),
            capacity: Capacity { cpu: 4, mem_mb, disk_gb: 500 },
            used: Capacity { cpu: 0, mem_mb: 0, disk_gb: 0 },
            heartbeat_at: chrono::Utc::now(),
            status: "alive".into(),
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
    async fn first_job_creates_binding_to_sole_agent() {
        let store = MemStore::new();
        store.upsert_agent(&agent("a1", "r1", 8192)).await.unwrap();
        store.create_ws(&ws("A", "ws-1", "r1", None)).await.unwrap();

        let job = ws_job("r1", "A", "ws-1");
        store.create_job(&job).await.unwrap();
        let placed = schedule(&store, &job).await.unwrap();
        assert_eq!(placed, Some("a1".into()));

        let b = store.get_binding("r1", "A").await.unwrap().unwrap();
        assert_eq!(b.agent, "a1");
        let (got_ws, _) = store.get_ws("A", "ws-1").await.unwrap().unwrap();
        assert_eq!(got_ws.placement, Some("a1".into()));
    }

    #[tokio::test]
    async fn second_owner_may_share_the_same_least_loaded_node() {
        let store = MemStore::new();
        store.upsert_agent(&agent("a1", "r1", 8192)).await.unwrap();
        store.create_ws(&ws("A", "ws-1", "r1", None)).await.unwrap();
        store.create_ws(&ws("B", "ws-1", "r1", None)).await.unwrap();

        let job_a = ws_job("r1", "A", "ws-1");
        store.create_job(&job_a).await.unwrap();
        assert_eq!(schedule(&store, &job_a).await.unwrap(), Some("a1".into()));

        // Only one agent exists, so B's binding lands on the SAME node as A's — sharing, not
        // exclusive dedication.
        let job_b = Job { id: "job-2".into(), ..ws_job("r1", "B", "ws-1") };
        store.create_job(&job_b).await.unwrap();
        let placed = schedule(&store, &job_b).await.unwrap();
        assert_eq!(placed, Some("a1".into()));
    }

    #[tokio::test]
    async fn owner_pins_to_their_binding_regardless_of_later_load_changes() {
        let store = MemStore::new();
        store.upsert_agent(&agent("a1", "r1", 8192)).await.unwrap();
        store.create_ws(&ws("A", "ws-1", "r1", None)).await.unwrap();
        let job_a = ws_job("r1", "A", "ws-1");
        store.create_job(&job_a).await.unwrap();
        assert_eq!(schedule(&store, &job_a).await.unwrap(), Some("a1".into()));

        // a2 now reports way more free mem than a1, but A is already bound to a1.
        store.upsert_agent(&agent("a2", "r1", 65536)).await.unwrap();
        store.create_ws(&ws("A", "ws-2", "r1", None)).await.unwrap();
        let job_a2 = Job { id: "job-2".into(), ..ws_job("r1", "A", "ws-2") };
        store.create_job(&job_a2).await.unwrap();
        assert_eq!(schedule(&store, &job_a2).await.unwrap(), Some("a1".into()));
    }

    #[tokio::test]
    async fn concurrent_first_jobs_for_one_owner_converge_on_one_binding() {
        let store = std::sync::Arc::new(MemStore::new());
        store.upsert_agent(&agent("a1", "r1", 8192)).await.unwrap();
        store.upsert_agent(&agent("a2", "r1", 8192)).await.unwrap();
        store.create_ws(&ws("A", "ws-1", "r1", None)).await.unwrap();
        store.create_ws(&ws("A", "ws-2", "r1", None)).await.unwrap();
        let job_1 = ws_job("r1", "A", "ws-1");
        let job_2 = Job { id: "job-2".into(), ..ws_job("r1", "A", "ws-2") };
        store.create_job(&job_1).await.unwrap();
        store.create_job(&job_2).await.unwrap();

        let s1 = store.clone();
        let s2 = store.clone();
        let (r1, r2) = tokio::join!(
            tokio::spawn(async move { schedule(&*s1, &job_1).await.unwrap() }),
            tokio::spawn(async move { schedule(&*s2, &job_2).await.unwrap() }),
        );
        let (r1, r2) = (r1.unwrap(), r2.unwrap());
        // Both jobs are for the same owner, so both must land on the SAME node — whichever one
        // won the create_binding race, the loser adopts it.
        assert!(r1.is_some() && r2.is_some());
        assert_eq!(r1, r2);
    }

    #[tokio::test]
    async fn dead_bound_agent_leaves_job_queued_without_reclaim() {
        let store = MemStore::new();
        let mut dead = agent("a1", "r1", 8192);
        dead.heartbeat_at = chrono::Utc::now() - chrono::Duration::seconds(200);
        store.upsert_agent(&dead).await.unwrap();
        store
            .create_binding(&Binding { id: "A".into(), region: "r1".into(), agent: "a1".into() })
            .await
            .unwrap();
        store.create_ws(&ws("A", "ws-1", "r1", None)).await.unwrap();

        let job = ws_job("r1", "A", "ws-1");
        store.create_job(&job).await.unwrap();
        let placed = schedule(&store, &job).await.unwrap();
        assert_eq!(placed, None);
        let (got_job, _) = store.get_job("r1", "job-1").await.unwrap().unwrap();
        assert_eq!(got_job.state, JobState::Queued);
        assert_eq!(got_job.agent, None);

        // Still bound to a1, dead or not — no silent re-homing.
        let b = store.get_binding("r1", "A").await.unwrap().unwrap();
        assert_eq!(b.agent, "a1");
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
