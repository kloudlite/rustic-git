//! A nudge, never the record. Publishing to `events` tells the merge worker "something changed,
//! go look" — it never carries the authoritative state of what changed. Redis can drop the
//! stream, evict it, or simply be absent (`Cache::connect(None)`), and every consumer must keep
//! working: it falls back to scanning Mongo for pending work on its own schedule, the way the
//! worker already does today. `publish` is fire-and-forget for exactly this reason — a failed
//! XADD costs a consumer one fallback poll cycle, never a lost event.
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

pub fn fields(e: &Event) -> Vec<(String, String)> {
    vec![
        ("kind".to_string(), e.kind.as_str().to_string()),
        ("repo".to_string(), e.repo.clone()),
        ("number".to_string(), e.number.to_string()),
        ("actor".to_string(), e.actor.clone()),
        ("at_ms".to_string(), e.at_ms.to_string()),
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
        };
        assert_eq!(from_fields(&fields(&e)).unwrap().number, 7);
        assert_eq!(from_fields(&fields(&e)).unwrap().kind.as_str(), "pull_opened");
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
        };
        publish(&c, &e).await; // must not panic
    }
}
