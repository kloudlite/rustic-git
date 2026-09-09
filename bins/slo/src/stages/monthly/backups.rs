//! Monthly drills on backups: the tarball's age, the daily slots, versioning, Cosmos.

use super::*;


pub(crate) async fn backups(c: &mut Ctx) {
    tarball_age(c).await;
    daily_slots(c).await;
    versioning(c).await;
    cosmos(c).await;
}


/// Every blob in the backup container, newest-modified first. The storage credential is
/// `AZURE_STORAGE_ACCOUNT_NAME`/`_KEY`, which `object_store` reads from the environment itself —
/// the same Secret every other tier mounts, given to the MONTHLY CronJob only.
pub(crate) async fn slots() -> Result<Vec<(String, chrono::DateTime<Utc>)>> {
    let store = object_store::azure::MicrosoftAzureBuilder::from_env()
        .with_container_name(BACKUP_CONTAINER)
        .build()
        .context("could not reach the backup container")?;
    let objects: Vec<object_store::ObjectMeta> =
        object_store::ObjectStore::list(&store, None).try_collect().await.context("could not list it")?;
    Ok(objects.into_iter().map(|o| (o.location.to_string(), o.last_modified)).collect())
}


/// `bak.tarball.age`: the newest hourly slot is under two hours old.
///
/// The hourly slots, not any blob: the `.hmac` companions and the daily slots are written by the
/// same run, so a `daily-Mon` from Monday would keep this green all week if the age were taken over
/// everything in the container.
pub(crate) async fn tarball_age(c: &mut Ctx) {
    if c.cfg.azure.is_none() {
        return c.skip("bak.tarball.age", "no Azure credential configured");
    }
    c.step("bak.tarball.age", READ_CEILING, |_| {
        async move {
            let all = slots().await?;
            let newest = all
                .iter()
                .filter(|(n, _)| n.starts_with("hourly-") && n.ends_with(SLOT_SUFFIX))
                .map(|(_, at)| *at)
                .max();
            // Naming what IS there, not only what is missing: the first live monthly run found
            // `hourly-03.tgz` — the node's installed unit is an older script that writes plain,
            // unencrypted tarballs — and "no hourly tarball at all" sent the operator looking for
            // a backup that had in fact run.
            let Some(newest) = newest else {
                let seen: Vec<&str> = all
                    .iter()
                    .map(|(n, _)| n.as_str())
                    .filter(|n| n.starts_with("hourly-"))
                    .take(3)
                    .collect();
                return Err(match seen.is_empty() {
                    true => anyhow!("the backup container holds no hourly tarball at all"),
                    false => anyhow!(
                        "the newest hourly blob is {}, not {SLOT_SUFFIX} — the encrypted backup unit is not the one installed on the node",
                        seen.join(", ")
                    ),
                });
            };
            let mins = (Utc::now() - newest).num_minutes();
            if mins >= MAX_TARBALL_AGE_MINS {
                return Err(anyhow!("the newest backup is {mins} minutes old"));
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}


/// `bak.daily.slots`: all seven daily slots exist.
///
/// Existence, not age: the slots are FIXED names that overwrite, so a missing one means a whole
/// weekday's run has never succeeded — which is exactly the failure a single "the newest backup is
/// recent" check cannot see.
pub(crate) async fn daily_slots(c: &mut Ctx) {
    if c.cfg.azure.is_none() {
        return c.skip("bak.daily.slots", "no Azure credential configured");
    }
    c.step("bak.daily.slots", READ_CEILING, |_| {
        async move {
            let rows = slots().await?;
            let have: Vec<String> = rows.iter().map(|(n, _)| n.clone()).collect();
            // The encrypted unit fills one slot a day, so in its first week the slots from before
            // it existed cannot hold anything: only days at or after its oldest blob are due.
            let since = rows.iter().filter(|(n, _)| n.ends_with(SLOT_SUFFIX)).map(|(_, t)| *t).min();
            let missing: Vec<String> = slots_due(Utc::now(), since)
                .into_iter()
                .filter(|want| !have.contains(want))
                .collect();
            if !missing.is_empty() {
                // Same reason as `tarball_age`: an unencrypted `daily-Mon.tgz` beside the missing
                // `daily-Mon.tgz.enc` is a different problem from no backup at all, and the
                // operator should read which one this is.
                let plain: Vec<&String> = have.iter().filter(|n| n.starts_with("daily-") && !n.ends_with(SLOT_SUFFIX)).collect();
                return Err(match plain.is_empty() {
                    true => anyhow!("no backup in slot {}", missing.join(", ")),
                    false => anyhow!(
                        "no backup in slot {} — the container holds unencrypted slots instead ({}), so the encrypted unit is not the one installed",
                        missing.join(", "),
                        plain.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                    ),
                });
            }
            Ok(())
        }
        .boxed()
    })
    .await;
}


/// The daily slots that must be present at `now`: the last seven days' weekdays, minus any day
/// before `since` — the first encrypted blob's time, i.e. when the encrypted unit was installed.
/// `None` (no encrypted blob at all) leaves every slot due, which the caller then reports.
pub(crate) fn slots_due(now: chrono::DateTime<Utc>, since: Option<chrono::DateTime<Utc>>) -> Vec<String> {
    (0..7)
        .map(|back| now - chrono::Duration::days(back))
        .filter(|day| since.is_none_or(|s| day.date_naive() >= s.date_naive()))
        .map(|day| format!("daily-{}{SLOT_SUFFIX}", day.format("%a")))
        .collect()
}


/// `bak.versioning`: blob versioning is on for the account the whole product's data lives in.
///
/// This is the switch that turns the 24+7 overwriting slots into a history longer than a week, and
/// the only thing that saves a good backup a bad one overwrote. It is also the one that is off by
/// default and stays off silently.
pub(crate) async fn versioning(c: &mut Ctx) {
    let Some(az) = c.cfg.azure.clone() else {
        return c.skip("bak.versioning", "no Azure subscription configured");
    };
    let path = format!(
        "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.Storage/storageAccounts/{}/blobServices/default?api-version=2023-01-01",
        az.subscription, az.resource_group, az.storage_account
    );
    c.step("bak.versioning", READ_CEILING, move |c| {
        async move {
            let doc = arm(c, &path).await?;
            match doc.pointer("/properties/isVersioningEnabled").and_then(Value::as_bool) {
                Some(true) => Ok(()),
                // Absent and `false` are the same answer to the only question here.
                _ => Err(anyhow!("blob versioning is OFF on {}", az.storage_account)),
            }
        }
        .boxed()
    })
    .await;
}


/// `bak.cosmos`: the directory and PR store has a backup policy at all.
///
/// The TYPE is what is read, not a job outcome: Cosmos runs the backup itself, and the only thing
/// that can silently be wrong is an account whose policy nobody ever set — the default periodic
/// tier keeps eight hours, which `deploy/BACKUPS.md` asks be migrated to Continuous.
pub(crate) async fn cosmos(c: &mut Ctx) {
    let Some(az) = c.cfg.azure.clone() else {
        return c.skip("bak.cosmos", "no Azure subscription configured");
    };
    let path = format!(
        "/subscriptions/{}/resourceGroups/{}/providers/Microsoft.DocumentDB/databaseAccounts/{}?api-version=2024-05-15",
        az.subscription, az.resource_group, az.cosmos_account
    );
    c.step("bak.cosmos", READ_CEILING, move |c| {
        async move {
            let doc = arm(c, &path).await?;
            match doc.pointer("/properties/backupPolicy/type").and_then(Value::as_str) {
                Some(t) if !t.is_empty() => {
                    tracing::info!(kind = "cosmos", policy = t, "slo.backup.read");
                    Ok(())
                }
                _ => Err(anyhow!("{} has no backup policy", az.cosmos_account)),
            }
        }
        .boxed()
    })
    .await;
}


/// One ARM GET, with a token from the Azure Monitor service principal the collector already holds.
///
/// A client-credentials grant rather than anything cached: the probe runs once a month, and a token
/// cache for a process that makes two requests in its life is code that can only rot.
pub(crate) async fn arm(c: &Ctx, path: &str) -> Result<Value> {
    let var = |k: &str| std::env::var(k).with_context(|| format!("{k} is not set"));
    let (tenant, client, secret) =
        (var("AZURE_TENANT_ID")?, var("AZURE_CLIENT_ID")?, var("AZURE_CLIENT_SECRET")?);
    let token_url = format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token");
    let r = c
        .http
        .post(&token_url)
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form(&[
            ("grant_type", "client_credentials"),
            ("client_id", &client),
            ("client_secret", &secret),
            ("scope", "https://management.azure.com/.default"),
        ]))
        .send()
        .await
        // `without_url` and no body: a token endpoint's error carries the request back, and the
        // request is a client secret.
        .map_err(|e| anyhow!("could not reach Entra: {}", e.without_url()))?;
    if !r.status().is_success() {
        return Err(anyhow!("Entra answered {} to the token request", r.status()));
    }
    let token = r
        .json::<Value>()
        .await
        .ok()
        .and_then(|v| v.get("access_token").and_then(Value::as_str).map(str::to_string))
        .ok_or_else(|| anyhow!("Entra answered no access token"))?;
    super::super::get(c, &format!("https://management.azure.com{path}"), &token).await
}


/// `a=b&c=d`, percent-encoded. A client secret is a random string that may hold anything, and a
/// dependency for one form body would be the wrong trade — `reqwest` is pinned without the feature
/// that would have done it.
pub(crate) fn form(pairs: &[(&str, &str)]) -> String {
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                other => format!("%{other:02X}"),
            })
            .collect::<String>()
    };
    pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}

// ── drills ──────────────────────────────────────────────────────────────
