//! The derived views, timed: the whole-tree file list and the per-directory
//! last-change walk. These are the expensive ones — the cheap listing is already
//! sub-millisecond — so this is what decides whether the work belongs on a server
//! or in the browser.
use std::time::Instant;

fn main() -> rustic_git::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let repo = a.get(1).cloned().unwrap_or_else(|| ".".into());
    let oid: gix_hash::ObjectId = a.get(2).expect("<commit>").parse().unwrap();
    let odb = gix_odb::at(format!("{repo}/.git/objects"))?;

    // Warm the pack index: the first read of a big repo pays to mmap and load it,
    // which is a one-off per process, not a per-request cost.
    let _ = rustic_git::browse::files_at(&odb, oid, "", 5000)?;

    let t = Instant::now();
    let files = rustic_git::browse::files_at(&odb, oid, "", 5000)?;
    println!("files_at (whole tree, cap 5000): {: >8.1?}  {} files", t.elapsed(), files.len());
    let sized = files.iter().filter(|f| f.size.is_some()).count();
    println!("  of which have a size:          {sized}");

    for budget in [100usize, 500, 2000] {
        let _ = rustic_git::browse::last_changes(&odb, oid, "", budget)?;
        let t = Instant::now();
        let out = rustic_git::browse::last_changes(&odb, oid, "", budget)?;
        println!("last_changes(root, budget {budget: >4}): {: >8.1?}  {} of the entries attributed", t.elapsed(), out.len());
    }

    let _ = rustic_git::browse::log(&odb, oid, 50)?;
    let t = Instant::now();
    let log = rustic_git::browse::log(&odb, oid, 50)?;
    println!("log (50 commits):                {: >8.1?}  {} commits", t.elapsed(), log.len());
    Ok(())
}
