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

/// What does one SlateDB instance cost? This decides whether a database-per-repo (a writer per
/// repo, dynamically assignable) is viable, or whether writers must be pooled into shards.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn database_instance_cost() {
    use slatedb::Db;
    let os: Arc<dyn slatedb::object_store::ObjectStore> = match std::env::var("RUSTIC_GIT_S3_URL") {
        Ok(url) if url.starts_with("s3://") => {
            use slatedb::object_store::aws::AmazonS3Builder;
            let mut b =
                AmazonS3Builder::from_env().with_bucket_name(url.strip_prefix("s3://").unwrap());
            if let Ok(ep) = std::env::var("AWS_ENDPOINT") {
                b = b.with_endpoint(ep).with_virtual_hosted_style_request(false);
            }
            println!("  backend: {url}");
            Arc::new(b.build().unwrap())
        }
        _ => {
            println!("  backend: in-memory");
            Arc::new(slatedb::object_store::memory::InMemory::new())
        }
    };
    let tag = std::process::id();
    let mut dbs = Vec::new();
    for &n in &[1usize, 10, 25, 50] {
        let start = Instant::now();
        while dbs.len() < n {
            let i = dbs.len();
            dbs.push(
                Db::builder(format!("dbcost/{tag}/db{i}"), os.clone())
                    .build()
                    .await
                    .unwrap(),
            );
        }
        let opened = start.elapsed();
        // one small write to each, to include steady-state cost
        let t0 = Instant::now();
        for (i, db) in dbs.iter().enumerate() {
            db.put(format!("k{i}"), b"v").await.unwrap();
        }
        println!(
            "  {n:>3} databases open: +{:.2}s to reach this count, {:.1}ms per serial write across all",
            opened.as_secs_f64(),
            t0.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
    }
    for db in dbs {
        db.close().await.ok();
    }
}

/// Where does the ref-update ceiling actually come from? Compares the serializable transaction
/// (what update_refs uses) against a plain put and a batched put, so we can tell whether the limit
/// is the KV store or the transaction machinery on top of it.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn write_path_comparison() {
    let (s, _t) = store().await;
    let db = s.pool.get("bench", "bench").await.unwrap();
    let conc = 512usize;
    let n = 4096usize;

    // 1. plain put, concurrent
    let start = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..n {
        let db = db.clone();
        tasks.push(tokio::spawn(async move {
            db.put(format!("plain/{i}"), b"v").await.unwrap();
        }));
        if tasks.len() >= conc {
            for t in tasks.drain(..) {
                t.await.unwrap();
            }
        }
    }
    for t in tasks {
        t.await.unwrap();
    }
    println!(
        "  plain put            : {:>8.0} ops/s",
        n as f64 / start.elapsed().as_secs_f64()
    );

    // 2. batched puts: many refs per commit
    for batch in [10usize, 100] {
        let start = Instant::now();
        let mut tasks = Vec::new();
        for c in 0..(n / batch) {
            let db = db.clone();
            tasks.push(tokio::spawn(async move {
                let mut wb = slatedb::WriteBatch::new();
                for i in 0..batch {
                    wb.put(format!("batch{batch}/{c}/{i}"), b"v");
                }
                db.write(wb).await.unwrap();
            }));
            if tasks.len() >= conc {
                for t in tasks.drain(..) {
                    t.await.unwrap();
                }
            }
        }
        for t in tasks {
            t.await.unwrap();
        }
        println!(
            "  batched put ({batch:>3}/txn): {:>8.0} ops/s",
            n as f64 / start.elapsed().as_secs_f64()
        );
    }

    // 3. serializable transaction (what update_refs does today)
    let start = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..n {
        let db = db.clone();
        tasks.push(tokio::spawn(async move {
            let txn = db
                .begin(slatedb::IsolationLevel::SerializableSnapshot)
                .await
                .unwrap();
            let _ = txn.get(format!("txn/{i}")).await.unwrap();
            txn.put(format!("txn/{i}"), b"v").unwrap();
            txn.commit().await.unwrap();
        }));
        if tasks.len() >= conc {
            for t in tasks.drain(..) {
                t.await.unwrap();
            }
        }
    }
    for t in tasks {
        t.await.unwrap();
    }
    println!(
        "  serializable txn     : {:>8.0} ops/s",
        n as f64 / start.elapsed().as_secs_f64()
    );
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

/// Where does a cold open's wall clock actually go? Wraps the object store and prints every
/// request it makes, in order, with its duration — so the 1.7s is attributed rather than guessed.
mod trace {
    use slatedb::object_store::{path::Path, *};
    use futures::stream::BoxStream;
    use std::ops::Range;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    #[derive(Debug)]
    pub struct Tracing {
        pub inner: Arc<dyn ObjectStore>,
        pub log: Arc<Mutex<Vec<(String, String, std::time::Duration)>>>,
    }

    impl std::fmt::Display for Tracing {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "tracing({})", self.inner)
        }
    }

    impl Tracing {
        fn rec<T>(&self, op: &str, path: String, t: Instant, r: T) -> T {
            self.log.lock().unwrap().push((op.into(), path, t.elapsed()));
            r
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for Tracing {
        async fn put_opts(&self, l: &Path, p: PutPayload, o: PutOptions) -> Result<PutResult> {
            let t = Instant::now();
            let r = self.inner.put_opts(l, p, o).await;
            self.rec("PUT", l.to_string(), t, r)
        }
        async fn put_multipart_opts(&self, l: &Path, o: PutMultipartOptions) -> Result<Box<dyn MultipartUpload>> {
            let t = Instant::now();
            let r = self.inner.put_multipart_opts(l, o).await;
            self.rec("PUT-MP", l.to_string(), t, r)
        }
        async fn get_opts(&self, l: &Path, o: GetOptions) -> Result<GetResult> {
            let t = Instant::now();
            let r = self.inner.get_opts(l, o).await;
            self.rec("GET", l.to_string(), t, r)
        }
        fn delete_stream(
            &self,
            locations: BoxStream<'static, Result<Path>>,
        ) -> BoxStream<'static, Result<Path>> {
            self.inner.delete_stream(locations)
        }
        fn list(&self, p: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
            self.log.lock().unwrap().push((
                "LIST".into(),
                p.map(|x| x.to_string()).unwrap_or_default(),
                std::time::Duration::ZERO, // streamed; consumed by the caller, not here
            ));
            self.inner.list(p)
        }
        async fn list_with_delimiter(&self, p: Option<&Path>) -> Result<ListResult> {
            let t = Instant::now();
            let r = self.inner.list_with_delimiter(p).await;
            self.rec("LIST-D", p.map(|x| x.to_string()).unwrap_or_default(), t, r)
        }
        async fn copy_opts(&self, f: &Path, to: &Path, o: CopyOptions) -> Result<()> {
            let t = Instant::now();
            let r = self.inner.copy_opts(f, to, o).await;
            self.rec("COPY", format!("{f} -> {to}"), t, r)
        }
        async fn get_ranges(&self, l: &Path, r: &[Range<u64>]) -> Result<Vec<slatedb::bytes::Bytes>> {
            let t = Instant::now();
            let out = self.inner.get_ranges(l, r).await;
            self.rec("GET-RANGES", l.to_string(), t, out)
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn database_open_trace() {
    use slatedb::Db;
    use std::sync::Mutex;

    let (s, _t) = store().await;
    let tag = std::process::id();
    let path = format!("opentrace/{tag}/repo");

    // seed a realistic repo database, then close it cleanly
    {
        let db = Db::builder(path.clone(), s.os.clone()).build().await.unwrap();
        for i in 0..100 {
            db.put(format!("ref/o/r/refs/heads/b{i}"), b"deadbeef").await.unwrap();
        }
        db.close().await.unwrap();
    }

    let log = Arc::new(Mutex::new(Vec::new()));
    let traced = Arc::new(trace::Tracing {
        inner: s.os.clone(),
        log: log.clone(),
    });

    let t0 = Instant::now();
    let db = Db::builder(path, traced).build().await.unwrap();
    let total = t0.elapsed();
    let entries = log.lock().unwrap().clone(); // snapshot before close() adds its own requests
    db.close().await.ok();

    let mut by_kind: std::collections::BTreeMap<String, (usize, std::time::Duration)> =
        Default::default();
    println!("\n  cold open took {total:?} in {} object-store requests:", entries.len());
    for (op, path, d) in &entries {
        let kind = path.rsplit('/').next().unwrap_or("");
        let kind = if kind.ends_with(".manifest") {
            "manifest"
        } else if kind.ends_with(".compactions") {
            "compactions"
        } else if path.contains("/wal/") {
            "wal"
        } else if path.contains("/gc/") {
            "gc boundary"
        } else if path.contains("/compacted/") {
            "sst"
        } else {
            "other"
        };
        let e = by_kind.entry(format!("{op} {kind}")).or_default();
        e.0 += 1;
        e.1 += *d;
        println!("    {:>10} {:>8.1?}  {}", op, d, path);
    }
    println!("\n  by kind (count, summed latency — requests overlap, so this exceeds wall clock):");
    for (k, (n, d)) in by_kind {
        println!("    {k:<22} {n:>3}  {d:?}");
    }
    println!("  wall clock: {total:?}\n");
}
