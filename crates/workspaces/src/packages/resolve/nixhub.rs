//! Nixhub (`KLOUDLITE_NIXHUB_URL`) as the first index a pinned `attr@version` is asked of.

use super::*;


pub struct Nixhub {
    pub client: reqwest::Client,
    /// `https://search.devbox.sh`, no trailing slash.
    pub base: String,
}


#[async_trait]
impl Index for Nixhub {
    async fn resolve(&self, attr: &str, version: &VersionReq) -> Result<Option<Lock>, String> {
        let url = format!("{}/v2/resolve", self.base);
        let r = self
            .client
            .get(&url)
            .query(&[("name", attr), ("version", req_str(version))])
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !r.status().is_success() {
            return Err(format!("nixhub answered {}", r.status()));
        }
        let body: serde_json::Value = r.json().await.map_err(|e| e.to_string())?;
        nixhub_lock(attr, version, &body)
    }

    async fn versions(&self, attr: &str) -> Result<Vec<String>, String> {
        let url = format!("{}/v2/pkg", self.base);
        let r = self
            .client
            .get(&url)
            .query(&[("name", attr)])
            .timeout(HTTP_TIMEOUT)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !r.status().is_success() {
            return Err(format!("nixhub answered {}", r.status()));
        }
        let body: serde_json::Value = r.json().await.map_err(|e| e.to_string())?;
        // Already newest-first from the API; we keep its order rather than re-sorting, so a
        // version string it knows how to order and we do not (a date-shaped release, say) is not
        // scrambled by our numeric comparison.
        Ok(body["releases"]
            .as_array()
            .map(|rs| {
                rs.iter()
                    .filter_map(|r| r["version"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }
}


/// The shape verified against the live API on 2026-09-08.
///
/// Two different failures, deliberately kept apart: a body that simply does not carry OUR system
/// is `Ok(None)` — the version exists, just not for x86_64-linux, which is "unknown" from here —
/// while a body that carries the system but is missing a field we need is `Err`. A broken upstream
/// response is not the person's typo, and answering it with "nobody published that" would send
/// them hunting for a version that exists.
pub(crate) fn nixhub_lock(
    attr: &str,
    version: &VersionReq,
    body: &serde_json::Value,
) -> Result<Option<Lock>, String> {
    let Some(sys) = body["systems"].get(SYSTEM) else {
        return Ok(None);
    };
    let inst = &sys["flake_installable"];
    let outputs = sys["outputs"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let store_path = outputs
        .iter()
        .find(|o| o["default"].as_bool() == Some(true))
        .or(outputs.first())
        .and_then(|o| o["path"].as_str());
    let (Some(v), Some(attr_path), Some(rev), Some(store_path)) = (
        body["version"].as_str(),
        inst["attr_path"].as_str(),
        inst["ref"]["rev"].as_str(),
        store_path,
    ) else {
        return Err(format!(
            "nixhub answered for {attr} without version, attr_path, rev or an output path"
        ));
    };
    Ok(Some(Lock {
        entry: entry_string(attr, version),
        version: v.to_string(),
        attr_path: attr_path.to_string(),
        rev: rev.to_string(),
        store_path: store_path.to_string(),
        resolved_at: String::new(), // stamped by the Resolver, the one holder of the clock
        source: LockSource::Nixhub,
    }))
}
