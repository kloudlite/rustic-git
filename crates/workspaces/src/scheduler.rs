//! Warmth-aware scheduler — spec §Scheduler. Runs at job creation and from the requeue sweep.
//!
//! Picks a candidate agent (alive, in-region, with free capacity for the job's reservation),
//! preferring the workspace's current placement (warm fork/pull) over a cold agent with more
//! free disk. Writes `job.agent` then `workspace.placement`, both etag-CAS, retrying a lost
//! race twice before giving up and leaving the job `Queued` for a later pass.

use crate::model::{AgentDoc, Capacity, Job, JobKind, JobState};
use crate::store::{MetaStore, StoreErr};

/// Same "alive" threshold as the requeue sweep (3x the poll hold).
const AGENT_ALIVE_SECS: i64 = 90;
const RETRIES: u32 = 3;

fn ws_owner_id(job: &Job) -> Option<(String, String)> {
    match job.kind {
        JobKind::WsCreate | JobKind::WsPush | JobKind::WsFork | JobKind::WsClone | JobKind::WsDelete => {
            let owner = job.payload.get("owner")?.as_str()?.to_string();
            let id = job.payload.get("workspace")?.as_str()?.to_string();
            Some((owner, id))
        }
        _ => None,
    }
}

/// Fixed per-job reservation: ws jobs are 1 cpu / 1 GB flat; env jobs sum a per-service
/// 1 cpu / 512 MB across the environment's services, falling back to a single service's worth
/// if the environment doc can't be read (deleted mid-flight, bad payload, etc).
async fn reservation(meta: &dyn MetaStore, job: &Job) -> Result<Capacity, StoreErr> {
    match job.kind {
        JobKind::EnvUp | JobKind::EnvDown | JobKind::EnvDelete => {
            let owner = job.payload.get("owner").and_then(|v| v.as_str());
            let env_id = job.payload.get("environment").and_then(|v| v.as_str());
            let n = match (owner, env_id) {
                (Some(o), Some(id)) => match meta.get_env(o, id).await? {
                    Some((env, _)) => env.services.len() as u64,
                    None => 1,
                },
                _ => 1,
            };
            Ok(Capacity { cpu: n as u32, mem_mb: n * 512, disk_gb: 0 })
        }
        _ => Ok(Capacity { cpu: 1, mem_mb: 1024, disk_gb: 0 }),
    }
}

fn free_disk(a: &AgentDoc) -> u64 {
    a.capacity.disk_gb.saturating_sub(a.used.disk_gb)
}

fn fits(a: &AgentDoc, need: &Capacity) -> bool {
    a.capacity.cpu.saturating_sub(a.used.cpu) >= need.cpu
        && a.capacity.mem_mb.saturating_sub(a.used.mem_mb) >= need.mem_mb
}

/// Place `job` on an agent, returning the chosen agent id, or `None` (job stays `Queued`) if no
/// candidate fits or the CAS race is lost three times in a row.
pub async fn schedule(meta: &dyn MetaStore, job: &Job) -> Result<Option<String>, StoreErr> {
    let now = chrono::Utc::now();
    let need = reservation(meta, job).await?;
    let candidates: Vec<AgentDoc> = meta
        .agents_in(&job.region)
        .await?
        .into_iter()
        .filter(|a| (now - a.heartbeat_at).num_seconds() <= AGENT_ALIVE_SECS)
        .filter(|a| fits(a, &need))
        .collect();
    if candidates.is_empty() {
        return Ok(None);
    }

    let ws_key = ws_owner_id(job);
    let warm = match &ws_key {
        Some((owner, id)) => match meta.get_ws(owner, id).await? {
            Some((ws, _)) => ws.placement.filter(|p| candidates.iter().any(|a| &a.id == p)),
            None => None,
        },
        None => None,
    };
    let chosen = match warm {
        Some(id) => id,
        None => candidates.iter().max_by_key(|a| free_disk(a)).unwrap().id.clone(),
    };

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
    use crate::model::{EnvState, Environment, Service, WsState};
    use crate::store::MemStore;

    fn agent(id: &str, region: &str, cpu: u32, mem_mb: u64, disk_gb: u64) -> AgentDoc {
        AgentDoc {
            id: id.into(),
            region: region.into(),
            hostname: id.into(),
            pool: "/mnt/wspool".into(),
            capacity: Capacity { cpu, mem_mb, disk_gb },
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
    async fn warm_placement_preferred_over_bigger_free_agent() {
        let store = MemStore::new();
        store.upsert_agent(&agent("warm", "r1", 4, 8192, 50)).await.unwrap();
        store.upsert_agent(&agent("big", "r1", 4, 8192, 500)).await.unwrap();
        store.create_ws(&ws("karthik", "ws-1", "r1", Some("warm"))).await.unwrap();

        let job = ws_job("r1", "karthik", "ws-1");
        store.create_job(&job).await.unwrap();
        let placed = schedule(&store, &job).await.unwrap();
        assert_eq!(placed, Some("warm".into()));

        let (got_job, _) = store.get_job("r1", "job-1").await.unwrap().unwrap();
        assert_eq!(got_job.agent, Some("warm".into()));
        let (got_ws, _) = store.get_ws("karthik", "ws-1").await.unwrap().unwrap();
        assert_eq!(got_ws.placement, Some("warm".into()));
    }

    #[tokio::test]
    async fn no_warm_hint_picks_max_free_disk() {
        let store = MemStore::new();
        store.upsert_agent(&agent("small", "r1", 4, 8192, 50)).await.unwrap();
        store.upsert_agent(&agent("big", "r1", 4, 8192, 500)).await.unwrap();
        store.create_ws(&ws("karthik", "ws-1", "r1", None)).await.unwrap();

        let job = ws_job("r1", "karthik", "ws-1");
        store.create_job(&job).await.unwrap();
        let placed = schedule(&store, &job).await.unwrap();
        assert_eq!(placed, Some("big".into()));
    }

    #[tokio::test]
    async fn capacity_excludes_full_agent() {
        let store = MemStore::new();
        let mut full = agent("full", "r1", 1, 1024, 500);
        full.used = Capacity { cpu: 1, mem_mb: 1024, disk_gb: 0 };
        store.upsert_agent(&full).await.unwrap();
        store.upsert_agent(&agent("ok", "r1", 1, 1024, 10)).await.unwrap();
        store.create_ws(&ws("karthik", "ws-1", "r1", None)).await.unwrap();

        let job = ws_job("r1", "karthik", "ws-1");
        store.create_job(&job).await.unwrap();
        let placed = schedule(&store, &job).await.unwrap();
        assert_eq!(placed, Some("ok".into()));
    }

    #[tokio::test]
    async fn dead_agent_excluded() {
        let store = MemStore::new();
        let mut dead = agent("dead", "r1", 4, 8192, 500);
        dead.heartbeat_at = chrono::Utc::now() - chrono::Duration::seconds(200);
        store.upsert_agent(&dead).await.unwrap();
        store.create_ws(&ws("karthik", "ws-1", "r1", None)).await.unwrap();

        let job = ws_job("r1", "karthik", "ws-1");
        store.create_job(&job).await.unwrap();
        let placed = schedule(&store, &job).await.unwrap();
        assert_eq!(placed, None);
        let (got_job, _) = store.get_job("r1", "job-1").await.unwrap().unwrap();
        assert_eq!(got_job.state, JobState::Queued);
        assert_eq!(got_job.agent, None);
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

    #[tokio::test]
    async fn env_job_reservation_counts_services() {
        let store = MemStore::new();
        // 3 services => 3 cpu / 1536 mem needed; one agent has just enough, the other doesn't.
        store.upsert_agent(&agent("small", "r1", 2, 1024, 50)).await.unwrap();
        store.upsert_agent(&agent("big", "r1", 4, 4096, 10)).await.unwrap();
        let svc = |n: &str| Service {
            name: n.into(),
            image: "img".into(),
            command: vec![],
            env: Default::default(),
            mounts: vec![],
        };
        store
            .create_env(&Environment {
                id: "env-1".into(),
                owner: "karthik".into(),
                name: "app".into(),
                region: "r1".into(),
                state: EnvState::Creating,
                placement: None,
                volume: None,
                services: vec![svc("a"), svc("b"), svc("c")],
            })
            .await
            .unwrap();

        let job = Job {
            id: "job-1".into(),
            region: "r1".into(),
            agent: None,
            kind: JobKind::EnvUp,
            payload: serde_json::json!({"environment": "env-1", "owner": "karthik"}),
            state: JobState::Queued,
            lease_until: None,
            attempts: 0,
            error: None,
        };
        store.create_job(&job).await.unwrap();
        let placed = schedule(&store, &job).await.unwrap();
        assert_eq!(placed, Some("big".into()));
    }

    #[tokio::test]
    async fn cas_race_places_exactly_once() {
        let store = std::sync::Arc::new(MemStore::new());
        store.upsert_agent(&agent("a1", "r1", 4, 8192, 500)).await.unwrap();
        store.create_ws(&ws("karthik", "ws-1", "r1", None)).await.unwrap();
        let job = ws_job("r1", "karthik", "ws-1");
        store.create_job(&job).await.unwrap();

        let s1 = store.clone();
        let s2 = store.clone();
        let j1 = job.clone();
        let j2 = job.clone();
        let (r1, r2) = tokio::join!(
            tokio::spawn(async move { schedule(&*s1, &j1).await.unwrap() }),
            tokio::spawn(async move { schedule(&*s2, &j2).await.unwrap() }),
        );
        let (r1, r2) = (r1.unwrap(), r2.unwrap());
        // Exactly one of the two calls wins the placement; the other sees the job no longer
        // Queued and backs off with None.
        let wins = [r1.clone(), r2.clone()].into_iter().filter(|r| r.is_some()).count();
        assert_eq!(wins, 1);
        let (got_job, _) = store.get_job("r1", "job-1").await.unwrap().unwrap();
        assert_eq!(got_job.agent, Some("a1".into()));
    }
}
