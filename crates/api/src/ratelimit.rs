//! Token buckets for the two anonymous write surfaces (`POST /v1/cli/code`, which writes a Mongo
//! row per call, and `POST /v1/signin/email`, which sends a mail per call). Keyed by the client
//! address the ingress supplies, or by the email inside the body — the one thing the caller
//! cannot make up fresh per request without it costing them the mail.
//!
//! ponytail: per process. Each api replica keeps its own buckets, so the real ceiling is
//! `limit × replicas`; a shared counter in Redis is the upgrade if the replica count grows past
//! what that slack tolerates.

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Past this many distinct keys the full buckets are dropped — a full bucket carries no
/// information, so forgetting it changes nothing. Same sweep-on-overflow as `storage::auth`.
const SWEEP_AT: usize = 10_000;

pub(crate) struct Limiter {
    capacity: f64,
    per_sec: f64,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

impl Limiter {
    /// `capacity` calls at once, refilling evenly over `period`.
    pub(crate) fn new(capacity: u32, period: Duration) -> Self {
        Self {
            capacity: f64::from(capacity.max(1)),
            per_sec: f64::from(capacity.max(1)) / period.as_secs_f64().max(1e-9),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// `N/SECONDS`, e.g. `20/600`. An unparseable value falls back to `default` rather than to
    /// no limit: a typo must not open the surface.
    pub(crate) fn from_env(var: &str, default: &str) -> Self {
        let parse = |s: &str| {
            let (n, secs) = s.trim().split_once('/')?;
            Some(Self::new(n.trim().parse().ok()?, Duration::from_secs(secs.trim().parse().ok()?)))
        };
        match std::env::var(var).ok().and_then(|v| parse(&v)) {
            Some(l) => l,
            None => parse(default).expect("default limit is well-formed"),
        }
    }

    /// Ok to proceed, or the seconds until one token is back.
    pub(crate) fn check(&self, key: &str) -> std::result::Result<(), u64> {
        self.check_at(key, Instant::now())
    }

    fn check_at(&self, key: &str, now: Instant) -> std::result::Result<(), u64> {
        let mut b = self.buckets.lock().unwrap_or_else(|p| p.into_inner());
        if b.len() >= SWEEP_AT && !b.contains_key(key) {
            let (cap, rate) = (self.capacity, self.per_sec);
            b.retain(|_, (t, at)| *t + now.duration_since(*at).as_secs_f64() * rate < cap);
        }
        let (tokens, at) = b.entry(key.to_string()).or_insert((self.capacity, now));
        *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * self.per_sec).min(self.capacity);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - *tokens) / self.per_sec).ceil() as u64)
        }
    }
}

/// The address ingress-nginx derived (`X-Real-IP`, from `CF-Connecting-IP` behind Cloudflare —
/// see deploy/ingress-nginx-config.yaml), or the first hop of `X-Forwarded-For`. Anything
/// arriving without either (dev, tests) shares one bucket, which is the safe direction.
pub(crate) fn client_ip(headers: &HeaderMap) -> String {
    let first = |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    first("x-real-ip").or_else(|| first("x-forwarded-for")).unwrap_or_default()
}

fn too_many(retry_after: u64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, retry_after.to_string())],
        format!("too many requests; try again in {retry_after}s"),
    )
        .into_response()
}

/// One bucket per client address.
pub(crate) async fn per_ip(State(l): State<Arc<Limiter>>, req: Request, next: Next) -> Response {
    match l.check(&client_ip(req.headers())) {
        Ok(()) => next.run(req).await,
        Err(secs) => too_many(secs),
    }
}

/// One bucket per `email` in a JSON body. The body is read here and handed on intact; a body
/// that is not `{ "email": … }` passes through for the handler to refuse as it already does.
pub(crate) async fn per_email(State(l): State<Arc<Limiter>>, req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    // 4 KiB is generous for an address; anything bigger is not a sign-in request.
    let bytes = match axum::body::to_bytes(body, 4096).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "body too large").into_response(),
    };
    let email = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v.get("email")?.as_str().map(|e| e.trim().to_lowercase()));
    if let Some(secs) = email.and_then(|e| l.check(&e).err()) {
        return too_many(secs);
    }
    next.run(Request::from_parts(parts, axum::body::Body::from(bytes))).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_past_the_bucket_is_refused_and_refills_over_time() {
        let l = Limiter::new(3, Duration::from_secs(30));
        let t0 = Instant::now();
        for _ in 0..3 {
            assert_eq!(l.check_at("ip", t0), Ok(()));
        }
        // One token every 10 s; the fourth call is 10 s early.
        assert_eq!(l.check_at("ip", t0), Err(10));
        assert_eq!(l.check_at("other", t0), Ok(()), "keys are independent");
        assert_eq!(l.check_at("ip", t0 + Duration::from_secs(10)), Ok(()));
        assert_eq!(l.check_at("ip", t0 + Duration::from_secs(10)), Err(10));
    }

    #[test]
    fn a_cooldown_is_a_bucket_of_one() {
        let l = Limiter::new(1, Duration::from_secs(60));
        let t0 = Instant::now();
        assert_eq!(l.check_at("a@x", t0), Ok(()));
        assert_eq!(l.check_at("a@x", t0 + Duration::from_secs(30)), Err(30));
        assert_eq!(l.check_at("a@x", t0 + Duration::from_secs(60)), Ok(()));
    }

    #[test]
    fn a_bad_env_value_keeps_the_default() {
        std::env::set_var("KLOUDLITE_TEST_LIMIT", "lots");
        let l = Limiter::from_env("KLOUDLITE_TEST_LIMIT", "2/60");
        assert_eq!(l.capacity, 2.0);
    }

    #[test]
    fn the_real_ip_header_wins_and_forwarded_for_takes_the_first_hop() {
        let mut h = HeaderMap::new();
        assert_eq!(client_ip(&h), "");
        h.insert("x-forwarded-for", "1.2.3.4, 10.0.0.1".parse().unwrap());
        assert_eq!(client_ip(&h), "1.2.3.4");
        h.insert("x-real-ip", "5.6.7.8".parse().unwrap());
        assert_eq!(client_ip(&h), "5.6.7.8");
    }
}
