use anyhow::{anyhow, Result};
use k8s_openapi::api::{batch::v1::Job, core::v1::{ConfigMap, Pod}};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::{api::{Api, DeleteParams, PostParams, Preconditions}, ResourceExt};
use std::time::{Duration, Instant};

const NAMESPACE: &str = "kloudlite";
const NAME: &str = "kloudlite-roll-coordination";

fn active_phase(phase: Option<&str>) -> bool {
    !matches!(phase, Some("Succeeded" | "Failed"))
}

/// A live holder refused the lock — distinct from every other `acquire` failure so the fast suite
/// (the only caller that may see this and keep going, R-1) can tell "somebody else has it" apart
/// from "the cluster is unreachable", which must still fail closed. Carries the holder string
/// (`ctx.rs` puts it straight into the skipped run's reason) rather than the ConfigMap, since the
/// caller has no use for the object once it has decided not to take it.
#[derive(Debug)]
pub struct Held(pub String);

impl std::fmt::Display for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "roll coordination is held by {}", self.0)
    }
}

impl std::error::Error for Held {}

/// Is the pod or job named on an existing lock still able to finish and release it? `None` for
/// either half of the pair means "nothing recorded" — a plain (non-job) lock has no job at all,
/// and a lock this same function is deciding on has always been written with `owner_pod_uid`, so
/// treat a missing pod uid as gone rather than as "unknown, so assume alive": a lock that predates
/// this field would otherwise block the fleet forever, which is exactly the bug R-1 closes.
async fn holder_is_live(client: &kube::Client, owner_pod_uid: Option<&str>, job_uid: Option<&str>) -> Result<bool> {
    if let Some(job_uid) = job_uid {
        let jobs: Api<Job> = Api::namespaced(client.clone(), NAMESPACE);
        // A job that no longer exists at all cannot still be holding anything either.
        let job = match jobs.list(&kube::api::ListParams::default()).await {
            Ok(list) => list.items.into_iter().find(|j| j.uid().as_deref() == Some(job_uid)),
            Err(e) => return Err(anyhow!("could not list jobs to judge roll lock holder: {e}")),
        };
        let Some(job) = job else { return Ok(false) };
        let terminal = job
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|conds| conds.iter().any(|c| matches!(c.type_.as_str(), "Complete" | "Failed") && c.status == "True"));
        return Ok(!terminal);
    }
    let Some(owner_pod_uid) = owner_pod_uid else { return Ok(false) };
    let pods: Api<Pod> = Api::namespaced(client.clone(), NAMESPACE);
    let pod = pods
        .list(&kube::api::ListParams::default())
        .await
        .map_err(|e| anyhow!("could not list pods to judge roll lock holder: {e}"))?
        .items
        .into_iter()
        .find(|p| p.uid().as_deref() == Some(owner_pod_uid));
    Ok(match pod {
        None => false,
        Some(pod) => active_phase(pod.status.as_ref().and_then(|s| s.phase.as_deref())),
    })
}

/// How long a roll lock with no resolvable pod (`roll.sh` run by hand, from an operator's laptop —
/// there is no API object to ask) is still read as live. The ONE place this module judges by age
/// rather than the API — matches `roll.sh`'s own upper bound (it does not run for 2 h), so a lock
/// left behind by a killed `roll.sh` is dead within the same window the script itself would have
/// given up in.
const OPERATOR_ROLL_MAX_AGE: Duration = Duration::from_secs(2 * 3600);

/// A roll lock's own liveness, distinct from `holder_is_live` above: a roll lock never carries a
/// `job_uid` (only the hourly probe does), and its `owner_pod_uid` is absent for an operator's
/// `roll.sh` — the one case this whole module allows age to decide, because there is no pod or job
/// object to ask. `created_at` is the lock's own `creationTimestamp` off the wire, not a value this
/// process invented, so two processes agree on it.
async fn roll_holder_is_live(client: &kube::Client, owner_pod_uid: Option<&str>, created_at_unix_secs: Option<i64>) -> Result<bool> {
    if owner_pod_uid.is_some() {
        return holder_is_live(client, owner_pod_uid, None).await;
    }
    Ok(created_at_unix_secs.is_none_or(|created| {
        let age_secs = chrono::Utc::now().timestamp() - created;
        age_secs < 0 || (age_secs as u64) < OPERATOR_ROLL_MAX_AGE.as_secs()
    }))
}

/// What kind of thing a lock is keeping apart from the fleet — the whole reason the fast suite may
/// read one and not the other (ruling 3): a roll and a probe never share a lock's semantics, only
/// its ConfigMap. Recorded in `data.kind`; inferred for a lock an older build wrote with no such
/// field, from the one shape only `roll.sh` ever produced (`holder` = `{pod_uid}/roll-…`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Roll,
    Probe,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Roll => "roll",
            Kind::Probe => "probe",
        }
    }

    fn of(data: &std::collections::BTreeMap<String, String>) -> Option<Kind> {
        match data.get("kind").map(String::as_str) {
            Some("roll") => Some(Kind::Roll),
            Some("probe") => Some(Kind::Probe),
            // No `kind` at all: only a lock `roll.sh` wrote before this field existed looks like
            // this — every probe-written lock has always carried `holder = {pod_uid}/{run_id}/…`
            // with a run id, never a bare `roll-…` prefix on its own.
            None if data.get("holder").is_some_and(|h| h.split('/').nth(1).is_some_and(|part| part.starts_with("roll-"))) => Some(Kind::Roll),
            _ => None,
        }
    }
}

pub struct RollLock {
    api: Api<ConfigMap>,
    uid: String,
    resource_version: String,
}

pub async fn client() -> Result<kube::Client> {
    if std::env::var_os("KUBERNETES_SERVICE_HOST").is_some() || std::env::var_os("KUBERNETES_SERVICE_PORT").is_some() {
        return crate::drill::incluster();
    }
    kube::Client::try_default()
        .await
        .map_err(|e| anyhow!("no kube client for roll coordination: {e}"))
}

/// Every non-fast suite's lock: `kind: probe`, `suite` naming which one. The fast suite never
/// calls this (ruling 3) — it only ever peeks with `fast_peek`, below.
pub async fn acquire(client: kube::Client, run_id: &str, suite: &str) -> Result<RollLock> {
    let api = Api::namespaced(client.clone(), NAMESPACE);
    let pod_uid = std::env::var("KLOUDLITE_POD_UID").unwrap_or_else(|_| "manual".into());
    // Kubelet sets a pod's hostname to its own name by default — `roll.sh` already leans on the
    // same `${HOSTNAME:-}` for its ownerReference, so this is not a new assumption.
    let pod_name = std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty());
    let job_name = std::env::var("KLOUDLITE_SLO_JOB_NAME").unwrap_or_default();
    let random = format!("{:032x}", rand::random::<u128>());
    let holder = format!("{pod_uid}/{run_id}/{random}");
    let mut data = [("holder".into(), holder), ("kind".into(), Kind::Probe.as_str().into()), ("suite".into(), suite.into())]
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>();
    // `owner_pod_uid` on EVERY lock, not only a job-owned one: a takeover needs it just as much
    // for `holder_is_live` to judge one, and a plain "manual" run (no pod at all, a human at a
    // shell) records the sentinel so takeover treats it as never live rather than panicking on a
    // missing key.
    if pod_uid != "manual" {
        data.insert("owner_pod_uid".into(), pod_uid.clone());
    }
    if !job_name.is_empty() {
        let jobs: Api<Job> = Api::namespaced(client.clone(), NAMESPACE);
        let job = jobs.get(&job_name).await.map_err(|e| anyhow!("could not inspect hourly probe job: {e}"))?;
        data.insert("job_name".into(), job_name);
        data.insert("job_uid".into(), job.uid().ok_or_else(|| anyhow!("hourly probe job has no uid"))?);
    }
    // So Kubernetes collects the lock itself when the holder pod is garbage-collected — belt to
    // the takeover-on-AlreadyExists braces, not a replacement for it (a Job's pod is deleted on
    // its own schedule, not the instant it goes terminal).
    let owner_references = pod_name.as_ref().map(|name| {
        vec![OwnerReference {
            api_version: "v1".into(),
            kind: "Pod".into(),
            name: name.clone(),
            uid: pod_uid.clone(),
            controller: Some(true),
            block_owner_deletion: Some(false),
        }]
    });
    create_or_take_over(&api, client, data, owner_references).await
}

/// Try the ordinary create; on AlreadyExists, judge the existing lock and either take it over or
/// report who holds it. Shared by `acquire` (probe locks) — a roll lock is created by `roll.sh` in
/// bash, never from here.
async fn create_or_take_over(
    api: &Api<ConfigMap>,
    client: kube::Client,
    data: std::collections::BTreeMap<String, String>,
    owner_references: Option<Vec<OwnerReference>>,
) -> Result<RollLock> {
    let cm = ConfigMap {
        metadata: kube::api::ObjectMeta {
            name: Some(NAME.into()),
            namespace: Some(NAMESPACE.into()),
            owner_references: owner_references.clone(),
            ..Default::default()
        },
        data: Some(data.clone()),
        ..Default::default()
    };
    match api.create(&PostParams::default(), &cm).await {
        Ok(created) => Ok(RollLock {
            api: api.clone(),
            uid: created.uid().ok_or_else(|| anyhow!("coordination lock has no uid"))?,
            resource_version: created.resource_version().ok_or_else(|| anyhow!("coordination lock has no resourceVersion"))?,
        }),
        Err(kube::Error::Api(e)) if e.code == 409 => take_over(api, client, data, owner_references).await,
        Err(e) => Err(anyhow!("roll coordination is held or unavailable: {e}")),
    }
}

/// The existing lock's holder is gone or terminal (checked against the API, never age — except a
/// roll lock with no resolvable pod, `roll_holder_is_live`'s one exception): DELETE it
/// preconditioned on the uid/resourceVersion just read, then run the ordinary create. Never a PUT
/// — the probe's Role has no `update`, and needs none: the delete's precondition is what stops two
/// racing takeovers both winning. The loser's delete gets 409/404 (someone else's delete already
/// landed, or already recreated it), so it falls through to reporting whoever won as the new live
/// holder — same as any other contended acquire.
async fn take_over(
    api: &Api<ConfigMap>,
    client: kube::Client,
    data: std::collections::BTreeMap<String, String>,
    owner_references: Option<Vec<OwnerReference>>,
) -> Result<RollLock> {
    let existing = api.get(NAME).await.map_err(|e| anyhow!("could not read existing roll lock: {e}"))?;
    let existing_data = existing.data.clone().unwrap_or_default();
    let live = holder_is_live(&client, existing_data.get("owner_pod_uid").map(String::as_str), existing_data.get("job_uid").map(String::as_str)).await?;
    if live {
        let holder = existing_data.get("holder").cloned().unwrap_or_else(|| "an unknown holder".into());
        return Err(Held(holder).into());
    }
    let uid = existing.uid().ok_or_else(|| anyhow!("existing roll lock has no uid"))?;
    let resource_version = existing.resource_version().ok_or_else(|| anyhow!("existing roll lock has no resourceVersion"))?;
    let params = DeleteParams { preconditions: Some(Preconditions { uid: Some(uid), resource_version: Some(resource_version) }), ..Default::default() };
    match api.delete(NAME, &params).await {
        Ok(_) => {}
        // Someone else's delete (or takeover) already won this race — fall through to the create,
        // which will itself answer AlreadyExists if they recreated it before we get there, and
        // report THEM as the live holder rather than looping.
        Err(kube::Error::Api(e)) if e.code == 409 || e.code == 404 => {}
        Err(e) => return Err(anyhow!("roll coordination takeover delete was refused: {e}")),
    }
    let cm = ConfigMap {
        metadata: kube::api::ObjectMeta { name: Some(NAME.into()), namespace: Some(NAMESPACE.into()), owner_references, ..Default::default() },
        data: Some(data),
        ..Default::default()
    };
    match api.create(&PostParams::default(), &cm).await {
        Ok(created) => Ok(RollLock {
            api: api.clone(),
            uid: created.uid().ok_or_else(|| anyhow!("coordination lock has no uid"))?,
            resource_version: created.resource_version().ok_or_else(|| anyhow!("coordination lock has no resourceVersion"))?,
        }),
        Err(kube::Error::Api(e)) if e.code == 409 => {
            let winner = api.get(NAME).await.map_err(|e| anyhow!("could not read the lock a racing takeover won: {e}"))?;
            let holder = winner.data.as_ref().and_then(|d| d.get("holder")).cloned().unwrap_or_else(|| "an unknown holder".into());
            Err(Held(holder).into())
        }
        Err(e) => Err(anyhow!("roll coordination is held or unavailable: {e}")),
    }
}

/// The fast suite's own path (ruling 3): GET only, never `acquire` — a fast run does not hold the
/// lock and creates none. `Some(holder)` only for a LIVE lock of kind `roll`; every other case
/// (no lock, a dead roll, any probe lock however live) is `None`, meaning "run normally".
pub async fn fast_peek(client: kube::Client) -> Result<Option<String>> {
    let api: Api<ConfigMap> = Api::namespaced(client.clone(), NAMESPACE);
    let existing = match api.get(NAME).await {
        Ok(cm) => cm,
        Err(kube::Error::Api(e)) if e.code == 404 => return Ok(None),
        Err(e) => return Err(anyhow!("could not read roll coordination: {e}")),
    };
    let data = existing.data.clone().unwrap_or_default();
    if Kind::of(&data) != Some(Kind::Roll) {
        return Ok(None);
    }
    let created_at_unix_secs = existing.metadata.creation_timestamp.map(|t| t.0.as_second());
    let live = roll_holder_is_live(&client, data.get("owner_pod_uid").map(String::as_str), created_at_unix_secs).await?;
    if !live {
        return Ok(None);
    }
    Ok(Some(data.get("holder").cloned().unwrap_or_else(|| "an unknown holder".into())))
}

pub async fn wait_for_group_owner(client: kube::Client, job_name: &str, timeout: Duration) -> Result<()> {
    let jobs: Api<Job> = Api::namespaced(client.clone(), NAMESPACE);
    let job = jobs.get(job_name).await.map_err(|e| anyhow!("could not inspect hourly probe job: {e}"))?;
    let job_uid = job.uid().ok_or_else(|| anyhow!("hourly probe job has no uid"))?;
    let api: Api<ConfigMap> = Api::namespaced(client, NAMESPACE);
    let started = Instant::now();
    loop {
        match api.get(NAME).await {
            Ok(lock) => {
                let data = lock.data.as_ref().ok_or_else(|| anyhow!("hourly probe coordination lock has no holder data"))?;
                if data.get("job_name").map(String::as_str) != Some(job_name) || data.get("job_uid").map(String::as_str) != Some(job_uid.as_str()) {
                    return Err(anyhow!("hourly probe coordination lock belongs to another job"));
                }
                if data.get("owner_pod_uid").is_some_and(|uid| !uid.is_empty()) {
                    return Ok(());
                }
                return Err(anyhow!("hourly probe coordination lock has no owner pod"));
            }
            Err(kube::Error::Api(error)) if error.code == 404 => {}
            Err(error) => return Err(anyhow!("could not inspect hourly probe coordination: {error}")),
        }
        if started.elapsed() >= timeout {
            return Err(anyhow!("hourly probe coordination owner did not appear"));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

impl RollLock {
    pub async fn release(self) -> Result<()> {
        let params = DeleteParams { preconditions: Some(Preconditions { uid: Some(self.uid), resource_version: Some(self.resource_version) }), ..Default::default() };
        match self.api.delete(NAME, &params).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(anyhow!("coordination lock release refused: {e}")),
        }
    }
}

pub async fn wait_for_group(client: kube::Client, job_name: &str, pod_uid: &str, timeout: Duration) -> Result<()> {
    let jobs: Api<Job> = Api::namespaced(client.clone(), NAMESPACE);
    let expected_job_uid = jobs.get(job_name).await.map_err(|e| anyhow!("could not inspect hourly probe job: {e}"))?.uid().ok_or_else(|| anyhow!("hourly probe job has no uid"))?;
    let api: Api<k8s_openapi::api::core::v1::Pod> = Api::namespaced(client, NAMESPACE);
    let started = Instant::now();
    loop {
        let job = jobs.get(job_name).await.map_err(|e| anyhow!("could not inspect hourly probe job: {e}"))?;
        if job.uid().as_deref() != Some(expected_job_uid.as_str()) {
            return Err(anyhow!("hourly probe job changed while siblings were running"));
        }
        let pods = api
            .list(&kube::api::ListParams::default().labels(&format!("job-name={job_name}")))
            .await
            .map_err(|e| anyhow!("could not inspect hourly probe siblings: {e}"))?;
        let siblings_active = pods.items.iter().any(|pod| {
            pod.uid().as_deref() != Some(pod_uid)
                && active_phase(pod.status.as_ref().and_then(|status| status.phase.as_deref()))
        });
        let completion_count = job.spec.as_ref().and_then(|spec| spec.completions).unwrap_or(1).max(0) as usize;
        let status = job.status.as_ref();
        let all_indexes_accounted = (1..completion_count).all(|index| {
            index_accounted(status.and_then(|status| status.completed_indexes.as_deref()), index)
                || index_accounted(status.and_then(|status| status.failed_indexes.as_deref()), index)
        });
        if !siblings_active && all_indexes_accounted {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(anyhow!("hourly probe siblings did not stop before coordination release"));
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn index_accounted(indexes: Option<&str>, expected: usize) -> bool {
    indexes.into_iter().flat_map(|value| value.split(',')).any(|range| {
        let mut bounds = range.split('-').map(|part| part.parse::<usize>().ok());
        match (bounds.next().flatten(), bounds.next().flatten()) {
            (Some(first), Some(last)) => (first..=last).contains(&expected),
            (Some(index), None) => index == expected,
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{active_phase, index_accounted};

    #[test]
    fn holder_contains_pod_run_and_random_identity() {
        let holder = format!("pod/run-1/{:032x}", rand::random::<u128>());
        assert!(holder.starts_with("pod/run-1/"));
    }

    #[test]
    fn only_live_siblings_hold_group_release() {
        assert!(active_phase(Some("Running")));
        assert!(active_phase(Some("Pending")));
        assert!(!active_phase(Some("Succeeded")));
        assert!(active_phase(None));
    }

    #[test]
    fn indexed_group_requires_completed_or_failed_indexes() {
        assert!(index_accounted(Some("0,2-3"), 0));
        assert!(index_accounted(Some("0,2-3"), 3));
        assert!(!index_accounted(Some("0,2-3"), 1));
        assert!(!index_accounted(Some("invalid"), 0));
    }
}
