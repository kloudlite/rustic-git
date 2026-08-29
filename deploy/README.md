# Deploy notes

Rebuilding either cluster from what survives in Azure: `RECOVERY.md`. Which Azure retention
switches must be on for that to work, who holds which credential, and how each is rotated:
`BACKUPS.md`. What pages when something breaks: `alerts.md`. The k3s side's own build order:
`k3s/README.md`.

## Incomplete multipart uploads

The registry's chunked blob uploads (`crates/registry/src/uploads.rs`) stream parts into a
multipart upload and abort it on every path we control: a cancelled session, a refused chunk, and
the GC worker's sweep of an abandoned session. Two paths are out of our hands — a crash between
the part upload and the sidecar write, and `WriteMultipart::finish` failing mid-part — and those
leave parts in the bucket that no object ever references. The bucket needs its own rule for them:

- S3: a lifecycle rule with `AbortIncompleteMultipartUpload` at 1 day (longer than
  `RUSTIC_GIT_UPLOAD_GRACE_SECS`, default 24 h, so the rule never races a live session).
- Azure Blob: uncommitted blocks expire after 7 days on their own; nothing to configure.
