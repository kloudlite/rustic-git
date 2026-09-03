//! Deterministic name derivations. `/v1` and the agent must both compute these identically —
//! `/v1` writes the name a controller then has to find again — so nothing here may read state,
//! only its arguments. Every one of them has had a bug where two distinct pairs collided
//! (`ws_namespace`'s team/owner join, `binding_name`'s region/owner join), which is why each is
//! documented with the exact collision it closes rather than trusted to "obviously" be unique.

/// The RFC-1123 object name for an owner's node binding: `{region}-{owner}` plus a hash tail
/// over the PAIR. Region ids and handles both allow `-`, so the bare join was ambiguous —
/// `centralindia-x` + `att` and `centralindia` + `x-att` — and the tail is what tells them apart.
pub fn binding_name(region: &str, owner: &str) -> String {
    let (region, owner) = (region.to_lowercase(), owner.to_lowercase());
    dns_label(&format!("{region}-{owner}-{}", pair_tail(&region, &owner)))
}

/// Twelve hex characters of sha256 over `"{a}/{b}"`. `/` is the separator because no handle,
/// team slug or region id can contain it, which is what makes the pre-image — and so the tail —
/// distinct for distinct pairs.
fn pair_tail(a: &str, b: &str) -> String {
    hex_prefix(&format!("{a}/{b}"), 6)
}

fn hex_prefix(raw: &str, bytes: usize) -> String {
    use sha2::Digest;
    hex::encode(&sha2::Sha256::digest(raw.as_bytes())[..bytes])
}

/// The namespace ALL of an owner's workspace pods live in — one per user, not one per workspace.
///
/// Shared on purpose: it keeps the object count proportional to users rather than to workspaces,
/// and it gives a per-user `ResourceQuota` somewhere to live, which is the unit a limit is
/// naturally expressed in ("this user gets N CPUs across everything they run").
///
/// Two consequences follow and are handled where they arise, not here: the namespace must carry NO
/// `ownerReference` (deleting one workspace would otherwise garbage-collect the namespace and every
/// sibling in it), and an attachment must select the individual workspace's POD rather than the
/// whole namespace.
///
/// Personal is `ws-{owner}`; a team pair is `wt-{owner}-{tail}`, the tail hashed over
/// `(team, owner)`. Not `ws-{team}-{owner}`: handles and team slugs both allow `-`, so team
/// `acme` with owner `bob` and the personal namespace of handle `acme-bob` were ONE namespace, and the
/// fixed-name `user-key` Secret in it — the owner's private git key — was shared between two
/// people. A distinct prefix keeps team namespaces out of the personal keyspace entirely, and the
/// tail keeps two pairs apart without a separator a handle could forge. The longest case is
/// `wt-` + 39 + `-` + 12 = 55 characters, so a team name never reaches `dns_label`'s truncation.
pub fn ws_namespace(owner: &str, team: &str) -> String {
    let owner = owner.to_lowercase();
    if team.is_empty() || team.eq_ignore_ascii_case(&owner) {
        return dns_label(&format!("ws-{owner}"));
    }
    dns_label(&format!("wt-{owner}-{}", pair_tail(&team.to_lowercase(), &owner)))
}

/// A namespace name is an RFC 1123 label: 63 characters at most. Two 39-character handles and
/// the prefix can reach 82, so a long pair is cut and given a hash tail — the tail is what keeps
/// two pairs that share a prefix apart. Deterministic, so the controller and the API agree.
fn dns_label(raw: &str) -> String {
    if raw.len() <= 63 {
        return raw.to_string();
    }
    let tail = hex_prefix(raw, 4);
    let head = raw[..63 - tail.len() - 1].trim_end_matches('-');
    format!("{head}-{tail}")
}

/// The namespace an environment's deployments and services live in. One namespace per environment
/// is what makes a default-deny NetworkPolicy the isolation boundary.
///
/// Idempotent, because environment ids are already minted as `env-{hex}` (`api::rid("env")`) and
/// prefixing unconditionally produced `env-env-{hex}` — valid, and wrong every time anyone read it.
/// Written this way rather than by dropping the prefix so an id whose shape changes still lands in
/// a namespace that says what it is.
pub fn env_namespace(id: &str) -> String {
    let id = id.to_lowercase();
    format!("env-{}", id.strip_prefix("env-").unwrap_or(&id))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `hex` is already a workspace dependency; a hand-rolled `format!("{b:02x}")` fold is the same
    /// bytes with more places to get it wrong. The tail must not move — it is in stored object names.
    #[test]
    fn the_namespace_tail_is_unchanged_by_the_hex_swap() {
        assert_eq!(ws_namespace("bob", "acme"), "wt-bob-2e737765961a");
    }
}
