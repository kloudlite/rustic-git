//! Write-throughput measurements for the KV store (SlateDB), which is what bounds pushes per node.
//!
//! Ignored by default — they need a real object store and take minutes.
//!   cargo test --release --test throughput -- --ignored --nocapture
//! Set RUSTIC_GIT_S3_URL (plus AWS_*) to measure against a real bucket; unset for in-memory,
//! which isolates SlateDB's own cost from network latency.
mod common;

use gix_hash::ObjectId;
use rustic_git_gitbase::refs::{RefUpdate, UpdateRefsExt};
use rustic_git_storage::store::Store;
use std::sync::Arc;
use std::time::Instant;

async fn store() -> (Arc<Store>, Option<tempfile::TempDir>) {
    match std::env::var("RUSTIC_GIT_S3_URL") {
        Ok(url) if url.starts_with("az://") => {
            let tmp = tempfile::tempdir().unwrap();
            let s = Store::open(
                slatedb::Db::resolve_object_store(&url).unwrap(),
                tmp.path().join("cache"),
                false,
            )
            .await
            .unwrap();
            println!("  backend: {url}");
            (Arc::new(s), Some(tmp))
        }
        Ok(url) if url.starts_with("s3://") => {
            use slatedb::object_store::aws::AmazonS3Builder;
            let bucket = url.strip_prefix("s3://").unwrap();
            let mut b = AmazonS3Builder::from_env().with_bucket_name(bucket);
            if let Ok(ep) = std::env::var("AWS_ENDPOINT") {
                b = b.with_endpoint(ep).with_virtual_hosted_style_request(false);
            }
            let tmp = tempfile::tempdir().unwrap();
            let s = Store::open(
                Arc::new(b.build().unwrap()),
                tmp.path().join("cache"),
                false,
            )
            .await
            .unwrap();
            println!("  backend: {url}");
            (Arc::new(s), Some(tmp))
        }
        _ => {
            let e = common::env().await;
            println!("  backend: in-memory");
            (e.store.clone(), Some(e._tmp))
        }
    }
}

fn oid(i: usize) -> ObjectId {
    ObjectId::from_hex(format!("{i:040x}").as_bytes()).unwrap()
}

/// One ref update = one serializable transaction = what a push pays in the KV store.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn ref_update_throughput() {
    let (s, _t) = store().await;
    let repo_name = format!("bench{}", std::process::id());
    s.create_repo("bench", &repo_name).await.unwrap();
    let repo = Arc::new(s.open_repo("bench", &repo_name).await.unwrap().unwrap());

    for &conc in &[1usize, 8, 32, 128, 512] {
        let n = if conc == 1 { 20 } else { conc * 8 };
        let start = Instant::now();
        let mut tasks = Vec::new();
        for i in 0..n {
            let (s, repo) = (s.clone(), repo.clone());
            let branch = format!("refs/heads/c{conc}-{i}");
            tasks.push(tokio::spawn(async move {
                let t0 = Instant::now();
                s.update_refs(
                    &repo,
                    &[RefUpdate {
                        name: branch,
                        old: None,
                        new: Some(oid(i)),
                    }],
                )
                .await
                .unwrap();
                t0.elapsed()
            }));
            if tasks.len() >= conc {
                for t in tasks.drain(..) {
                    t.await.unwrap();
                }
            }
        }
        let mut lat: Vec<_> = Vec::new();
        for t in tasks {
            lat.push(t.await.unwrap());
        }
        let elapsed = start.elapsed();
        let mut all = lat;
        all.sort();
        let p50 = all.get(all.len() / 2).copied().unwrap_or_default();
        println!(
            "  ref updates: concurrency {conc:>3} -> {:>7.1} ops/s   ({n} ops in {:.2}s, batch p50 {:?})",
            n as f64 / elapsed.as_secs_f64(),
            elapsed.as_secs_f64(),
            p50
        );
    }
    s.delete_repo("bench", &repo_name).await.ok();
}

/// Does the ref store stay fast as repo count grows? Refs for every repo in a shard share one
/// database, so the operation that matters is a prefix scan for one repo among many.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn scales_with_repo_count() {
    let (s, _t) = store().await;
    let db = s.pool.get("bench", "bench").await.unwrap();
    let refs_per_repo = 10;
    let mut total = 0usize;

    for &repos in &[1_000usize, 10_000, 50_000, 100_000] {
        // fill up to `repos` repositories, 10 refs each
        while total < repos {
            let mut wb = slatedb::WriteBatch::new();
            for _ in 0..500 {
                if total >= repos {
                    break;
                }
                for r in 0..refs_per_repo {
                    wb.put(
                        format!("ref/org{}/repo{}/refs/heads/b{}", total % 100, total, r),
                        b"1111111111111111111111111111111111111111",
                    );
                }
                total += 1;
            }
            db.write(wb).await.unwrap();
        }
        db.flush().await.unwrap();

        // scan one repo's refs, from the middle of the keyspace
        let target = format!("ref/org{}/repo{}/", (repos / 2) % 100, repos / 2);
        let mut worst = std::time::Duration::ZERO;
        let mut found = 0;
        for _ in 0..20 {
            let t0 = Instant::now();
            let mut it = db.scan_prefix(target.as_bytes(), ..).await.unwrap();
            found = 0;
            while let Some(_kv) = it.next().await.unwrap() {
                found += 1;
            }
            worst = worst.max(t0.elapsed());
        }
        // and a point lookup
        let t0 = Instant::now();
        let _ = db.get(format!("{target}refs/heads/b0")).await.unwrap();
        println!(
            "  {repos:>7} repos ({:>8} keys): list_refs worst {:>8.2?} ({found} refs), point get {:>8.2?}",
            repos * refs_per_repo,
            worst,
            t0.elapsed()
        );
    }
}

/// What does opening a database cost? This decides whether a database-per-repo, opened on demand,
/// is viable — every cold request would pay this.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn database_open_cost() {
    use slatedb::Db;
    let (s, _t) = store().await;
    let os = s.os.clone();
    let tag = std::process::id();

    // cold: a database that does not exist yet
    let mut first = Vec::new();
    for i in 0..5 {
        let t0 = Instant::now();
        let db = Db::builder(format!("opencost/{tag}/new{i}"), os.clone())
            .build()
            .await
            .unwrap();
        first.push(t0.elapsed());
        db.put(b"k", b"v").await.unwrap();
        db.close().await.unwrap();
    }

    // warm: reopening a database that already exists and holds data
    let path = format!("opencost/{tag}/existing");
    {
        let db = Db::builder(path.clone(), os.clone()).build().await.unwrap();
        for i in 0..100 {
            db.put(format!("ref/x/y/refs/heads/b{i}"), b"deadbeef").await.unwrap();
        }
        db.close().await.unwrap();
    }
    let mut reopen = Vec::new();
    let mut close = Vec::new();
    for _ in 0..5 {
        let t0 = Instant::now();
        let db = Db::builder(path.clone(), os.clone()).build().await.unwrap();
        reopen.push(t0.elapsed());
        let t1 = Instant::now();
        db.close().await.unwrap();
        close.push(t1.elapsed());
    }
    let avg = |v: &Vec<std::time::Duration>| {
        v.iter().sum::<std::time::Duration>() / v.len() as u32
    };
    println!("  open (new database)      : avg {:?}  (min {:?}, max {:?})",
        avg(&first), first.iter().min().unwrap(), first.iter().max().unwrap());
    println!("  open (existing, 100 refs): avg {:?}  (min {:?}, max {:?})",
        avg(&reopen), reopen.iter().min().unwrap(), reopen.iter().max().unwrap());
    println!("  close                    : avg {:?}", avg(&close));
}
