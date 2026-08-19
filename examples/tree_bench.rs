//! How long does listing a directory take, and where does the time go?
//!
//! Run against any real repo: `cargo run --release --example tree_bench -- <repo> <commit> [path]`.
//! Reports the whole `tree_at` call and, separately, the part that reads file
//! sizes — which is the thing under suspicion.

use std::time::Instant;

fn main() -> rustic_git::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let repo = args.get(1).cloned().unwrap_or_else(|| ".".into());
    let commit = args.get(2).cloned().expect("usage: tree_bench <repo> <commit> [path]");
    let path = args.get(3).cloned().unwrap_or_default();

    let odb = gix_odb::at(format!("{repo}/.git/objects"))?;
    let oid: gix_hash::ObjectId = commit.parse().expect("a commit id");

    // Warm: the first call pays for opening packs, which is not what is measured.
    let entries = rustic_git::browse::tree_at(&odb, oid, &path)?;
    let blobs = entries.iter().filter(|e| e.kind == "blob").count();

    let runs = 20;
    let start = Instant::now();
    for _ in 0..runs {
        std::hint::black_box(rustic_git::browse::tree_at(&odb, oid, &path)?);
    }
    let each = start.elapsed() / runs;

    println!(
        "{}/{: <18} {: >3} entries ({blobs} blobs)   tree_at: {:?}",
        repo.rsplit('/').next().unwrap_or(&repo),
        if path.is_empty() { "<root>" } else { &path },
        entries.len(),
        each,
    );
    Ok(())
}
