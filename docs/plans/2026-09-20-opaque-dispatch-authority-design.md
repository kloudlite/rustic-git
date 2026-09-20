# Opaque Dispatch Authority Design

## Decision

O05 and O02 share an in-memory dispatch authority. O05 issues opaque tokens after it durably authorizes an attempt; O02 consumes a token immediately before invoking an approval-required adapter.

## Authority

The authority owns a private identity-keyed registry. A token has no public fields and cannot be constructed into a valid authorization by a caller. Registry entries bind operation, step, capability, capability version, payload digest, and attempt.

O05 issues a token only from a granted `resume()` or a validated `retryStep()`. O02 receives the token and the trusted authority separately, then calls `consume(token, claims)`. Consumption removes the token before validating claims, so every consume attempt is one-shot, including a mismatched attempt.

The authority checks the current O05 snapshot before accepting the token. Fabricated tokens, tokens from another authority, replayed tokens, prior-attempt tokens, terminal or otherwise stale steps, and capability, version, digest, step, or attempt mismatches fail closed.

Capability version is the approval-policy revision boundary. Changing approval semantics requires a capability version change; this design does not introduce a second policy revision system.

## Wiring

The composition root creates one authority and gives it to the operation store and capability registry. Public dispatch dependencies carry only an opaque token. Arbitrary callbacks are not accepted as authorization.

Reads and approval-free capabilities do not require a token and retain their current behavior.

## Recovery

Recovery remains expiry-only. An in-memory token does not survive restart, and recovery cannot reconstruct authoritative arguments, so reconciliation, dispatch, retry, and abort remain deferred.

## Scheduler Coverage

Restore typed open-object binding coverage. Missing-output diagnostics assert the exact path `$.calls[1].argsFrom.id`.

## Tests

- A fabricated token cannot authorize dispatch.
- A token from another authority cannot authorize dispatch.
- The first consume attempt burns the token, whether claims match or not.
- A previous-attempt token cannot authorize a retry.
- Terminal, cancelled, failed, or unknown-outcome state invalidates a token.
- Capability, version, digest, step, and attempt mismatches fail closed.
- A real store and registry authorize an initial mutation and a retry only with their respective tokens.
- Recovery executes expiry only.
