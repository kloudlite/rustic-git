/** The snapshots page's in-flight push marker, or `null` once the record has landed. A push is
 *  over when the history has GROWN past what it held at submit; any other change in length —
 *  a deleted record — says nothing about it. Its own module, free of `server-only`, because
 *  the client component that needs it cannot reach `lib/env-page`. */
export function pendingPush<A extends { had: number }>(asked: A | null, historyLength: number): A | null {
  return asked && historyLength > asked.had ? null : asked;
}
