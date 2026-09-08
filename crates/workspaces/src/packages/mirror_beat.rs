//! Keeps `index/pkgs/versions.json` (the mirror `resolve::Mirror` reads) fresh from
//! nixpkgs-multiverse's own published index, once a day.
//!
//! The upstream index names each release by a REVISION NUMBER — a 1-based index into its own
//! `revisions.json`, not a nixpkgs commit — because that is the compact form it ships. A mirror
//! `Lock` needs the actual git sha (`resolve::Mirror::resolve` evaluates `Lock.rev` straight
//! against `github:NixOS/nixpkgs/{rev}`), so this beat fetches both files and joins them before
//! normalising, keeping `normalise` itself pure and testable from one fixture.

use crate::api::ApiState;
use crate::packages::resolve::MIRROR_KEY;
use slatedb::object_store::{path::Path as OsPath, ObjectStoreExt, PutPayload};
use std::sync::Arc;
use std::time::Duration;

const VERSIONS_URL: &str =
    "https://raw.githubusercontent.com/fzakaria/nixpkgs-multiverse/main/index/versions.json";
const REVISIONS_URL: &str = "https://raw.githubusercontent.com/fzakaria/nixpkgs-multiverse/main/revisions.json";
const REFRESH_SECS: u64 = 24 * 3600;
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// `raw` is the two upstream files joined as `{"attrs": versions.json's "attrs", "revisions":
/// revisions.json}`. Produces the exact shape `resolve::MirrorIndex` deserialises: attr ->
/// `[{version, rev, attr_path}]`, newest revision first. A version whose index is `null` (that
/// release's build never finished) is dropped, and an attribute left with no rows is omitted
/// entirely rather than written as an empty list.
pub fn normalise(raw: &serde_json::Value) -> serde_json::Value {
    let revisions = raw["revisions"].as_array().cloned().unwrap_or_default();
    let rev_at = |idx: u64| -> Option<String> {
        // The index is 1-based; `revisions[idx - 1]` is the sha it names.
        revisions.get((idx as usize).checked_sub(1)?)?["rev"].as_str().map(str::to_string)
    };
    let mut out = serde_json::Map::new();
    let Some(attrs) = raw["attrs"].as_object() else { return serde_json::Value::Object(out) };
    for (attr, versions) in attrs {
        let Some(versions) = versions.as_object() else { continue };
        let mut rows: Vec<(u64, serde_json::Value)> = versions
            .iter()
            .filter_map(|(version, idx)| {
                let idx = idx.as_u64()?;
                let rev = rev_at(idx)?;
                Some((idx, serde_json::json!({ "version": version, "rev": rev, "attr_path": attr })))
            })
            .collect();
        if rows.is_empty() {
            continue;
        }
        rows.sort_by_key(|(idx, _)| std::cmp::Reverse(*idx));
        out.insert(attr.clone(), serde_json::Value::Array(rows.into_iter().map(|(_, r)| r).collect()));
    }
    serde_json::Value::Object(out)
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<serde_json::Value, String> {
    client
        .get(url)
        .timeout(HTTP_TIMEOUT)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

async fn refresh(s: &ApiState) -> Result<(), String> {
    // Same object store `resolve::Resolver::from_env` hands the `Mirror` reader (`ApiState::keys`'
    // `.os`) — `None` means no store is configured (dev), so there is nothing to refresh.
    let Some(store) = s.keys.as_ref() else { return Err("no object store configured".into()) };
    let client = reqwest::Client::new();
    let versions = fetch(&client, VERSIONS_URL).await?;
    let revisions = fetch(&client, REVISIONS_URL).await?;
    let raw = serde_json::json!({ "attrs": versions["attrs"], "revisions": revisions });
    let normalised = normalise(&raw);
    let bytes = serde_json::to_vec(&normalised).map_err(|e| e.to_string())?;
    store
        .os
        .put(&OsPath::from(MIRROR_KEY), PutPayload::from(bytes))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Every 24h, first tick immediate. A fetch/parse/write failure logs `packages.mirror.failed` and
/// keeps whatever `index/pkgs/versions.json` already holds — a stale mirror only costs a lock a
/// full nixpkgs evaluation on an older revision, never a wrong one, so there is nothing here worth
/// retrying faster than the next beat.
pub async fn run_beat(s: Arc<ApiState>) {
    let mut tick = tokio::time::interval(Duration::from_secs(REFRESH_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = refresh(&s).await {
            tracing::warn!(error = %e, "packages.mirror.failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::resolve::MirrorIndex;

    // A genuine 3-attribute excerpt of `versions.json` (`python3`, `AgdaStdlib`,
    // `jetbrains.datagrip`), fetched 2026-09-08. `python3`'s `3.14.7` is `null` (its build never
    // finished); `jetbrains.datagrip` has no releases recorded at all.
    const VERSIONS_EXCERPT: &str = r#"{
        "attrs": {
            "python3": {"3.2.3": 7, "3.3.1": 9, "3.14.7": null},
            "AgdaStdlib": {"0.12": 39},
            "jetbrains.datagrip": {}
        }
    }"#;

    // `revisions.json`'s first 39 rows, same fetch — the only ones the excerpt above indexes into.
    const REVISIONS_EXCERPT: &str = r#"[
{"rev":"1ae3ea8385e60f964c322bd317ebfd55d3597b68","date":"2012-07-05","name":"nixos-0.1pre3784_3486916-1ae3ea8","narHash":"sha256-WRTgeV8aGM4+Ztxpsp5js/lFDm0OACpCJwbLlpJfJ20="},
{"rev":"1480737d0f9daca02cb790ed8586111e7bc5f7eb","date":"2012-08-01","name":"nixos-0.1pre3857_52fd5ea-1480737","narHash":"sha256-ZNVGwrWB0c3CusWHWb1p0Idk4ui3ym3Hppq+0Fw5WZ0="},
{"rev":"36619822a1545d4eff59b8de9670ba7821d64069","date":"2012-09-01","name":"nixos-0.1pre3914_bce1cdd-3661982","narHash":"sha256-YeKcH9vYbjZ3+N6K8m6jTvmsKeKORe3TGFlePaFFGtM="},
{"rev":"0013b8faa5a0824212ff1f107a089ed74ee915ad","date":"2012-10-04","name":"nixos-0.1pre3949_4b78161-0013b8f","narHash":"sha256-a6LS5+FZJQs++nW/bEu3jV6HhfE+UVzMZBBELJ+VtbE="},
{"rev":"2a58708d7406cf27ae70931527c9e63d6fd53664","date":"2012-11-02","name":"nixos-0.1pre3985_cd372c6-2a58708","narHash":"sha256-1Ggjn/f93YEU/MR4STot2nqE796xIdgbzZ1APAS/XwQ="},
{"rev":"bc9efb67ef29ff6a1cceb8bea2b64b9fd2d19bd7","date":"2012-12-05","name":"nixos-0.1pre4006_7435db4-bc9efb6","narHash":"sha256-8POgDTisYMW2GJI4TNTi1inSYw5RSgkLXbtQenlaXgM="},
{"rev":"495fbceef93ca347d74413cf1a716975dface96d","date":"2013-01-25","name":"nixos-0.2pre4422_28cf26d-495fbce","narHash":"sha256-LvZMyDzabt5pV62HqK9P743HyMEgc+x5C5G3MD5/eG4="},
{"rev":"089fd0a76912b554fa83710442645060706417c9","date":"2013-03-01","name":"nixos-0.2pre4517_5686833-089fd0a","narHash":"sha256-1nhuavNukapulcmXp2lz9yqPskbgpxqPKqMrGFGZpGY="},
{"rev":"98ba667094f3395cd0e4999693541285ed5e0478","date":"2013-04-01","name":"nixos-0.2pre4592_969c577-98ba667","narHash":"sha256-jrNXlWD013/ZwrjTW8CZY+UC/aV0BAbK0TnUbf1iRKs="},
{"rev":"270ba268d7b8f5a1dbc430259f2500bdf6eaed79","date":"2013-06-30","name":"nixos-0.2pre4809_9dcc4c2-270ba26","narHash":"sha256-UTz4j49omwAG0lg886UY4SkCKAgwdEidsxMYd5qIPwU="},
{"rev":"2238a233523b8608dd607fe9d6460e58144d55c0","date":"2013-07-31","name":"nixos-13.07pre4909_b32ef4d-2238a23","narHash":"sha256-nmKKLl/UwaSx7AaHKUkspt7sSVO/xrEMYRzlB0/qY/w="},
{"rev":"1380ca3e57f2e74071475862bed2659c8f03321a","date":"2013-09-01","name":"nixos-13.07pre5009_388f1d4-1380ca3","narHash":"sha256-ee8wRIjVZFnpRDMyiB7EGjDAOFQsIDbKjoGQ3VtpKxA="},
{"rev":"fca11ef5009bf43bcdb54bb414481fb25d72b379","date":"2013-10-01","name":"nixos-13.09pre5070_869be56-fca11ef","narHash":"sha256-81Y1/wuF+kEf7MvcgR7/8WdZ7SiOs26Zllv3zzVpL3w="},
{"rev":"139ff6d52f7f2840ce129124daeb563336ca75ac","date":"2013-10-31","name":"nixos-13.10pre35424.139ff6d","narHash":"sha256-syp9gorPB3r5lYUW++r1VqrbwZuM4bNrP8lbtT/lH+k="},
{"rev":"adfcc2d9531e78bf6a9e3b56e2f4fc873cb3d87b","date":"2016-09-24","name":"nixos-17.03pre92039.adfcc2d","narHash":"sha256-PCJxrU92f5wzQeE1O566Ao7oggVV8FswBZ3ytJ6BNYA="},
{"rev":"09e4b78b48fa9b5da00f44d2c01f0f9f16c3d406","date":"2016-10-13","name":"nixos-17.03pre93673.09e4b78","narHash":"sha256-f7+6AZxlEndYyl8rzQmNhWs8j8iwrUy9cxqeVazV01I="},
{"rev":"210b3b3184b27be8597f320fc9f337d3997dce94","date":"2016-10-22","name":"nixos-17.03pre94121.210b3b3","narHash":"sha256-ZaOROT6YxL60jpGuSSCBxYVWon+FllbL09l/NA4g9A0="},
{"rev":"fa4167c0a13cbe0d97b9c88d91b86845a8c4e740","date":"2016-10-29","name":"nixos-17.03pre94694.fa4167c","narHash":"sha256-61ap1Bo1EtH2T1h0pcli6ieT1S6epbltJic2lasE9bE="},
{"rev":"a24728fe295f28648f1812b3565541f7ef6269f1","date":"2016-11-08","name":"nixos-17.03pre95306.a24728f","narHash":"sha256-+kGmU4MyKsrWV3oHrDWDYeT4ICqZuMM2kINfDeaVEHk="},
{"rev":"b09435ea51caaae1865e667aaa32f7cba4cc4ff2","date":"2016-11-26","name":"nixos-17.03pre96298.b09435e","narHash":"sha256-P/UpwgxWv5w2PVTVuKAXA+rvdiZnsOabBmoD42+mtL8="},
{"rev":"7926e754f86d25c5c68a4f797b1989862a91de3e","date":"2016-12-02","name":"nixos-17.03pre96677.7926e75","narHash":"sha256-FcEI9bUUJ2axN1iO9I//ZR++dtz2IXng2OYdjPpenKQ="},
{"rev":"69bee1b361afdd4e5348c210c9253995c8cb171a","date":"2016-12-04","name":"nixos-17.03pre96717.69bee1b","narHash":"sha256-uPqLLmtv1ptzIjH2XrC7Sr5awno36KkbazH9ken7MVo="},
{"rev":"571cf4f2095744011a5a7629a1ee71ea4d40a7ea","date":"2016-12-05","name":"nixos-17.03pre96799.571cf4f2","narHash":"sha256-OdCPBHWB/BTkrUjB84Lwalxxyd8Xkxx038cvratvGyY="},
{"rev":"1c50bdd928cec055d2ca842e2cf567aba2584efc","date":"2016-12-07","name":"nixos-17.03pre96925.1c50bdd","narHash":"sha256-7wTqap3bQTRwgK5iZruk9HVeABP0VizhlzsvjjoBJbw="},
{"rev":"da70d3da0f11b22eac77756b39b349215e06b2e3","date":"2016-12-26","name":"nixos-17.03pre97748.da70d3d","narHash":"sha256-SIXxYw2Hl6VfK3+GSUyX66FI6Raaht7+WvZGckdADGo="},
{"rev":"5ba7f33e3a4f8ebee9944e5e7f092edf4cb57f3e","date":"2016-12-27","name":"nixos-17.03pre97767.5ba7f33","narHash":"sha256-HrcpbPbV+SST6IULB3ttuYyjaUeKxyBHrgmZ6hXne0I="},
{"rev":"c311871a6d0a3f83a0cec3e6b8804a741b83dcb5","date":"2016-12-27","name":"nixos-17.03pre97792.c311871","narHash":"sha256-rq5F6QAfQ3s2tTPx6sbgKqyEvnHtNZHRaGxmCoLTlSo="},
{"rev":"d15c62a2a0135bb8968f17326ebda271748cb7f6","date":"2016-12-29","name":"nixos-17.03pre97896.d15c62a","narHash":"sha256-siZOpdDqSI7yr6G9R8qWmNOMU1uCZ+z321mmcsLTxlY="},
{"rev":"59dbcefaa74b940f9b7490c034dc4ff885fa2627","date":"2016-12-30","name":"nixos-17.03pre98008.59dbcef","narHash":"sha256-nBGjeBmZwz66K3LthRCoUBFK6W2acsHvdHUvtg9k97g="},
{"rev":"7ee897a3b3aa75dc53bd408ceea5f9e4e98822b2","date":"2017-01-06","name":"nixos-17.03pre98383.7ee897a","narHash":"sha256-k3taJJ64ddDcVrgrcUbLqvdBdYC4BLLCvq1UuHOb5q8="},
{"rev":"f673243aff0bc9ae4d2e96ccd60124ff9fe5b103","date":"2017-01-10","name":"nixos-17.03pre98682.f673243","narHash":"sha256-5gn5ztgRN67MiWKMUUjZiVs9dVym+6AcFGpll6BOigo="},
{"rev":"f1ba2c8d3bbe4f2421a6872d56d34b3bb2a4bf00","date":"2017-01-26","name":"nixos-17.03pre99626.f1ba2c8","narHash":"sha256-g+Qn/fVdh/dsQK52X2JLPsWRVKY92CXJwFCJIZLXCx4="},
{"rev":"f66d7823ece6fa4bf99e56fa4b4cb0ab16522839","date":"2017-01-27","name":"nixos-17.03pre99759.f66d782","narHash":"sha256-hIfS6u/yTxHIJ5E/Eu4dnO76N4JV2z5ZeEuugoK2MC0="},
{"rev":"97bf0637d5bec8d1fe7a9b0b5a220528afbac97c","date":"2017-02-07","name":"nixos-17.03pre100496.97bf063","narHash":"sha256-Ti/cT9WNFKmqplTUzGbIhKxDmnZ/gPTwkJHDjKYcDi0="},
{"rev":"9dc2cb2e84069041eb4876d1eb44f5afbac46491","date":"2017-02-07","name":"nixos-17.03pre100515.9dc2cb2","narHash":"sha256-uEikg3gi1dHO1ISMg4XLmNteyCnjQ2s2iTe8bTpdkwg="},
{"rev":"f7b7d8e7b59f5e13338be76256e8b524eb993959","date":"2017-02-07","name":"nixos-17.03pre100479.f7b7d8e","narHash":"sha256-8O1f9Aty3h9GFXUt61kSPUUAgD7ml1NDBZPBbYGktTo="},
{"rev":"01fef3f7db4cc208faea31f26c9451b326058781","date":"2017-02-08","name":"nixos-17.03pre100522.01fef3f","narHash":"sha256-z0DNWO7psNBU9PvkoEMklFoMzTwuwZmyCLVcw4Z0fWE="},
{"rev":"20eac78ccbc1f8896da75c3596ce23fe649dd46d","date":"2017-02-11","name":"nixos-17.03pre100741.20eac78","narHash":"sha256-OWG0mY1tcy44JbsliGwUZWXkqj1Nct48Ctd1SxDnTjA="},
{"rev":"6651c72df1c4c47d36e67fe59dcf3c50e75c5383","date":"2017-02-11","name":"nixos-17.03pre100821.6651c72","narHash":"sha256-e5KJcnti8qjcX6CxvAkt7MvlE7nNi6Sv5vvtZ9IiC+4="}
]"#;

    fn raw() -> serde_json::Value {
        let versions: serde_json::Value = serde_json::from_str(VERSIONS_EXCERPT).unwrap();
        let revisions: serde_json::Value = serde_json::from_str(REVISIONS_EXCERPT).unwrap();
        serde_json::json!({ "attrs": versions["attrs"], "revisions": revisions })
    }

    #[test]
    fn normalise_produces_the_mirror_index_shape_newest_first_and_drops_unbuilt_and_empty() {
        let out = normalise(&raw());
        let index: MirrorIndex =
            serde_json::from_value(out.clone()).expect("must round-trip through resolve::MirrorIndex");

        assert!(!index.contains_key("jetbrains.datagrip"), "an attribute with no releases must not appear");

        let python3 = &index["python3"];
        assert_eq!(python3.len(), 2, "3.14.7 (index null, unbuilt) must be dropped: {python3:?}");
        assert_eq!(python3[0].version, "3.3.1", "newest revision (index 9) first");
        assert_eq!(python3[0].rev, "98ba667094f3395cd0e4999693541285ed5e0478");
        assert_eq!(python3[0].attr_path, "python3");
        assert_eq!(python3[1].version, "3.2.3");
        assert_eq!(python3[1].rev, "495fbceef93ca347d74413cf1a716975dface96d");

        let agda = &index["AgdaStdlib"];
        assert_eq!(agda.len(), 1);
        assert_eq!(agda[0].version, "0.12");
        assert_eq!(agda[0].rev, "6651c72df1c4c47d36e67fe59dcf3c50e75c5383"); // index 39 -> revisions[38]
        assert_eq!(agda[0].attr_path, "AgdaStdlib");
    }
}
