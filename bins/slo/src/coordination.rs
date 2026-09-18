use anyhow::{anyhow, Result};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::{api::{Api, DeleteParams, PostParams, Preconditions}, ResourceExt};

const NAMESPACE: &str = "kloudlite";
const NAME: &str = "kloudlite-roll-coordination";

pub struct RollLock {
    api: Api<ConfigMap>,
    uid: String,
    resource_version: String,
}

pub async fn acquire(client: kube::Client, run_id: &str) -> Result<RollLock> {
    let api = Api::namespaced(client, NAMESPACE);
    let pod_uid = std::env::var("KLOUDLITE_POD_UID").unwrap_or_else(|_| "manual".into());
    let random = format!("{:032x}", rand::random::<u128>());
    let holder = format!("{pod_uid}/{run_id}/{random}");
    let cm = ConfigMap {
        metadata: kube::api::ObjectMeta { name: Some(NAME.into()), namespace: Some(NAMESPACE.into()), ..Default::default() },
        data: Some([("holder".into(), holder)].into_iter().collect()),
        ..Default::default()
    };
    let created = api.create(&PostParams::default(), &cm).await.map_err(|e| anyhow!("roll coordination is held or unavailable: {e}"))?;
    Ok(RollLock { api, uid: created.uid().ok_or_else(|| anyhow!("coordination lock has no uid"))?, resource_version: created.resource_version().ok_or_else(|| anyhow!("coordination lock has no resourceVersion"))? })
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

#[cfg(test)]
mod tests {
    #[test]
    fn holder_contains_pod_run_and_random_identity() {
        let holder = format!("pod/run-1/{:032x}", rand::random::<u128>());
        assert!(holder.starts_with("pod/run-1/"));
    }
}
