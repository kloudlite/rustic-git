/** When a snapshot record was taken, in millis.
 *
 *  The one place the wire's field name lives. `/v1/volumes/{name}/history` builds its rows by
 *  hand (`crates/workspaces/src/api.rs:commit_model_history_rows`) and emits camelCase
 *  `createdAt`; it used to serialize `registry::CommitRecord`, whose field is `created_at`, and
 *  eight readers here kept the old name — every one of them silently produced `Invalid Date`.
 *  `NaN` for a record with no timestamp: an unorderable row is the truth, the epoch is not. */
export function snapshotTime(record: { createdAt: string | null }): number {
  return record.createdAt ? Date.parse(record.createdAt) : NaN;
}
