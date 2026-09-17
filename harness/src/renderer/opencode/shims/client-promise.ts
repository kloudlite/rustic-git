/**
 * harness: `@opencode-ai/client` is a tarball inside opencode's own repo, not a package on npm
 * (`packages/app/vendor/opencode-ai-client-1.17.13-v2.tgz`). session-ui imports exactly one thing
 * from it, and only as a type — the shape of a file's diff — so that shape is written out here
 * rather than a 1.3 MB client being vendored for a type alias.
 *
 * Kept structurally identical to what the renderer reads: `session-diff.ts`, `session-review.tsx`,
 * `session-turn.tsx`, `context/data.tsx`, `session-review-file-preview-v2.tsx`.
 */
export type FileDiffInfo = {
  file: string;
  before?: string;
  after?: string;
  patch?: string;
  /** Required, as `session-diff.ts:41` and `session-turn.tsx:446` read them without a guard. */
  additions: number;
  deletions: number;
  /** The same three the SDK's own `SnapshotFileDiff` carries; a rename arrives as `modified`. */
  status?: "added" | "deleted" | "modified";
};
