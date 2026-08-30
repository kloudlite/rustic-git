use sha2::{Digest, Sha256};

/// Highest-random-weight: score every candidate by `sha256(volume_id, node)` and take the top
/// `total-1` that are not this node. Deterministic across processes on purpose — sha2, never
/// `DefaultHasher`, whose output the std docs do not promise stable across releases.
pub fn targets(volume_id: &str, me: &str, candidates: &[String], total: usize) -> Vec<String> {
    let n = total.saturating_sub(1);
    let mut scored: Vec<(Vec<u8>, &String)> = candidates
        .iter()
        .filter(|c| c.as_str() != me)
        .map(|c| {
            let mut h = Sha256::new();
            h.update(volume_id.as_bytes());
            h.update([0]); // separator: ("ab","c") must not collide with ("a","bc")
            h.update(c.as_bytes());
            (h.finalize().to_vec(), c)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().take(n).map(|(_, c)| c.clone()).collect()
}

/// Topological order over `cloneOf`, sources first. Kahn-style with a stable tie-break so the
/// beat visits volumes in the same order every time. A missing source is a root: its clone still
/// replicates, just without a `-c` to share against.
pub fn order_groups(vols: &[(String, Option<String>)]) -> Vec<String> {
    let ids: std::collections::HashSet<&str> = vols.iter().map(|(id, _)| id.as_str()).collect();
    let mut out = Vec::with_capacity(vols.len());
    let mut placed = std::collections::HashSet::new();
    let mut rest: Vec<&(String, Option<String>)> = vols.iter().collect();
    rest.sort_by(|a, b| a.0.cmp(&b.0));
    while !rest.is_empty() {
        let before = out.len();
        rest.retain(|(id, src)| {
            let ready = match src.as_deref() {
                Some(s) if ids.contains(s) => placed.contains(s),
                _ => true,
            };
            if ready {
                out.push(id.clone());
                placed.insert(id.clone());
            }
            !ready
        });
        if out.len() == before {
            // A cloneOf cycle cannot be built through /v1, but a hand-edited spec must not hang
            // the beat: emit the remainder in name order and let the sends be full.
            out.extend(rest.drain(..).map(|(id, _)| id.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rendezvous, not modulo: every agent computes the same set with no coordinator, and adding
    /// a node moves ~1/N of volumes instead of nearly all — each move is a full btrfs send.
    #[test]
    fn selection_is_deterministic_excludes_me_and_caps_at_total() {
        let nodes: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let t = targets("ws-1", "a", &nodes, 3);
        assert_eq!(t, targets("ws-1", "a", &nodes, 3), "same inputs, same answer, any process");
        assert_eq!(t.len(), 2);
        assert!(!t.contains(&"a".to_string()));
        assert!(targets("ws-1", "a", &nodes, 1).is_empty(), "N=1 is replication off");
        assert_eq!(targets("ws-1", "a", &nodes[..2], 5).len(), 1, "capped by the cluster");
    }

    #[test]
    fn adding_a_node_moves_few_volumes() {
        let four: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        let five: Vec<String> = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
        let moved = (0..1000)
            .filter(|i| {
                let id = format!("ws-{i}");
                targets(&id, "a", &four, 2) != targets(&id, "a", &five, 2)
            })
            .count();
        assert!(moved < 400, "rendezvous keeps most assignments: {moved}/1000 moved");
    }

    /// Ancestor-first is the entire sharing mechanism: a clone sent before its source arrives as
    /// a full copy, and nothing ever repairs that.
    #[test]
    fn groups_order_sources_before_clones() {
        let vols = vec![
            ("ws-clone2".into(), Some("ws-clone1".into())),
            ("ws-root".into(), None),
            ("ws-clone1".into(), Some("ws-root".into())),
            ("ws-lonely".into(), Some("ws-gone".into())), // source not on this node
        ];
        let out = order_groups(&vols);
        let pos = |id: &str| out.iter().position(|v| v == id).unwrap();
        assert!(pos("ws-root") < pos("ws-clone1") && pos("ws-clone1") < pos("ws-clone2"));
        assert_eq!(out.len(), 4, "a clone with an absent source is still sent");
    }
}
