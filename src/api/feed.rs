use super::*;

/// `GET /v1/repos?owner=X`. Members only: a stranger's view of a namespace is a
/// different question (which repos are PUBLIC), and answering it from here would
/// mean this route decided visibility for two audiences at once.
/// GET a browse route from the owning node, for the feed.
///
/// `None` for anything that did not work. One unreachable or empty repo must not
/// empty the whole feed — a glance at what happened is worth having in part.
pub(crate) async fn feed_get(api: &Api, owner: &str, path: String) -> Option<String> {
    let res = api
        .client
        .get(format!("{}{path}", api.upstream))
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .header(crate::proxy::OWNER_HEADER, owner)
        .send()
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    res.text().await.ok()
}

/// The half of the feed that does not depend on Redis at all: the listing markers, one row each.
pub(crate) fn repo_created(r: &RepoOut) -> Event {
    Event {
        kind: "repo_created".into(),
        repo: r.name.clone(),
        actor: r.created_by.clone(),
        title: format!("created {}", r.name),
        detail: if r.public { "public".into() } else { "private".into() },
        at: r.created_at / 1000,
        href: format!("/{}/{}", r.owner, r.name),
    }
}

/// Turns a stream `events::Event` into a feed row, or `None` for kinds the feed does not show
/// (`PullCommented`, `MergeRequested`, `HeadMoved` — noise for a glance-at-it rail). `title`/
/// `detail` are built off the `title`/`base`/`head` the publisher carried on the event, which is
/// now the only source for the PR half of the feed. An event from before that field existed carries them empty
/// (see `events::from_fields`), so this degrades to a plain "opened #7" rather than failing.
pub(crate) fn pull_event(e: events::Event, name: String) -> Option<Event> {
    let (kind, verb, detail) = match e.kind {
        Kind::PullOpened => ("pull_opened", "opened", format!("{} into {}", e.head, e.base)),
        Kind::PullMerged => ("pull_merged", "merged", format!("into {}", e.base)),
        Kind::PullClosed => ("pull_closed", "closed", format!("into {}", e.base)),
        Kind::PullCommented | Kind::MergeRequested | Kind::HeadMoved => return None,
    };
    // `e.repo` is `owner/name`; the route is `[owner]/[repo]/pulls/[number]` — the bare `name`
    // alone 404s.
    let repo = e.repo.clone();
    Some(Event {
        kind: kind.into(),
        href: format!("/{repo}/pulls/{}", e.number),
        title: format!("{verb} #{} {}", e.number, e.title).trim_end().to_string(),
        detail,
        repo: name,
        actor: e.actor,
        at: e.at_ms / 1000,
    })
}

/// One thing that happened, as the feed shows it.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Event {
    /// `commit` | `pull_opened` | `pull_merged` | `repo_created`
    kind: String,
    repo: String,
    /// Who did it. The empty string when only the system knows.
    actor: String,
    title: String,
    /// The short thing under the title — a sha, a branch, a number.
    detail: String,
    /// Seconds since the epoch. Formatted by the reader, in their locale.
    at: i64,
    /// Where clicking it goes, relative to the site root.
    href: String,
}

/// The rail's worth of feed, and the whole page's.
///
/// Each repo read costs two upstream round trips, so the depth is what the caller
/// is paying for. A rail is a glance at half a dozen repos; the page is willing to
/// walk further, but still not the whole namespace — an archive would need the
/// event log this deliberately does not keep.
pub(crate) const FEED_EVENTS: usize = 10;
pub(crate) const FEED_EVENTS_MAX: usize = 100;
pub(crate) fn feed_depth(events: usize) -> (usize, usize) {
    if events <= FEED_EVENTS { (6, 5) } else { (20, 20) }
}

/// What has happened lately across an owner's repos.
///
/// DERIVED, not recorded. Nothing writes an event log — the feed is assembled
/// from what the directory and git already know, which means it is correct for
/// repos that existed long before it, and there is no second copy of the truth
/// to drift. The cost is that it can only show what those two sources record: a
/// commit, a change opened or merged, a repo created. A deploy or a pipeline run
/// is not in here because nothing in this system knows about one.
pub(crate) async fn activity(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let Some(owner) = q.get("owner").map(|s| s.trim()).filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "owner is required").into_response();
    };
    // Clamped, not rejected: a caller asking for a thousand wants as many as we
    // will give, not an error.
    let want = q
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(FEED_EVENTS)
        .clamp(1, FEED_EVENTS_MAX);
    let (feed_repos, per_repo) = feed_depth(want);
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match may_act_under(db, &user, owner).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such owner").into_response(),
        Err(e) => {
            eprintln!("feed authorization: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not read the feed").into_response();
        }
    }

    // Membership was just established, so the private names under this owner are this caller's
    // to see — the same order `list_repos` uses before it passes `true` on.
    let repos = match repo_listing(&api, owner, true).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("feed repos: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not read the feed").into_response();
        }
    };

    let mut events: Vec<Event> = Vec::new();

    events.extend(repos.iter().map(repo_created));

    // `owner/name`, not the bare name: `e.repo` on a stream event is also `owner/name`, and a
    // same-named repo under a different owner must never match (that was the leak — filtering
    // on the basename let `bob/web`'s events through alice's `alice/web` feed).
    let scope: std::collections::HashSet<String> =
        repos.iter().map(|r| format!("{}/{}", r.owner, r.name)).collect();
    let stream_events: Vec<Event> = api
        .cache
        .xrevrange("events", want.max(FEED_EVENTS_MAX))
        .await
        .iter()
        .filter_map(|(_, fields)| {
            let e = events::from_fields(fields)?;
            // Events are global, one stream for every repo; the feed is per-owner. Only
            // `repo_listing` told us which repos this caller may see, so filter to those, on the
            // full `owner/name` — see `scope` above for why the bare name is not enough.
            if !scope.contains(&e.repo) {
                return None;
            }
            let name = e.repo.split('/').next_back().unwrap_or(&e.repo).to_string();
            pull_event(e, name)
        })
        .take(want)
        .collect();

    events.extend(stream_events);
    // No fallback here on purpose. The PR half of the feed is stream-only now: a Redis flush
    // thins it out until new events arrive, and that is accepted. It loses no truth — every
    // repo's pull requests stay complete and readable from the node that owns them, and only
    // this aggregated VIEW goes quiet. The obvious alternative, asking each owning node for its
    // repo's pulls, is forbidden by the no-peer-fan-out-on-the-read-path rule: a rolling restart
    // must never break a listing. The feed does not go blank either — its `repo_created` half
    // above reads the listing markers, which are durable object-store keys, not the stream.

    // The commits. Newest repos first, and only a few of them: each is a round
    // trip to the node that owns it, and a feed nobody scrolls should not cost
    // one request per repo in the namespace.
    for r in repos.iter().take(feed_repos) {
        // Two calls, not one: `log` starts from an OID, and the tip of a branch
        // is exactly the thing that changes. Asking for the refs first is also
        // what makes an empty repo cost nothing here.
        let Some(refs) = feed_get(&api, &r.owner, format!(
            "/api/{}/{}/refs", encode(&r.owner), encode(&r.name)
        )).await else { continue };
        let Ok(refs) = serde_json::from_str::<Vec<serde_json::Value>>(&refs) else { continue };
        let tip = refs
            .iter()
            .find(|x| x.get("kind").and_then(|v| v.as_str()) == Some("branch")
                && x.get("name").and_then(|v| v.as_str()).is_some_and(|n| n.ends_with("/main") || n.ends_with("/master")))
            .or_else(|| refs.iter().find(|x| x.get("kind").and_then(|v| v.as_str()) == Some("branch")))
            .and_then(|x| x.get("oid").and_then(|v| v.as_str()));
        let Some(tip) = tip else { continue };

        let Some(body) = feed_get(&api, &r.owner, format!(
            "/api/{}/{}/log/{}?n={per_repo}", encode(&r.owner), encode(&r.name), encode(tip)
        )).await else { continue };
        let Ok(commits) = serde_json::from_str::<Vec<serde_json::Value>>(&body) else { continue };
        for c in commits {
            let oid = c.get("oid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let msg = c.get("message").and_then(|v| v.as_str()).unwrap_or_default();
            let title = msg.lines().next().unwrap_or_default().to_string();
            let at = c.get("time").and_then(|v| v.as_i64()).unwrap_or(0);
            if oid.is_empty() {
                continue;
            }
            events.push(Event {
                kind: "commit".into(),
                repo: r.name.clone(),
                actor: c.get("author").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                title,
                detail: oid.chars().take(7).collect(),
                at,
                href: format!("/{}/{}/commit/{}", r.owner, r.name, oid),
            });
        }
    }

    events.sort_by_key(|e| std::cmp::Reverse(e.at));
    events.truncate(want);
    axum::Json(events).into_response()
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testing::*;

    /// Opening a PR must publish exactly one `PullOpened` carrying `repo` and `number` — the
    /// contract task 2 exists to satisfy. Exercised directly against `publish_pull_event` rather
    /// than through the HTTP handler: the handler needs a live Mongo-backed `Directory`, which
    /// this test suite has no fixture for, but the publish call itself is what's under test.
    /// The feed's `XREVRANGE` read must come back newest-first and capped at the requested
    /// count — the same guarantee `activity()` leans on to build the PR half of the feed
    /// without a full Mongo scan. Exercised against `Cache` + `pull_event` directly (see
    /// `opening_a_pull_publishes_pull_opened` above for why: `activity()` itself needs a
    /// live Mongo-backed `Directory` this suite has no fixture for).
    #[tokio::test]
    async fn xrevrange_feed_events_are_newest_first_capped_at_n() {
        let api = test_api_with_secret("s").await;
        for n in 1..=3 {
            publish_pull_event(
                &api.cache,
                Kind::PullOpened,
                "alice/web",
                n,
                "alice@example.com",
                "fix the thing",
                "main",
                "fix-it",
            )
            .await;
        }
        let rows: Vec<Event> = api
            .cache
            .xrevrange("events", 2)
            .await
            .iter()
            .filter_map(|(_, fields)| {
                let e = events::from_fields(fields)?;
                pull_event(e, "web".to_string())
            })
            .collect();
        assert_eq!(rows.len(), 2, "capped at the requested count of 2");
        assert_eq!(rows[0].title, "opened #3 fix the thing", "newest first");
        assert_eq!(rows[1].title, "opened #2 fix the thing");
        assert_eq!(rows[0].detail, "fix-it into main", "the format the feed renders");
    }

    /// The two conditions that leave `activity()` with no PR rows at all: a stream entry
    /// for a repo the caller cannot see (filtered against the caller's `owner/name` scope, the
    /// same shape `activity()` builds from `repos_for`), and a kind the feed does not show at
    /// all. Either one leaves `stream_events` empty, which is exactly the trigger `activity()`
    /// checks.
    #[tokio::test]
    async fn events_outside_the_feeds_scope_are_dropped_not_shown() {
        let api = test_api_with_secret("s").await;
        publish_pull_event(
            &api.cache,
            Kind::PullCommented,
            "alice/web",
            1,
            "alice@example.com",
            "",
            "",
            "",
        )
        .await; // not a kind the feed shows
        publish_pull_event(
            &api.cache,
            Kind::PullOpened,
            "bob/other",
            2,
            "bob@example.com",
            "t",
            "main",
            "h",
        )
        .await; // not the caller's repo at all

        let scope: std::collections::HashSet<String> = ["alice/web".to_string()].into_iter().collect();
        let rows: Vec<Event> = api
            .cache
            .xrevrange("events", 10)
            .await
            .iter()
            .filter_map(|(_, fields)| {
                let e = events::from_fields(fields)?;
                if !scope.contains(&e.repo) {
                    return None;
                }
                let name = e.repo.split('/').next_back().unwrap_or(&e.repo).to_string();
                pull_event(e, name)
            })
            .collect();
        assert!(rows.is_empty(), "neither event belongs in this caller's feed");
    }

    /// With the stream empty — Redis flushed, down, or simply nothing published yet — the PR half
    /// of the feed is gone (there is no Mongo fallback any more), but the feed must still render
    /// its `repo_created` half rather than blowing up or coming back blank.
    #[tokio::test]
    async fn feed_still_renders_repo_created_when_the_stream_is_empty() {
        let api = test_api_with_secret("s").await;
        // A marker row, as `repo_listing` builds it — no directory involved.
        let repos = vec![RepoOut {
            id: "alice/web".into(),
            owner: "alice".into(),
            name: "web".into(),
            public: true,
            description: String::new(),
            created_by: "alice@example.com".into(),
            created_at: 1_700_000_000_000,
        }];

        // The same assembly `activity()` does, with nothing in the stream.
        let mut events: Vec<Event> = repos.iter().map(repo_created).collect();
        let stream_events: Vec<Event> = api
            .cache
            .xrevrange("events", 10)
            .await
            .iter()
            .filter_map(|(_, fields)| pull_event(events::from_fields(fields)?, "web".to_string()))
            .collect();
        assert!(stream_events.is_empty(), "nothing was published");
        events.extend(stream_events);

        assert_eq!(events.len(), 1, "the repo_created half survives an empty stream");
        assert_eq!(events[0].kind, "repo_created");
        assert_eq!(events[0].href, "/alice/web");
    }

    /// The owner-scoping leak this replaces: a same-named repo under a DIFFERENT owner
    /// (`bob/web` vs `alice/web`) must never pass alice's scope filter just because the basename
    /// matches — that was the bug (filtering on `e.repo`'s last path segment alone). And the
    /// href on a stream-sourced row must carry the owner (`/{owner}/{name}/pulls/{n}`), not the bare `/{name}/pulls/{n}` that used to 404.
    #[tokio::test]
    async fn same_named_repo_under_another_owner_is_excluded_and_href_carries_owner() {
        let api = test_api_with_secret("s").await;
        publish_pull_event(
            &api.cache,
            Kind::PullOpened,
            "bob/web",
            9,
            "bob@example.com",
            "bob's private title",
            "main",
            "bob-branch",
        )
        .await;
        publish_pull_event(
            &api.cache,
            Kind::PullOpened,
            "alice/web",
            9,
            "alice@example.com",
            "alice's title",
            "main",
            "alice-branch",
        )
        .await;

        // alice's feed scope: only her own `owner/name` rows, never bob's same-named repo.
        let scope: std::collections::HashSet<String> = ["alice/web".to_string()].into_iter().collect();
        let rows: Vec<Event> = api
            .cache
            .xrevrange("events", 10)
            .await
            .iter()
            .filter_map(|(_, fields)| {
                let e = events::from_fields(fields)?;
                if !scope.contains(&e.repo) {
                    return None;
                }
                let name = e.repo.split('/').next_back().unwrap_or(&e.repo).to_string();
                pull_event(e, name)
            })
            .collect();

        assert_eq!(rows.len(), 1, "bob's same-named repo must be excluded");
        assert!(rows[0].title.contains("alice's title"), "must not leak bob's PR title");
        assert_eq!(rows[0].href, "/alice/web/pulls/9", "href must carry the owner, not just the name");
    }
}
