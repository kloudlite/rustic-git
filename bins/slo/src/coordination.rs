use anyhow::{anyhow, Result};
use k8s_openapi::api::{batch::v1::Job, core::v1::ConfigMap};
use kube::{api::{Api, DeleteParams, PostParams, Preconditions}, ResourceExt};
use std::time::{Duration, Instant};

const NAMESPACE: &str = "kloudlite";
const NAME: &str = "kloudlite-roll-coordination";

fn active_phase(phase: Option<&str>) -> bool {
    !matches!(phase, Some("Succeeded" | "Failed"))
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

pub async fn acquire(client: kube::Client, run_id: &str) -> Result<RollLock> {
    let api = Api::namespaced(client.clone(), NAMESPACE);
    let pod_uid = std::env::var("KLOUDLITE_POD_UID").unwrap_or_else(|_| "manual".into());
    let job_name = std::env::var("KLOUDLITE_SLO_JOB_NAME").unwrap_or_default();
    let random = format!("{:032x}", rand::random::<u128>());
    let holder = format!("{pod_uid}/{run_id}/{random}");
    let mut data = [("holder".into(), holder)].into_iter().collect::<std::collections::BTreeMap<_, _>>();
    if !job_name.is_empty() {
        let jobs: Api<Job> = Api::namespaced(client, NAMESPACE);
        let job = jobs.get(&job_name).await.map_err(|e| anyhow!("could not inspect hourly probe job: {e}"))?;
        data.insert("job_name".into(), job_name);
        data.insert("job_uid".into(), job.uid().ok_or_else(|| anyhow!("hourly probe job has no uid"))?);
        data.insert("owner_pod_uid".into(), pod_uid.clone());
    }
    let cm = ConfigMap {
        metadata: kube::api::ObjectMeta { name: Some(NAME.into()), namespace: Some(NAMESPACE.into()), ..Default::default() },
        data: Some(data),
        ..Default::default()
    };
    let created = api.create(&PostParams::default(), &cm).await.map_err(|e| anyhow!("roll coordination is held or unavailable: {e}"))?;
    Ok(RollLock { api, uid: created.uid().ok_or_else(|| anyhow!("coordination lock has no uid"))?, resource_version: created.resource_version().ok_or_else(|| anyhow!("coordination lock has no resourceVersion"))? })
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
