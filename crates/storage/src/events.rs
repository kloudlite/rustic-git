//! A nudge, never the record. Publishing to `events` tells the merge worker "something changed,
//! go look" — it never carries the authoritative state of what changed. Redis can drop the
//! stream, evict it, or simply be absent (`Cache::connect(None)`), and every consumer must keep
//! working: the worker's nudges are a speed-up over the owning node's own periodic lanes
//! (`check_owned_pulls`, `announce_stranded_merges` in `bins/server/src/lanes.rs`); the activity
//! feed's PR half has no fallback and simply goes quiet. `publish` is fire-and-forget for exactly
//! this reason — a failed XADD costs a consumer one sweep interval, never a lost event.
//!
//! One `events` stream, not one per repo. The merge worker wants a single Redis consumer group
//! so every worker replica competes for entries off ONE stream (`XREADGROUP` on `events`,
//! standard work-queue fan-out). A stream per repo would mean the worker has to discover which
//! stream names currently exist before it can `XREADGROUP` on all of them — exactly the
//! per-repo-polling coupling this design exists to remove. All repos multiplex onto the one
//! stream; `repo` is just a field on each entry, not part of routing.

use crate::cache::Cache;

pub struct Event {
    pub kind: Kind,
    pub repo: String,
    pub number: i64,
    pub actor: String,
    pub at_ms: i64,
    /// PR title/branch names, carried so the feed (Task 4) can render the same `title`/`detail`
    /// it would have built from Mongo, without a second round trip. Empty when the publisher
    /// genuinely has none to give (e.g. `HeadMoved`, which is repo-wide, not PR-scoped) — never
    /// omitted, so `fields`/`from_fields` stay a fixed shape.
    pub title: String,
    pub base: String,
    pub head: String,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Kind {
    PullOpened,
    PullCommented,
    MergeRequested,
    PullMerged,
    PullClosed,
    HeadMoved,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::PullOpened => "pull_opened",
            Kind::PullCommented => "pull_commented",
            Kind::MergeRequested => "merge_requested",
            Kind::PullMerged => "pull_merged",
            Kind::PullClosed => "pull_closed",
            Kind::HeadMoved => "head_moved",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "pull_opened" => Kind::PullOpened,
            "pull_commented" => Kind::PullCommented,
            "merge_requested" => Kind::MergeRequested,
            "pull_merged" => Kind::PullMerged,
            "pull_closed" => Kind::PullClosed,
            "head_moved" => Kind::HeadMoved,
            _ => return None,
        })
    }
}

const STREAM: &str = "events";
const MAXLEN: usize = 5000;

pub fn fields(e: &Event) -> Vec<(&'static str, String)> {
    vec![
        ("kind", e.kind.as_str().to_string()),
        ("repo", e.repo.clone()),
        ("number", e.number.to_string()),
        ("actor", e.actor.clone()),
        ("at_ms", e.at_ms.to_string()),
        ("title", e.title.clone()),
        ("base", e.base.clone()),
        ("head", e.head.clone()),
    ]
}

pub fn from_fields(f: &[(String, String)]) -> Option<Event> {
    let get = |k: &str| f.iter().find(|(fk, _)| fk == k).map(|(_, v)| v.as_str());
    Some(Event {
        kind: Kind::parse(get("kind")?)?, // an unknown/future kind must be skipped, never fatal
        repo: get("repo")?.to_string(),
        number: get("number")?.parse().ok()?,
        actor: get("actor")?.to_string(),
        at_ms: get("at_ms")?.parse().ok()?,
        // Missing on an entry written before this field existed — default empty, never fail the
        // whole parse over it (see the struct doc: these are enrichment, not identity).
        title: get("title").unwrap_or("").to_string(),
        base: get("base").unwrap_or("").to_string(),
        head: get("head").unwrap_or("").to_string(),
    })
}

/// Fire-and-forget: see the module doc. Never propagates an error, so a caller on a hot path
/// (opening a PR, posting a comment) cannot be slowed or failed by a Redis blip.
pub async fn publish(cache: &Cache, e: &Event) {
    cache.xadd(STREAM, MAXLEN, &fields(e)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_round_trip() {
        let e = Event {
            kind: Kind::PullOpened,
            repo: "alice/web".into(),
            number: 7,
            actor: "alice@example.com".into(),
            at_ms: 1755772800000,
            title: "fix the thing".into(),
            base: "main".into(),
            head: "fix-it".into(),
        };
        let f: Vec<(String, String)> =
            fields(&e).into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        assert_eq!(from_fields(&f).unwrap().number, 7);
        assert_eq!(from_fields(&f).unwrap().kind.as_str(), "pull_opened");
        assert_eq!(from_fields(&f).unwrap().base, "main");
        assert_eq!(from_fields(&f).unwrap().head, "fix-it");
    }

    /// An entry written by a producer that predates `title`/`base`/`head` (Task 4's follow-up
    /// fix) must still parse — a missing enrichment field is not a reason to drop the whole
    /// event, only to render it plainer.
    #[test]
    fn an_old_shape_entry_without_branch_fields_still_parses() {
        let f = vec![
            ("kind".to_string(), "pull_merged".to_string()),
            ("repo".to_string(), "alice/web".to_string()),
            ("number".to_string(), "7".to_string()),
            ("actor".to_string(), "alice@example.com".to_string()),
            ("at_ms".to_string(), "1755772800000".to_string()),
        ];
        let e = from_fields(&f).expect("an old-shape entry must still yield a usable Event");
        assert_eq!(e.number, 7);
        assert_eq!(e.title, "");
        assert_eq!(e.base, "");
        assert_eq!(e.head, "");
    }

    #[test]
    fn unknown_kind_is_ignored_not_fatal() {
        let f = vec![
            ("kind".to_string(), "from_the_future".to_string()),
            ("repo".to_string(), "a/b".to_string()),
        ];
        assert!(from_fields(&f).is_none()); // a consumer must skip it, never panic
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn publish_is_a_no_op_on_a_disabled_cache() {
        let c = Cache::connect(None).await;
        let e = Event {
            kind: Kind::HeadMoved,
            repo: "a/b".into(),
            number: 0,
            actor: "x".into(),
            at_ms: 0,
            title: String::new(),
            base: String::new(),
            head: String::new(),
        };
        publish(&c, &e).await; // must not panic
    }
}
