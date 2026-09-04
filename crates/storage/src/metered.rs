//! Every object-store call, timed from our side and counted when it fails.
//!
//! Wrapping the `ObjectStore` is the only choke point there is: `Store.os` is a public handle that
//! six binaries and five crates call directly, and object_store has no interceptor of its own. The
//! wrapper goes on in `config::object_store_views`, so SlateDB's own traffic (it is handed the same
//! handle) is measured too — which is the point: this is OUR view of the blob store, not the
//! backend's.
//!
//! Only the seven required trait methods are here. Everything else on `ObjectStoreExt` — `get`,
//! `head`, `put`, `delete`, `copy`, `rename` — is a default that funnels through one of them, so
//! there is nothing to add and nothing that can be double counted.

use kloudlite_git_core::metrics::dep_done;
use slatedb::object_store::path::Path;
use slatedb::object_store::{
    CopyOptions, Error, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
};
use std::sync::Arc;
use std::time::Instant;

const DEP: &str = "blob";

/// Every op this wrapper can label, for `metrics::register_dependency` at boot.
pub const OPS: &[&str] = &["get", "head", "put", "put_multipart", "delete", "list", "copy"];

/// The class of an object-store failure. object_store erases the transport error behind
/// `Generic { source }` and exposes no status code, so the transport classes are sniffed from the
/// source's text — here, where the result is one of five stable words, and never as a label.
fn kind_of(e: &Error) -> &'static str {
    match e {
        // Neither is a dependency failure: they are answers. Counted as `other` rather than given
        // classes of their own, because nothing alerts on them.
        Error::NotFound { .. } | Error::AlreadyExists { .. } | Error::NotModified { .. } => "other",
        Error::PermissionDenied { .. } | Error::Unauthenticated { .. } => "refused",
        e => {
            let s = e.to_string().to_lowercase();
            if s.contains("timed out") || s.contains("timeout") {
                "timeout"
            } else if s.contains("connection refused") || s.contains("connect") {
                "refused"
            } else if s.contains("429") || s.contains("too many requests") || s.contains("slow down")
            {
                "status_429"
            } else if s.contains(" 50") || s.contains("internal error") {
                "status_5xx"
            } else {
                "other"
            }
        }
    }
}

fn done<T>(op: &'static str, start: Instant, r: &Result<T>) {
    dep_done(DEP, op, start, r.as_ref().err().map(kind_of));
}

/// A stream's duration is only known when the stream ends, and a stream can end by being dropped
/// (`list` under a `take`). Recording from `Drop` is the one shape that covers both.
struct Streamed(&'static str, Instant);

impl Drop for Streamed {
    fn drop(&mut self) {
        dep_done(DEP, self.0, self.1, None);
    }
}

#[derive(Debug)]
pub struct Metered(pub Arc<dyn ObjectStore>);

impl std::fmt::Display for Metered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

#[async_trait::async_trait]
impl ObjectStore for Metered {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        let start = Instant::now();
        let r = self.0.put_opts(location, payload, opts).await;
        done("put", start, &r);
        r
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        let start = Instant::now();
        let r = self.0.put_multipart_opts(location, opts).await;
        // Only the handshake that hands back the upload handle; the parts go through the handle
        // itself, which is the backend's type and not ours to wrap.
        done("put_multipart", start, &r);
        r
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        // `head` is a `get_opts` with `head: true` — the one distinction worth keeping, since a
        // HEAD is what the health probe and every existence check do.
        let op = if options.head { "head" } else { "get" };
        let start = Instant::now();
        let r = self.0.get_opts(location, options).await;
        done(op, start, &r);
        r
    }

    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<'static, Result<Path>>,
    ) -> futures::stream::BoxStream<'static, Result<Path>> {
        use futures::StreamExt;
        let held = Streamed("delete", Instant::now());
        self.0
            .delete_stream(locations)
            .map(move |r| {
                let _ = &held; // dropped with the stream; that is when the duration lands
                if let Err(e) = &r {
                    dep_done(DEP, "delete", Instant::now(), Some(kind_of(e)));
                }
                r
            })
            .boxed()
    }

    fn list(&self, prefix: Option<&Path>) -> futures::stream::BoxStream<'static, Result<ObjectMeta>> {
        use futures::StreamExt;
        let held = Streamed("list", Instant::now());
        self.0
            .list(prefix)
            .map(move |r| {
                let _ = &held;
                if let Err(e) = &r {
                    dep_done(DEP, "list", Instant::now(), Some(kind_of(e)));
                }
                r
            })
            .boxed()
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        let start = Instant::now();
        let r = self.0.list_with_delimiter(prefix).await;
        done("list", start, &r);
        r
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        let start = Instant::now();
        let r = self.0.copy_opts(from, to, options).await;
        done("copy", start, &r);
        r
    }
}

#[cfg(test)]
mod tests {
    use super::kind_of;
    use slatedb::object_store::Error;

    /// The classifier is what a rule filters on, so it has to be total and stable. The error text
    /// itself never becomes a label — that is the whole reason this function exists.
    #[test]
    fn every_failure_lands_in_one_of_the_five_classes() {
        let generic = |m: &str| Error::Generic { store: "S3", source: m.to_string().into() };
        assert_eq!(kind_of(&generic("operation timed out after 30s")), "timeout");
        assert_eq!(kind_of(&generic("tcp connect error: connection refused")), "refused");
        assert_eq!(kind_of(&generic("response 429 too many requests")), "status_429");
        assert_eq!(kind_of(&generic("server returned 503 internal error")), "status_5xx");
        assert_eq!(kind_of(&generic("something else entirely")), "other");
        assert_eq!(
            kind_of(&Error::NotFound { path: "a".into(), source: "x".to_string().into() }),
            "other",
            "a miss is an answer, not a failure of the dependency"
        );
        for e in [
            generic("timed out"),
            generic("connection refused"),
            generic("whatever"),
            Error::NotFound { path: "a".into(), source: "x".to_string().into() },
        ] {
            assert!(kloudlite_git_core::metrics::ERROR_KINDS.contains(&kind_of(&e)));
        }
    }
}
