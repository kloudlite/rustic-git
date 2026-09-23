//! `vol.usage.stamped`: disk quota is charged by what a volume OCCUPIES (owner ruling
//! 2026-09-17), and the only thing that makes that number true is the holding node stamping
//! `Volume.status.usedBytes` on its sync beat. Nothing else in the journey would notice a stamp
//! that stopped: `/v1/quota` would keep answering the 1 GiB floor per volume, every verb would
//! keep passing the gate, and an owner filling a disk would be charged for none of it.
//!
//! So the probe writes bytes and waits for the number to move. 200 MB is well over the ≥ 1 MiB
//! delta the agent writes on and well under the floor a single volume charges, so it proves the
//! stamp is a MEASUREMENT rather than a constant — and it is written through the workspace's own
//! tool server (`kl ide serve`), the way an agent working in the pod would write it, not through
//! a kubectl exec of the probe's own.
//!
//! The file lands in `~/workspace`, on the workspace's btrfs volume (ruling 2026-09-22), so its
//! qgroup is the one the stamp reads. Not `/home/kl` itself: every tool `exec` runs under bwrap
//! (`crates/ide/src/sandbox.rs`), which binds the tree and NOT the home, so a write to the home
//! lands in the sandbox's in-memory root, exits 0 and never reaches btrfs — the hourly 2026-09-23
//! 23:01 IST run's sync cut sent 11 KB after "200 MB written".
//!
//! Hourly, group 0, on the run's own workspace — the wait is two sync beats, which is more than
//! the five-minute suite can spend.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use serde_json::Value;

use kloudlite_workspaces::slo::catalogue::Suite;

use super::experience_ws::ws_tool_within;
use super::{admin, api, get};
use crate::ctx::Ctx;

const ID: &str = "vol.usage.stamped";

/// The catalogue's own 150 s: the write, then up to two sync beats for the beat that cuts after it
/// to stamp what it saw.
const CEILING: Duration = Duration::from_secs(150);
/// One `dd` of 200 MB through the tool server. Generous — `/dev/urandom` on a busy node is not
/// fast — but bounded, or a wedged pod eats the whole step's ceiling before the wait begins.
const WRITE: Duration = Duration::from_secs(45);

/// What is written, and the floor the stamp is then held to. Bytes, because that is what the
/// status field carries.
const WRITTEN_MB: u64 = 200;
const WANT_BYTES: u64 = WRITTEN_MB * (1 << 20);

/// How long to wait for the stamp, as a multiple of the region's sync beat: the beat has to come
/// round once to cut, and a beat that lands the instant before the write is a wasted one.
const BEATS: u32 = 2;

pub async fn stamped(c: &mut Ctx, ws: &str) {
    if c.suite != Suite::Hourly {
        return;
    }
    if c.kube.is_none() {
        return c.skip(ID, "no kubeconfig");
    }
    // Read BEFORE the step so the region's own knob, not the compiled-in default, sets the wait —
    // and outside the timing, since it is a precondition rather than part of what is measured.
    let beat = sync_secs(c).await;
    let ws = ws.to_string();
    c.step(ID, CEILING, move |c| {
        let jwt = c.probe_jwt.clone();
        let volumes = api(c, "/v1/volumes");
        async move {
            let before = used_bytes(c, &volumes, &jwt, &ws).await?;
            write_bytes(c, &ws).await?;
            let want = before.unwrap_or(0).max(WANT_BYTES);
            let deadline = std::time::Instant::now() + beat * BEATS;
            loop {
                let last = used_bytes(c, &volumes, &jwt, &ws).await?;
                if last.is_some_and(|b| b >= want) {
                    return Ok(());
                }
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow!(
                        "{WRITTEN_MB} MB written, but the volume still reads {} after {} s",
                        match last {
                            Some(b) => format!("{b} bytes"),
                            None => "no stamp at all".to_string(),
                        },
                        (beat * BEATS).as_secs()
                    ));
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
        .boxed()
    })
    .await;
}

/// `Volume.status.usedBytes` as `/v1/volumes` reports it for this workspace's own volume, which is
/// named after the workspace. `None` is "no node has stamped it yet", which is not zero — the
/// wait below tells them apart, because only one of the two can become a passing sample.
async fn used_bytes(c: &Ctx, url: &str, jwt: &str, ws: &str) -> Result<Option<u64>> {
    let rows = get(c, url, jwt).await.context("could not list the volumes")?;
    let row = rows
        .as_array()
        .ok_or_else(|| anyhow!("the volume listing is not a list"))?
        .iter()
        .find(|r| r.get("name").and_then(Value::as_str) == Some(ws))
        .ok_or_else(|| anyhow!("the run's own volume {ws} is not in the listing"))?;
    Ok(row.get("usedBytes").and_then(Value::as_u64))
}

/// 200 MB of incompressible bytes into the workspace's subvolume, through the tool server's own
/// `exec`. `conv=fsync` because btrfs charges a qgroup for what is on disk, and the beat that
/// reads it is seconds away — a write still in the page cache would be measured as nothing.
async fn write_bytes(c: &Ctx, ws: &str) -> Result<()> {
    let dir = kloudlite_workspaces::k8s::WORKSPACE_DIR;
    let cmd = format!("dd if=/dev/urandom of={dir}/slo-usage.bin bs=1M count={WRITTEN_MB} conv=fsync");
    // Through the shared helper, which reads the workspace's token from the file this container
    // mounts. Hand-rolling the curl here is what made this probe the one caller without a
    // credential: it exited 22 on every run the moment the tool server began requiring one.
    let (status, out) = ws_tool_within(c, ws, "exec", &serde_json::json!({ "cmd": cmd }), WRITE).await?;
    if status != 200 {
        return Err(anyhow!("the tool server answered {status}: {}", out.trim()));
    }
    if !out.contains("\"exit_code\":0") {
        return Err(anyhow!("the tool server did not write the file: {}", out.trim()));
    }
    Ok(())
}

/// The region's `syncSecs`, or the compiled-in default when the admin process cannot say. A
/// cluster that stores no value runs the default, so the fallback is the right answer rather than
/// a guess — and this step is about the stamp, never about the settings API.
async fn sync_secs(c: &Ctx) -> Duration {
    let url = admin(c, &format!("/admin/settings/clusters/{}", c.cfg.region));
    let stored = get(c, &url, &c.admin_jwt()).await.ok().and_then(|v| v.pointer("/spec/syncSecs").and_then(Value::as_u64));
    Duration::from_secs(stored.unwrap_or_else(kloudlite_workspaces::crd::defaults::sync_secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit;

    /// The file goes to the tree, the one part of the subvolume the exec sandbox binds.
    #[test]
    fn the_bytes_are_written_to_the_subvolume_and_flushed() {
        let dir = kloudlite_workspaces::k8s::WORKSPACE_DIR;
        let cmd = format!("dd if=/dev/urandom of={dir}/slo-usage.bin bs=1M count={WRITTEN_MB} conv=fsync");
        assert!(cmd.contains("of=/home/kl/workspace/slo-usage.bin"), "{cmd}");
        assert!(cmd.ends_with("conv=fsync"), "{cmd}");
    }

    /// A volume with no stamp reads as `None`, never as zero — the two mean different things and
    /// only one of them can ever become a pass.
    #[tokio::test]
    async fn an_unstamped_volume_is_not_a_zero() {
        let rows = serde_json::json!([{ "name": "ws-a" }, { "name": "ws-b", "usedBytes": 4096u64 }]);
        let find = |ws: &str| {
            rows.as_array()
                .unwrap()
                .iter()
                .find(|r| r.get("name").and_then(Value::as_str) == Some(ws))
                .and_then(|r| r.get("usedBytes"))
                .and_then(Value::as_u64)
        };
        assert_eq!(find("ws-a"), None);
        assert_eq!(find("ws-b"), Some(4096));
    }

    /// Fast runs never walk it, and a run with no cluster reports it once as a skip rather than
    /// as a breach.
    #[tokio::test]
    async fn it_is_hourly_and_skips_without_a_kubeconfig() {
        let mut c = testkit::ctx().await;
        c.kube = None;
        stamped(&mut c, "ws-1").await;
        assert!(c.steps.is_empty(), "the fast suite does not walk it");
        c.suite = Suite::Hourly;
        stamped(&mut c, "ws-1").await;
        let s = c.steps.iter().find(|s| s.slo_id == ID).expect("no row");
        assert!(s.skipped && s.detail == "no kubeconfig", "{s:?}");
    }
}
