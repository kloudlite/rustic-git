//! Redis stream operations backing `crate::events`: publish, consumer-group read/ack/reclaim,
//! and the plain feed read. Split out of `cache.rs` alongside the generation/get/put half in
//! `mod.rs` — named `disk` to match the brief's file split point, though nothing here touches
//! disk; there is no on-disk cache layer in this codebase to split out (see task-3 report).

use super::{run, run_within, Cache, CMD_TIMEOUT, MAINTENANCE_TIMEOUT};

impl Cache {
    /// `XADD {stream} MAXLEN ~ {maxlen} * {field} {value} …`. Fire-and-forget like `drop_refs`:
    /// the stream is a nudge (see `crate::events`), never the record, so a lost publish is not a
    /// lost event — it just costs the consumer a poll cycle. A disabled cache (`conn: None,
    /// mem: None`) is a silent no-op, same as every other cache miss path.
    pub async fn xadd(&self, stream: &str, maxlen: usize, fields: &[(&'static str, String)]) {
        if let Some(m) = &self.mem_stream {
            // `~` (approximate trim) has no meaning in-process; trim exactly, which is a superset
            // of what the real MAXLEN ~ guarantees and therefore never masks a bug the real one
            // would hide.
            let mut g = m.lock().unwrap();
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let id = format!("{now_ms}-0");
            // The mem-stream stores owned pairs (entries come back owned from Redis on the real
            // path too — see `from_fields`), so the static keys are converted at this boundary,
            // not per publish.
            g.push((id, fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()));
            let len = g.len();
            if len > maxlen {
                g.drain(0..len - maxlen);
            }
            return;
        }
        if let Some(mut c) = self.conn.clone() {
            let mut cmd = redis::cmd("XADD");
            cmd.arg(stream).arg("MAXLEN").arg("~").arg(maxlen).arg("*");
            for (k, v) in fields {
                cmd.arg(k).arg(v);
            }
            // Same fire-and-forget discipline as `drop_refs`; a lost nudge self-heals via each
            // consumer's fallback scan (see `crate::events` module doc).
            if let Err(e) = run::<()>(&mut cmd, &mut c).await {
                tracing::debug!(stream = %stream, error = %e, "cache xadd failed");
            }
        }
    }

    /// `XGROUP CREATE {stream} {group} $ MKSTREAM`. Idempotent by design (see the worker's
    /// startup call): a group that already exists answers `BUSYGROUP`, which is swallowed here
    /// rather than propagated, because every worker replica calls this on boot and only the
    /// first one should ever see it as new. `$` (not `0`), because a fresh group must only see
    /// entries published from here on — replaying history on every process restart would mean
    /// years of the fallback sweep having already covered whatever an old entry pointed at.
    pub async fn xgroup_create_mkstream(&self, stream: &str, group: &str) {
        if let Some(m) = &self.mem_stream {
            // The in-memory stand-in has no groups (see `mem_stream`'s doc); nothing to create.
            let _ = m;
            return;
        }
        if let Some(mut c) = self.conn.clone() {
            let mut cmd = redis::cmd("XGROUP");
            cmd.arg("CREATE").arg(stream).arg(group).arg("$").arg("MKSTREAM");
            if let Err(e) = run::<()>(&mut cmd, &mut c).await {
                if !e.to_string().contains("BUSYGROUP") {
                    tracing::warn!(%stream, %group, error = %e, "consumer group create failed");
                }
            }
        }
    }

    /// `XREADGROUP GROUP {group} {consumer} COUNT {count} BLOCK {block_ms} STREAMS {stream} >`.
    /// A disabled/absent cache answers empty, same as every other cache miss — the caller's
    /// periodic sweep is what makes that safe (see `crate::events` module doc).
    pub async fn xreadgroup(
        &self,
        stream: &str,
        group: &str,
        consumer: &str,
        count: usize,
    ) -> Vec<(String, Vec<(String, String)>)> {
        if self.mem_stream.is_some() {
            // No consumer-group delivery in the in-memory stand-in (see `mem_stream`'s doc);
            // a test that needs redelivery semantics exercises the real Redis-backed path.
            return Vec::new();
        }
        let Some(mut c) = self.conn.clone() else { return Vec::new() };
        let mut cmd = redis::cmd("XREADGROUP");
        cmd.arg("GROUP")
            .arg(group)
            .arg(consumer)
            .arg("COUNT")
            .arg(count)
            .arg("STREAMS")
            .arg(stream)
            .arg(">");
        // Deliberately NOT `BLOCK`. `ConnectionManager` multiplexes every command in the process
        // onto ONE connection, so a blocking read parks that connection for its whole timeout and
        // every other command queues behind it. With several lanes each blocking, XAUTOCLAIM never
        // got a turn and failed on every attempt in production, at any timeout — head-of-line
        // blocking, not a slow command. A plain read plus the caller's existing idle sleep gives
        // the same wake-up latency without holding the shared connection. If a blocking read is
        // ever wanted back, it needs its OWN connection, not this one.
        match tokio::time::timeout(CMD_TIMEOUT, cmd.query_async::<StreamReply>(&mut c)).await {
            Ok(Ok(reply)) => reply.0,
            Ok(Err(e)) => {
                tracing::warn!(%stream, %group, error = %e, "consumer group read failed");
                Vec::new()
            }
            Err(_) => Vec::new(), // timed out waiting; the caller's sweep covers it
        }
    }

    /// `XACK {stream} {group} {id}`. Fire-and-forget like `xadd`: an ack that is lost to a Redis
    /// blip just means the entry gets redelivered later (by `XAUTOCLAIM` or a PEL replay) and the
    /// worker does one redundant check — never a lost or duplicated merge, since `check_one` and
    /// `claim_merge` are themselves idempotent claims in the repo's own database
    /// (`pulls::claim_merge`).
    pub async fn xack(&self, stream: &str, group: &str, id: &str) {
        if self.mem_stream.is_some() {
            return;
        }
        if let Some(mut c) = self.conn.clone() {
            let mut cmd = redis::cmd("XACK");
            cmd.arg(stream).arg(group).arg(id);
            if let Err(e) = run::<()>(&mut cmd, &mut c).await {
                tracing::warn!(%stream, %group, %id, error = %e, "consumer group ack failed");
            }
        }
    }

    /// `XAUTOCLAIM {stream} {group} {consumer} {min_idle_ms} 0-0 COUNT {count}`. Re-delivers
    /// entries whose original consumer took them but never acked within `min_idle_ms` — the
    /// "consumer died mid-processing" case. Started at cursor `0-0` and the returned cursor is
    /// discarded: callers run this on a timer against the whole PEL rather than resuming a scan,
    /// which is simpler and cheap enough at this stream's volume (`MAXLEN` bounds it).
    pub async fn xautoclaim(
        &self,
        stream: &str,
        group: &str,
        consumer: &str,
        min_idle_ms: u64,
        count: usize,
    ) -> Vec<(String, Vec<(String, String)>)> {
        if self.mem_stream.is_some() {
            return Vec::new();
        }
        let Some(mut c) = self.conn.clone() else { return Vec::new() };
        let mut cmd = redis::cmd("XAUTOCLAIM");
        cmd.arg(stream)
            .arg(group)
            .arg(consumer)
            .arg(min_idle_ms)
            .arg("0-0")
            .arg("COUNT")
            .arg(count);
        match run_within::<AutoclaimReply>(MAINTENANCE_TIMEOUT, &mut cmd, &mut c).await {
            Ok(reply) => reply.1 .0,
            Err(e) => {
                tracing::warn!(%stream, %group, error = %e, "consumer group autoclaim failed");
                Vec::new()
            }
        }
    }

    /// `XREVRANGE {stream} + - COUNT {count}`: newest entries first, capped at `count`. Unlike
    /// `xreadgroup` this is a plain read — no group, no ack, no redelivery — because the feed
    /// only ever wants "what recently happened", not a work queue: an entry trimmed by `MAXLEN`
    /// before a caller reads it is just not shown, the same as it never happened.
    pub async fn xrevrange(&self, stream: &str, count: usize) -> Vec<(String, Vec<(String, String)>)> {
        if let Some(m) = &self.mem_stream {
            let g = m.lock().unwrap();
            return g.iter().rev().take(count).cloned().collect();
        }
        let Some(mut c) = self.conn.clone() else { return Vec::new() };
        let mut cmd = redis::cmd("XREVRANGE");
        cmd.arg(stream).arg("+").arg("-").arg("COUNT").arg(count);
        match run::<StreamEntries>(&mut cmd, &mut c).await {
            Ok(reply) => reply.0,
            Err(e) => {
                tracing::warn!(%stream, error = %e, "stream read failed");
                Vec::new()
            }
        }
    }
}

/// One stream's worth of entries out of an `XREADGROUP {..} STREAMS {stream} >` reply, which
/// nests as `[[stream_name, [[id, [field, value, ...]], ...]]]` — one element per stream
/// requested, and this crate only ever asks for one.
struct StreamReply(Vec<(String, Vec<(String, String)>)>);

impl redis::FromRedisValue for StreamReply {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        // A `BLOCK` timeout with nothing to deliver answers Nil, not an empty array.
        if matches!(v, redis::Value::Nil) {
            return Ok(StreamReply(Vec::new()));
        }
        type OneStream = (String, Vec<(String, Vec<(String, String)>)>);
        let streams: Vec<OneStream> = redis::FromRedisValue::from_redis_value(v)?;
        Ok(StreamReply(streams.into_iter().flat_map(|(_, entries)| entries).collect()))
    }
}

/// `XAUTOCLAIM` replies `[next_cursor, [[id, [field, value, ...]], ...], deleted_ids]` (Redis 7+
/// adds the trailing deleted-ids array; earlier servers omit it). Only the entries in the middle
/// matter here — the cursor is discarded, see `xautoclaim`'s doc comment.
struct AutoclaimReply(#[allow(dead_code)] String, StreamEntries);
struct StreamEntries(Vec<(String, Vec<(String, String)>)>);

impl redis::FromRedisValue for StreamEntries {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        Ok(StreamEntries(redis::FromRedisValue::from_redis_value(v)?))
    }
}

impl redis::FromRedisValue for AutoclaimReply {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        let redis::Value::Array(items) = v else {
            return Err((redis::ErrorKind::TypeError, "expected an array for XAUTOCLAIM").into());
        };
        let cursor: String = items
            .first()
            .map(redis::FromRedisValue::from_redis_value)
            .transpose()?
            .unwrap_or_default();
        let entries: StreamEntries = items
            .get(1)
            .map(redis::FromRedisValue::from_redis_value)
            .transpose()?
            .unwrap_or(StreamEntries(Vec::new()));
        Ok(AutoclaimReply(cursor, entries))
    }
}
