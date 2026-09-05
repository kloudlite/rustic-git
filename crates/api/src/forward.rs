use super::*;

/// `read_bounded`, as the text a handler relays. An oversized reply is an empty string, which the
/// relaying status code already explains better than a truncated body would.
pub(crate) async fn text_bounded(r: reqwest::Response) -> String {
    kloudlite_core::httpx::read_bounded(r)
        .await
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// The one way this tier talks to the node that owns a repo: present the peer secret, name the
/// reader, send, and turn an unreachable node into one 502 everywhere.
///
/// The peer secret is not an identity. It says "a node in this fleet is asking", and the node
/// still applies the same read check it applies to anyone — so it has to be told WHO is reading,
/// or a private repo answers 401 to a caller who is entitled to it. The caller establishes that
/// entitlement before calling this; `owner` is what it asserts upstream. `None` is for the writes
/// the node authorizes structurally rather than per reader.
pub(crate) async fn to_owner(
    api: &Api,
    req: reqwest::RequestBuilder,
    owner: Option<&str>,
) -> std::result::Result<reqwest::Response, Response> {
    let req = req.header(kloudlite_core::peer::PEER_HEADER, &api.secret);
    let req = match owner {
        Some(o) => req.header(kloudlite_core::peer::OWNER_HEADER, o),
        None => req,
    };
    send_retrying(req).await.map_err(|e| {
        tracing::error!(error = %e, "upstream.request.failed");
        (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response()
    })
}

/// Ask the node that owns this repo to do something; the caller reads the outcome off the status.
pub(crate) async fn ask_owner(api: &Api, path: String) -> std::result::Result<u16, Response> {
    let r = to_owner(api, api.client.post(format!("{}{path}", api.upstream)), None).await?;
    Ok(r.status().as_u16())
}

/// Read a repo-scoped route from the owning node, as `owner`, and pass its answer through.
pub(crate) async fn read_from_owner(api: &Api, owner: &str, path: String) -> Response {
    match to_owner(api, api.client.get(format!("{}{path}", api.upstream)), Some(owner)).await {
        Ok(r) => relay(r).await,
        Err(r) => r,
    }
}

/// Forward a JSON body to the owning node, as `owner`, and pass its answer straight back.
///
/// The sibling of `ask_owner` for the two PR writes that carry real user text. The node's own
/// refusals ("a title is required", "say something") are written for the person typing, so they
/// are relayed rather than replaced — the same choice `commit_patch` makes for its forward.
pub(crate) async fn tell_owner(api: &Api, owner: &str, path: String, body: serde_json::Value) -> Response {
    let req = api.client.post(format!("{}{path}", api.upstream)).json(&body);
    match to_owner(api, req, Some(owner)).await {
        Ok(r) => relay(r).await,
        Err(r) => r,
    }
}

/// Pass an upstream reply through with its own status. Only a success body is JSON — a refusal is
/// the node's own prose, and labelling that `application/json` makes it unreadable to the caller.
pub(crate) async fn relay(r: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let text = text_bounded(r).await;
    if status.is_success() {
        (status, [(header::CONTENT_TYPE, "application/json")], text).into_response()
    } else {
        (status, text).into_response()
    }
}

/// A rolling node answers 502/503 (its proxy has no backend yet) or 421 (the ownership map moved
/// under us) for the second or two a pod takes to hand over. One retry turns that into a served
/// request instead of a failed one.
///
/// `try_clone` is the whole safety story, and it is structural: reqwest cannot clone a request
/// whose body is a stream, so a git receive-pack or a blob upload is sent exactly once by
/// construction — only GET/HEAD and bodies this tier holds in memory can be replayed.
// ponytail: fixed 250 ms, one attempt; jittered backoff when a profile says so.
pub(crate) async fn send_retrying(req: reqwest::RequestBuilder) -> reqwest::Result<reqwest::Response> {
    let again = req.try_clone();
    let first = req.send().await;
    let worth = match &first {
        Err(_) => true,
        Ok(r) => matches!(r.status().as_u16(), 421 | 502 | 503),
    };
    let Some(again) = again.filter(|_| worth) else {
        return first;
    };
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    again.send().await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A node that answers `codes` in order, then 200, counting every call it got.
    async fn flaky(codes: Vec<u16>) -> (String, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let app = axum::Router::new().fallback(axum::routing::any(move |_: axum::extract::Request| {
            let seen = seen.clone();
            let codes = codes.clone();
            async move {
                let n = seen.fetch_add(1, Ordering::SeqCst);
                axum::http::StatusCode::from_u16(*codes.get(n).unwrap_or(&200)).unwrap()
            }
        }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (url, calls)
    }

    /// Catches: a roll's transient 503 reaching the caller, or a retry that never stops.
    #[tokio::test]
    async fn a_rolling_node_is_asked_exactly_twice() {
        let (url, calls) = flaky(vec![503]).await;
        let c = reqwest::Client::new();
        let r = super::send_retrying(c.get(&url)).await.unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// Catches: replaying a streamed body — a push or a blob upload sent twice corrupts both.
    #[tokio::test]
    async fn a_streamed_body_is_never_replayed() {
        let (url, calls) = flaky(vec![503]).await;
        let c = reqwest::Client::new();
        let body = reqwest::Body::wrap_stream(futures::stream::once(async {
            Ok::<_, std::io::Error>(b"pack".to_vec())
        }));
        let r = super::send_retrying(c.put(&url).body(body)).await.unwrap();
        assert_eq!(r.status().as_u16(), 503);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
