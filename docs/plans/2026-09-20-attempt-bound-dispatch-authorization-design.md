# Attempt-Bound Dispatch Authorization Design

## Decision

O05 issues an opaque, in-memory dispatch authorization after it durably authorizes an attempt. O02 consumes that authorization before invoking an approval-required adapter.

## Authorization

The authorization is a private object branded by identity and held in an O05-owned registry. It binds the operation, step, capability and version, current attempt, and dispatch digest. It is not serializable and cannot survive restart.

`resume()` issues the first authorization only after recording and consuming a granted decision. `retryStep()` issues a new authorization for the newly started attempt after applying current retry, session, approval, digest, and deadline policy. Each authorization is one-shot.

O02 accepts only an authorization that O05 validates and consumes for the exact dispatch. Independently constructed, stale, reused, previous-attempt, wrong-capability, and wrong-digest values fail closed. O02 no longer receives the original recorded decision as its dispatch proof.

## Recovery

Recovery cannot reconstruct authoritative arguments or an in-memory authorization. It therefore executes expiry only. Reconciliation is deferred alongside dispatch, retry, and abort until a durable reconciliation envelope exists.

## Minor Corrections

The scheduler diagnostic uses `$.calls[...]`. The capability registry comment states that approval is completed before dispatch and that dispatch validates authorization.

## Tests

- No approved mutation dispatches without a fresh O05 authorization.
- An authorization is consumed once.
- A previous-attempt authorization cannot authorize a retry.
- Expired or otherwise invalid approval state cannot mint retry authorization.
- Capability, version, digest, step, and attempt mismatches fail closed.
- Recovery never reconciles caller-reconstructed arguments.
