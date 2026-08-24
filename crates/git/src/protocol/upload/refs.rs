use super::super::block_on;
use crate::pktline;
use crate::store::{Repo, Store};
use crate::Result;
use gix_hash::ObjectId;
use std::io::Write;

/// Follow an annotated tag to the object it ultimately names.
///
/// `refs/tags/v1` may point at a tag object, which points at another tag, which
/// points at a commit. What a client wants is the commit at the end of that chain.
pub(super) fn peel_to_object(odb: &gix_odb::Handle, mut id: ObjectId) -> Option<ObjectId> {
    let mut buf = Vec::new();
    let mut peeled = None;
    // Bounded: a tag chain is two or three long in practice, and a cycle in the
    // object database must not hang a ref listing.
    for _ in 0..16 {
        match gix_object::FindExt::find(odb, &id, &mut buf).ok()?.decode().ok()? {
            gix_object::ObjectRef::Tag(t) => {
                id = t.target();
                peeled = Some(id);
            }
            _ => return peeled,
        }
    }
    peeled
}

/// Which branch HEAD names.
///
/// The default branch if it exists, else master, else the first branch there is
/// (like GitHub, for repos whose first push was not to the default). Shared,
/// because `ls-refs` advertises HEAD and `want-ref HEAD` has to resolve to the
/// same thing — two copies of this rule is a clone that fetches a different
/// branch than it was shown.
pub(super) fn head_target(refs: &[(String, ObjectId)]) -> String {
    let default = crate::refs::DEFAULT_BRANCH;
    let has = |b: &str| refs.iter().any(|(n, _)| n == &format!("refs/heads/{b}"));
    if has(default) || !refs.iter().any(|(n, _)| n.starts_with("refs/heads/")) {
        format!("refs/heads/{default}")
    } else if has("master") {
        "refs/heads/master".to_string()
    } else {
        refs.iter()
            .map(|(n, _)| n)
            .find(|n| n.starts_with("refs/heads/"))
            .cloned()
            .unwrap()
    }
}

pub(super) fn ls_refs(store: &Store, repo: &Repo, args: &[String], out: &mut dyn Write) -> Result<()> {
    let symrefs = args.iter().any(|a| a == "symrefs");
    let peel = args.iter().any(|a| a == "peel");
    let unborn = args.iter().any(|a| a == "unborn");
    let prefixes: Vec<&str> = args
        .iter()
        .filter_map(|a| a.strip_prefix("ref-prefix "))
        .collect();
    let want = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p));
    let refs = block_on(store.list_refs(repo))?;
    let head_target = head_target(&refs);
    if want("HEAD") {
        match refs.iter().find(|(n, _)| *n == head_target) {
            Some((_, oid)) => pktline::write_text(
                out,
                &if symrefs {
                    format!("{} HEAD symref-target:{head_target}", oid.to_hex())
                } else {
                    format!("{} HEAD", oid.to_hex())
                },
            )?,
            None => {
                if unborn {
                    pktline::write_text(out, &format!("unborn HEAD symref-target:{head_target}"))?;
                }
            }
        }
    }
    // Only opened when peeling was actually asked for: a listing should not pay
    // to open the object database to answer a question nobody put.
    let odb = if peel { repo.odb().ok() } else { None };
    for (name, oid) in &refs {
        if !want(name) {
            continue;
        }
        let peeled = odb
            .as_ref()
            .filter(|_| name.starts_with("refs/tags/"))
            .and_then(|o| peel_to_object(o, *oid));
        match peeled {
            Some(p) => pktline::write_text(out, &format!("{} {name} peeled:{}", oid.to_hex(), p.to_hex()))?,
            None => pktline::write_text(out, &format!("{} {name}", oid.to_hex()))?,
        }
    }
    pktline::write_flush(out)?;
    Ok(())
}
