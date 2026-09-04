use super::*;
use kloudlite_git_storage::events;

/// GET a browse route from the owning node, for the feed.
///
/// `None` for anything that did not work. One unreachable or empty repo must not
/// empty the whole feed — a glance at what happened is worth having in part.
pub(crate) async fn feed_get(api: &Api, owner: &str, path: String) -> Option<String> {
    let res = to_owner(api, api.client.get(format!("{}{path}", api.upstream)), Some(owner))
        .await
        .ok()?;
    if !res.status().is_success() {
        return None;
    }
    Some(text_bounded(res).await)
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

/// How many owning nodes the feed asks at once, and how long it waits for all of them. Serial
/// was up to 20 repos times two GETs on the 15 s client timeout each — minutes, for any member
/// who opened the page while one node was slow. Whatever has answered by the deadline is the
/// feed; a repo that has not is simply absent from a glance-at-it rail.
pub(crate) const FEED_FANOUT: usize = 4;
pub(crate) const FEED_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// The commit half of the feed: the newest repos first, only a few of them, each a round trip to
/// the node that owns it — a feed nobody scrolls should not cost one request per repo in the
/// namespace.
pub(crate) async fn commits_across(api: &Api, repos: &[RepoOut], feed_repos: usize, per_repo: usize) -> Vec<Event> {
    use futures::StreamExt as _;
    let deadline = tokio::time::Instant::now() + FEED_DEADLINE;
    // Built up front rather than mapped lazily: a closure borrowing `api` and `r` trips the
    // higher-ranked lifetime check that `buffer_unordered` on a stream of borrows needs.
    let futs: Vec<_> = repos.iter().take(feed_repos).map(|r| repo_commits(api, r, per_repo)).collect();
    let mut batches = futures::stream::iter(futs).buffer_unordered(FEED_FANOUT);
    let mut events = Vec::new();
    while let Ok(Some(batch)) = tokio::time::timeout_at(deadline, batches.next()).await {
        events.extend(batch);
    }
    events
}

/// One repo's latest commits. Two calls, not one: `log` starts from an OID, and the tip of a
/// branch is exactly the thing that changes. Asking for the refs first is also what makes an
/// empty repo cost nothing here.
async fn repo_commits(api: &Api, r: &RepoOut, per_repo: usize) -> Vec<Event> {
    let mut events = Vec::new();
    let Some(refs) = feed_get(api, &r.owner, format!(
        "/api/{}/{}/refs", encode(&r.owner), encode(&r.name)
    )).await else { return events };
    let Ok(refs) = serde_json::from_str::<Vec<serde_json::Value>>(&refs) else { return events };
    let tip = refs
        .iter()
        .find(|x| x.get("kind").and_then(|v| v.as_str()) == Some("branch")
            && x.get("name").and_then(|v| v.as_str()).is_some_and(|n| n.ends_with("/main") || n.ends_with("/master")))
        .or_else(|| refs.iter().find(|x| x.get("kind").and_then(|v| v.as_str()) == Some("branch")))
        .and_then(|x| x.get("oid").and_then(|v| v.as_str()));
    let Some(tip) = tip else { return events };

    let Some(body) = feed_get(api, &r.owner, format!(
        "/api/{}/{}/log/{}?n={per_repo}", encode(&r.owner), encode(&r.name), encode(tip)
    )).await else { return events };
    let Ok(commits) = serde_json::from_str::<Vec<serde_json::Value>>(&body) else { return events };
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
    events
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
            tracing::error!(owner = %owner, error = %e, "feed authorization");
            return (StatusCode::BAD_GATEWAY, "could not read the feed").into_response();
        }
    }

    // Membership was just established, so the private names under this owner are this caller's
    // to see — the same order `list_repos` uses before it passes `true` on.
    let repos = match repo_listing(&api, owner, true).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(owner = %owner, error = %e, "feed repos");
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

    events.extend(commits_across(&api, &repos, feed_repos, per_repo).await);

    events.sort_by_key(|e| std::cmp::Reverse(e.at));
    events.truncate(want);
    axum::Json(events).into_response()
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;

    /// Puts one entry on the stream the way an owning node does, so the feed tests need no fleet.
    #[allow(clippy::too_many_arguments)]
    async fn publish_pull_event(
        cache: &Cache,
        kind: Kind,
        repo: &str,
        number: i64,
        actor: &str,
        title: &str,
        base: &str,
        head: &str,
    ) {
        let at_ms = kloudlite_git_storage::ownership::now_ms() as i64;
        events::publish(
            cache,
            &events::Event {
                kind,
                repo: repo.to_string(),
                number,
                actor: actor.to_string(),
                at_ms,
                title: title.to_string(),
                base: base.to_string(),
                head: head.to_string(),
            },
        )
        .await;
    }

    /// The commit half asks the owning nodes concurrently, but never more than `FEED_FANOUT` at
    /// once — a rolling restart must not turn one member's page view into a thundering herd, and
    /// serial was minutes when one node stalled.
    #[tokio::test(flavor = "multi_thread")]
    async fn commits_fan_out_under_a_concurrency_cap() {
        use axum::{extract::Path, routing::get, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let inflight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let peak2 = peak.clone();
        let gate = move || {
            let (i, p) = (inflight.clone(), peak.clone());
            async move {
                let now = i.fetch_add(1, Ordering::SeqCst) + 1;
                p.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                i.fetch_sub(1, Ordering::SeqCst);
            }
        };
        let (g1, g2) = (gate.clone(), gate);
        let app = Router::new()
            .route("/api/{owner}/{name}/refs", get(move |Path((_, name)): Path<(String, String)>| {
                let g = g1();
                async move {
                    g.await;
                    axum::Json(serde_json::json!([{"kind": "branch", "name": "refs/heads/main", "oid": name}]))
                }
            }))
            .route("/api/{owner}/{name}/log/{oid}", get(move |Path((_, name, _)): Path<(String, String, String)>| {
                let g = g2();
                async move {
                    g.await;
                    axum::Json(serde_json::json!([{"oid": format!("{name}0000000"), "message": name, "time": 1, "author": "a"}]))
                }
            }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });

        let api = Api { upstream: base, ..test_api_with_secret("s").await };
        let repos: Vec<RepoOut> = (0..12)
            .map(|i| RepoOut {
                id: format!("alice/r{i}"),
                owner: "alice".into(),
                name: format!("r{i}"),
                public: true,
                description: String::new(),
                created_by: "alice@example.com".into(),
                created_at: 0,
            })
            .collect();
        let events = commits_across(&api, &repos, repos.len(), 5).await;
        assert_eq!(events.len(), 12, "every repo answered");
        let peak = peak2.load(Ordering::SeqCst);
        assert!(peak > 1, "the fan-out is concurrent, not serial");
        assert!(peak <= FEED_FANOUT, "in-flight peaked at {peak}, over the cap");
    }

    /// The feed's `XREVRANGE` read must come back newest-first and capped at the requested
    /// count — the same guarantee `activity()` leans on to build the PR half of the feed without
    /// a full Mongo scan — and each opened PR must land as exactly ONE entry carrying its `repo`
    /// and `number`. Exercised against `Cache` + `pull_event` directly rather than through the
    /// HTTP handler: `activity()` needs a live Mongo-backed `Directory` this suite has no
    /// fixture for, but the publish and the read are what is under test.
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

        // One publish per opened PR — not zero, and not the double-publish that would show the
        // same change twice in everyone's feed. Read uncapped, so the count is the whole stream.
        let all = api.cache.xrevrange("events", 10).await;
        assert_eq!(all.len(), 3, "one entry per publish");
        let field = |f: &[(String, String)], k: &str| {
            f.iter().find(|(fk, _)| fk == k).map(|(_, v)| v.clone())
        };
        let ones: Vec<_> = all.iter().filter(|(_, f)| field(f, "number").as_deref() == Some("1")).collect();
        assert_eq!(ones.len(), 1, "exactly one entry for #1");
        assert_eq!(field(&ones[0].1, "kind").as_deref(), Some("pull_opened"));
        assert_eq!(field(&ones[0].1, "repo").as_deref(), Some("alice/web"));
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
        let repos = [RepoOut {
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
