# Review Fixes — HTTP / API / Auth Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land every finding of the 2026-08-23 review that touches the HTTP front door, the api tier, auth/JWT, SSH/proxy fencing, `main.rs` and the directory — highest severity first, each behind its own test and commit.

**Architecture:** Each task is an independent fix following the pattern already beside it (copy the sibling, don't invent). The only cross-task dependencies are named in `Interfaces` blocks: Task 1 exposes `judge_ssh`/`judge_pgp`, Task 2 adds `api::peer_only`, Task 4 adds `App::open_repo_after_fence`, Task 19 adds `auth::basic_token`/`auth::unauthorized`, Task 20 adds `api::Identity`. Tests that need Mongo (`Directory`) cannot run in this suite — there is no Mongo fixture anywhere under `tests/` — so directory-side changes are covered by pure helpers extracted to be testable, and the plan says so where that applies.

**Tech Stack:** Rust, axum 0.8, tokio, SlateDB, russh/ssh-key 0.7.0-rc.11, pgp 0.20, mongodb 3.8, jsonwebtoken, reqwest.

**Spec:** `docs/code-review-2026-08-23.md` — sections 1 (Security), 2 (Bugs), 3 (Performance), 4 (Redundancy), 5 (Quality), 6 (Test coverage gaps). Only the items in the scope list below; registry/GC/worker/git-core/web/ops findings belong to other plans.

## Global Constraints

- `cargo test` must pass after every task. Run the named `--test` file the task points at, then the full suite before each commit.
- Clippy bar (`CLAUDE.md`): no NEW warnings in files you touch. `cargo clippy --all-targets -D warnings` has ~15 pre-existing errors (list in the spec's appendix) — ignore those.
- House style (`CLAUDE.md`): comments explain WHY, never what; match `src/http.rs` density. Keep every `// ponytail:` marker you edit near; add one when you cut a corner with a known ceiling. Commit subjects: imperative sentence case, NO tool attribution, no "claude" reference anywhere in the message.
- The routing invariant is untouched: nothing here adds a route (no `BROWSE_TAILS` change) and nothing opens a repo database outside the existing `open`/`open_repo` paths.
- Every `/v2` error stays the OCI envelope; `Digest::parse`, manifest verbatim storage and the blob-deletion rule are not in scope and must not change.
- No new crate dependencies. Everything needed (`futures`, `base64`, `pgp`, `russh`, `tokio::sync::Mutex::const_new`) is already in `Cargo.toml`.

---

## HIGH

### Task 1: SSH commit signatures verify (fingerprint case) — with tests for both signature kinds

**Files:**
- Modify: `src/api/signatures.rs` (split `verify_signature`/`verify_pgp` into lookup + pure judgement; tests)
- Modify: `src/api/credentials.rs:244-249` (`add_key` ssh branch → `ssh_signing_fingerprints`)
- Modify: `src/directory.rs:297-318` (`connect` runs the one-shot lowercase backfill), plus a pure helper + test
- Modify: `src/gpg.rs:292-300` (`mod tests` → `pub(crate) mod tests`; three helpers `pub(crate)`)

**Interfaces:**
- Produces: `signatures::ssh_signature_fingerprint(&SshSig) -> String`, `signatures::judge_ssh(&SshSig, &[u8], Option<Credential>, &str) -> Verification`, `signatures::judge_pgp(Option<Credential>, &SignatureOf, &[u8]) -> Verification`, `credentials::ssh_signing_fingerprints(&str) -> crate::Result<(String, Vec<String>)>`, `directory::lowercased(&[String]) -> Option<Vec<String>>`.
- Consumes: `crate::gpg::{issuers, fingerprints_of, verify, is_pgp}` (unchanged), `Store::ssh_fingerprint` (unchanged).

**Context:** `Store::ssh_fingerprint` returns `SHA256:<base64>` — mixed case. `add_key` stores that verbatim in `fingerprints`; `verify_signature` looks it up via `signer_by_any`, which lowercases every candidate and runs an exact `$in`. GPG fingerprints are lowercase hex so they match; ssh ones never do → every ssh-signed commit reads `unknown_key`. No test covers `verify_signature` because it takes a `Directory` (Mongo). Fix the root cause (lowercase at registration), repair existing rows (startup pass — idempotent, a handful of rows, no new CLI), and make the judgement pure so it is testable.

- [ ] **Step 1: Split the directory lookup out of the judgement (pure refactor, no behaviour change)**

Replace `verify_pgp` and `verify_signature` in `src/api/signatures.rs` (lines 175-303) with:

```rust
/// The fingerprint an ssh signature presents, in the form `signer_by_any` is queried with.
/// Lowercased here AND at registration (`ssh_signing_fingerprints`): Mongo's `$in` is an exact
/// match, and `SHA256:<base64>` is mixed case, so the two sides must agree on one spelling.
pub(crate) fn ssh_signature_fingerprint(sig: &russh::keys::ssh_key::SshSig) -> String {
    sig.public_key()
        .fingerprint(russh::keys::HashAlg::Sha256)
        .to_string()
        .to_lowercase()
}

fn unverified(code: &'static str, reason: &str) -> Verification {
    Verification { state: "unverified", reason_code: code, signer: None, reason: Some(reason.to_string()) }
}

/// Judge a GPG signature once the directory has answered. `known` is the key the signature's
/// issuers resolved to — normally a subkey's owner, because a commit is signed by a subkey while
/// the person is the primary key behind it; `signer_by_any` walks that back.
pub(crate) fn judge_pgp(
    known: Option<crate::directory::Credential>,
    signed: &SignatureOf,
    payload: &[u8],
) -> Verification {
    let Some(known) = known else {
        return unverified("unknown_key", "signed by a key nobody here has registered");
    };
    use crate::gpg::Reason;
    let reason = crate::gpg::verify(&known.material, &signed.signature, payload, &signed.author_email);
    let words = match reason {
        Reason::Valid => None,
        Reason::RevokedKey => Some("that key has been revoked".to_string()),
        Reason::ExpiredKey => Some("that key had expired".to_string()),
        Reason::Invalid => Some("the signature does not match the commit".to_string()),
        Reason::UnknownKey => Some("the registered key could not be read".to_string()),
        Reason::UnknownSignatureType => Some("the signature could not be read".to_string()),
        Reason::BadEmail => Some(format!(
            "signed by {}, but the commit says {} wrote it",
            known.created_by, signed.author_email
        )),
    };
    Verification {
        state: if reason == Reason::Valid { "verified" } else { "unverified" },
        reason_code: reason.as_str(),
        signer: Some(known.created_by),
        reason: words,
    }
}

/// Judge an ssh signature once the directory has answered.
///
/// Two things have to hold for "verified": the signature is good, AND the key belongs to the
/// person the commit says wrote it. A valid signature by somebody else's key is exactly what a
/// forged authorship line looks like, so it reports as unverified with the reason spelled out.
pub(crate) fn judge_ssh(
    sig: &russh::keys::ssh_key::SshSig,
    payload: &[u8],
    known: Option<crate::directory::Credential>,
    author_email: &str,
) -> Verification {
    let Some(known) = known else {
        return unverified("unknown_key", "signed by a key nobody here has registered");
    };
    // The cryptography last: an unknown key is not worth verifying against, and this order
    // means a bad signature and an unknown signer are never confused.
    let key = russh::keys::PublicKey::from(sig.public_key().clone());
    // `git` is the namespace git signs commits under; a signature made for anything else is not
    // a commit signature.
    if key.verify("git", payload, sig).is_err() {
        return unverified("invalid", "the signature does not match the commit");
    }
    if !known.created_by.eq_ignore_ascii_case(author_email.trim()) {
        return Verification {
            state: "unverified",
            reason_code: "bad_email",
            signer: Some(known.created_by.clone()),
            reason: Some(format!(
                "signed by {}, but the commit says {} wrote it",
                known.created_by, author_email
            )),
        };
    }
    Verification { state: "verified", reason_code: "valid", signer: Some(known.created_by), reason: None }
}

/// Judge one signature: decode, ask the directory who holds the key, then judge. The lookup is
/// the only async step and the only one that needs Mongo, which is why the judgement is split
/// off — `judge_ssh`/`judge_pgp` are tested without a directory.
pub(crate) async fn verify_signature(db: &crate::directory::Directory, signed: &SignatureOf) -> Verification {
    use base64::Engine;
    let Ok(payload) = base64::engine::general_purpose::STANDARD.decode(&signed.payload_base64)
    else {
        return unverified("invalid", "the signed content could not be read");
    };
    if crate::gpg::is_pgp(&signed.signature) {
        let Ok(issuers) = crate::gpg::issuers(&signed.signature) else {
            return unverified("unknown_signature_type", "the signature could not be read");
        };
        return match db.signer_by_any(&issuers).await {
            Ok(known) => judge_pgp(known, signed, &payload),
            Err(e) => {
                eprintln!("signer lookup: {e}"); // ponytail: eprintln
                unverified("invalid", "the signing key could not be looked up")
            }
        };
    }
    let Ok(sig) = signed.signature.parse::<russh::keys::ssh_key::SshSig>() else {
        return unverified("unknown_signature_type", "the signature could not be read");
    };
    // Looked up the same way as a GPG key, so one index serves both kinds.
    match db.signer_by_any(&[ssh_signature_fingerprint(&sig)]).await {
        Ok(known) => judge_ssh(&sig, &payload, known, &signed.author_email),
        Err(e) => {
            eprintln!("signer lookup: {e}"); // ponytail: eprintln
            unverified("invalid", "the signing key could not be looked up")
        }
    }
}
```

Also make the GPG test helpers reachable from this file. In `src/gpg.rs` change `mod tests {` (line ~292) to `pub(crate) mod tests {`, and prefix `fn reforge_subkey`, `fn subkey_signature`, `fn gen` with `pub(crate)`.

Run: `cargo build && cargo test --lib gpg`
Expected: builds; gpg tests unchanged and green.

- [ ] **Step 2: Extract the registration-side fingerprint helper in `src/api/credentials.rs`**

Add above `add_key`:

```rust
/// The credential id and the fingerprints an ssh SIGNING key answers to. Kept beside `add_key`
/// and used by it, so a test can build exactly the row registration writes.
pub(crate) fn ssh_signing_fingerprints(key_line: &str) -> crate::Result<(String, Vec<String>)> {
    let f = crate::store::Store::ssh_fingerprint(key_line)?;
    Ok((f.clone(), vec![f]))
}
```

and change the ssh branch of `add_key` (lines 244-249) to:

```rust
    } else {
        match ssh_signing_fingerprints(&body.key) {
            Ok(v) => v,
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        }
    };
```

(Note: NOT lowercased yet — this step only moves code, so the test in Step 3 fails for the real reason.)

- [ ] **Step 3: Write the failing tests**

Append to `src/api/signatures.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::{Credential, CredentialKind};
    use base64::Engine;
    use russh::keys::ssh_key::{LineEnding, SshSig};

    /// A throwaway ed25519 key, generated with `ssh-keygen` and pasted here so the test needs no
    /// binary and no rand_core (see the `host_key` note in main.rs). Its fingerprint,
    /// `SHA256:4RE6N1MZA852R72MTvoTtpbg/gfN4mbFpRIy7W0ei8E`, has upper-case letters — which is the
    /// whole point.
    const TEST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACDjDgvGHLBQbllMJ0mZD8phc152z5WbYfnwT9+FxjTfnQAAAJiaLxWEmi8V
hAAAAAtzc2gtZWQyNTUxOQAAACDjDgvGHLBQbllMJ0mZD8phc152z5WbYfnwT9+FxjTfnQ
AAAEC7fikKolcX288ZDKzeY1u7+Y6xCPYPSsHfKM1EP3nTn+MOC8YcsFBuWUwnSZkPymFz
XnbPlZth+fBP34XGNN+dAAAAEHRlc3RAZXhhbXBsZS5jb20BAgMEBQ==
-----END OPENSSH PRIVATE KEY-----
";

    fn credential(id: String, material: String, fingerprints: Vec<String>) -> Credential {
        Credential {
            id,
            kind: CredentialKind::SigningKey,
            owner: "alice".into(),
            created_by: "alice@example.com".into(),
            name: "laptop".into(),
            material,
            fingerprints,
            created_at: mongodb::bson::DateTime::now(),
        }
    }

    /// What `signer_by_any` does, without Mongo: lowercase each candidate, exact match against
    /// the stored `fingerprints`. The case bug lives exactly in this comparison.
    fn lookup(creds: &[Credential], candidates: &[String]) -> Option<Credential> {
        creds
            .iter()
            .find(|c| candidates.iter().any(|x| c.fingerprints.contains(&x.to_lowercase())))
            .cloned()
    }

    fn ssh_sign(payload: &[u8]) -> (Credential, SshSig) {
        let key = russh::keys::PrivateKey::from_openssh(TEST_KEY).unwrap();
        let line = key.public_key().to_openssh().unwrap();
        // Through the registration helper, so the row is exactly what `add_key` writes.
        let (fp, fingerprints) = crate::api::credentials::ssh_signing_fingerprints(&line).unwrap();
        let cred = credential(format!("sign:{fp}"), String::new(), fingerprints);
        // Round-tripped through the armoured form git stores, so the parse path is exercised too.
        let pem = key.sign("git", russh::keys::HashAlg::Sha256, payload).unwrap()
            .to_pem(LineEnding::LF)
            .unwrap();
        (cred, pem.parse().unwrap())
    }

    #[test]
    fn an_ssh_signature_by_a_registered_key_is_valid() {
        let payload = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nauthor A <alice@example.com> 1 +0000\n\nmsg\n";
        let (cred, sig) = ssh_sign(payload);
        let known = lookup(&[cred], &[ssh_signature_fingerprint(&sig)]);
        let v = judge_ssh(&sig, payload, known, "alice@example.com");
        assert_eq!(v.reason_code, "valid", "{:?}", v.reason);
        assert_eq!(v.state, "verified");
        assert_eq!(v.signer.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn an_ssh_signature_by_somebody_elses_key_is_bad_email() {
        let payload = b"commit body";
        let (cred, sig) = ssh_sign(payload);
        let known = lookup(&[cred], &[ssh_signature_fingerprint(&sig)]);
        assert_eq!(judge_ssh(&sig, payload, known, "bob@example.com").reason_code, "bad_email");
    }

    #[test]
    fn an_ssh_signature_over_other_bytes_is_invalid() {
        let (cred, sig) = ssh_sign(b"what was signed");
        let known = lookup(&[cred], &[ssh_signature_fingerprint(&sig)]);
        assert_eq!(judge_ssh(&sig, b"what is claimed", known, "alice@example.com").reason_code, "invalid");
    }

    #[test]
    fn an_unregistered_ssh_key_is_unknown() {
        let (_, sig) = ssh_sign(b"x");
        assert_eq!(judge_ssh(&sig, b"x", None, "alice@example.com").reason_code, "unknown_key");
    }

    #[test]
    fn a_gpg_signature_by_a_registered_subkey_is_valid() {
        use crate::gpg::tests::{gen, reforge_subkey, subkey_signature};
        use pgp::composed::ArmorOptions;
        let now = std::time::SystemTime::now();
        let sk = gen("alice@example.com", now);
        let pk = reforge_subkey(&sk, now, Some(10 * 365 * 86400), false);
        let armored = pk.to_armored_string(ArmorOptions::default()).unwrap();
        // Registration indexes the primary AND every subkey (`fingerprints_of`), which is what
        // lets a subkey-made signature find its owner.
        let fingerprints = crate::gpg::fingerprints_of(&armored).unwrap();
        let cred = credential(format!("sign:{}", fingerprints[0]), armored, fingerprints);

        let payload = b"commit body";
        let signed = SignatureOf {
            signature: subkey_signature(&sk, payload),
            payload_base64: base64::engine::general_purpose::STANDARD.encode(payload),
            author_email: "alice@example.com".into(),
        };
        let issuers = crate::gpg::issuers(&signed.signature).unwrap();
        let v = judge_pgp(lookup(&[cred.clone()], &issuers), &signed, payload);
        assert_eq!(v.reason_code, "valid", "{:?}", v.reason);

        let other = SignatureOf { author_email: "bob@example.com".into(), ..signed };
        assert_eq!(judge_pgp(lookup(&[cred], &issuers), &other, payload).reason_code, "bad_email");
        assert_eq!(judge_pgp(None, &other, payload).reason_code, "unknown_key");
    }
}
```

`SignatureOf` needs `..` struct update syntax: it has no `Clone`, so the `other` line moves `signed` — that is fine, `signed` is not used after. If the compiler complains about the partial move, add `#[derive(Clone)]` to `SignatureOf`.

- [ ] **Step 4: Run the tests to see the ssh one fail for the real reason**

Run: `cargo test --lib api::signatures`
Expected: `an_ssh_signature_by_a_registered_key_is_valid` FAILS with `reason_code == "unknown_key"` (stored mixed-case vs. lowercased candidate). `bad_email` and `invalid` tests also fail the same way (they need the lookup to hit). `an_unregistered_ssh_key_is_unknown` and the GPG test PASS.

- [ ] **Step 5: Lowercase at registration**

In `src/api/credentials.rs` change the helper body:

```rust
pub(crate) fn ssh_signing_fingerprints(key_line: &str) -> crate::Result<(String, Vec<String>)> {
    let f = crate::store::Store::ssh_fingerprint(key_line)?;
    // Lowercased: `signer_by_any` lowercases what a signature presents and Mongo's `$in` is an
    // exact match, while `SHA256:<base64>` is mixed case. Stored as-is, no ssh signature ever
    // found its key. The id keeps the original spelling — it is only ever matched by itself.
    Ok((f.clone(), vec![f.to_lowercase()]))
}
```

Run: `cargo test --lib api::signatures`
Expected: all PASS.

- [ ] **Step 6: Backfill rows written before this fix**

In `src/directory.rs`, add a free function next to `is_duplicate_key`:

```rust
/// `Some(lowercased)` when any fingerprint has upper-case letters, `None` when the row is already
/// in the one spelling `signer_by_any` can find. Pure so the rule has a test; `connect` applies it.
pub(crate) fn lowercased(fingerprints: &[String]) -> Option<Vec<String>> {
    let lower: Vec<String> = fingerprints.iter().map(|f| f.to_lowercase()).collect();
    (lower != fingerprints).then_some(lower)
}
```

Add this method inside `impl Directory`, after `ensure_indexes`:

```rust
    /// One-shot repair for ssh signing keys registered before fingerprints were lowercased at
    /// registration (they were stored as `SHA256:<base64>`, mixed case, which `signer_by_any`
    /// can never match). Runs on every connect rather than as an admin command: it is idempotent,
    /// touches a handful of rows, and nobody has to remember to run it. Logged and swallowed by
    /// the caller — a failed repair leaves signatures unverified, which is today's behaviour, not
    /// a reason to refuse to boot.
    async fn lowercase_signing_fingerprints(&self) -> Result<usize> {
        use futures::TryStreamExt;
        let kind = mongodb::bson::to_bson(&CredentialKind::SigningKey)
            .map_err(|e| err(format!("bson: {e}")))?;
        let mut cursor = self
            .credentials
            .find(doc! { "kind": kind })
            .await
            .map_err(|e| err(format!("mongo: {e}")))?;
        let mut fixed = 0;
        while let Some(c) = cursor.try_next().await.map_err(|e| err(format!("mongo: {e}")))? {
            let Some(lower) = lowercased(&c.fingerprints) else { continue };
            self.credentials
                .update_one(doc! { "_id": &c.id }, doc! { "$set": { "fingerprints": lower } })
                .await
                .map_err(|e| err(format!("mongo: {e}")))?;
            fixed += 1;
        }
        Ok(fixed)
    }
```

and in `connect`, after `dir.ensure_indexes().await?;`:

```rust
        match dir.lowercase_signing_fingerprints().await {
            Ok(0) => {}
            Ok(n) => eprintln!("directory: lowercased {n} signing-key fingerprint rows"), // ponytail: eprintln
            Err(e) => eprintln!("directory: fingerprint repair skipped: {e}"), // ponytail: eprintln
        }
```

Add to the `tests` module at the bottom of `directory.rs`:

```rust
    #[test]
    fn lowercased_only_reports_rows_that_change() {
        use super::lowercased;
        assert_eq!(
            lowercased(&["SHA256:AbC/+=".into()]),
            Some(vec!["sha256:abc/+=".to_string()])
        );
        assert_eq!(lowercased(&["0123abcdef".into()]), None);
        assert_eq!(lowercased(&[]), None);
    }
```

Run: `cargo test --lib directory && cargo build`
Expected: PASS; builds.

- [ ] **Step 7: Full suite, then commit**

Run: `cargo test`
Expected: PASS.

```bash
git add src/api/signatures.rs src/api/credentials.rs src/directory.rs src/gpg.rs
git commit -m "Lowercase ssh signing fingerprints so signatures find their key"
```

---

## MEDIUM — security

### Task 2: Peer-only routes refuse a session token (`upsert_user`, passkey lookup/used)

**Files:**
- Modify: `src/api/mod.rs:190-223` (split `caller` → `peer_only`)
- Modify: `src/api/teams.rs:84-98` (`upsert_user`)
- Modify: `src/api/passkeys.rs:118-174` (`lookup_passkey`, `passkey_used`)
- Test: `tests/api_server.rs`

**Interfaces:**
- Produces: `pub(crate) fn peer_only(api: &Api, headers: &HeaderMap) -> Result<String, Response>` — the peer-secret-plus-asserted-identity half of `caller`, with no Bearer path.
- Consumes: `crate::proxy::secret_eq`, `PEER_HEADER`, `OWNER_HEADER`.

**Context:** `upsert_user` accepts a Bearer whose `sub` equals the body email and mints a fresh 12h token — a leaked session renews itself forever. `lookup_passkey`/`passkey_used` say "PEER ONLY" in their docs but go through `caller`, which takes any valid session: any user can read another's passkey public key/email and set their `counter` (breaking the victim's next login via clone detection). All three are sign-in plumbing; only the web app (holding the peer secret) may call them.

- [ ] **Step 1: Write the failing test**

Append to `tests/api_server.rs`:

```rust
/// The three routes that exist to CREATE a session must not be reachable WITH one: a leaked
/// session token could otherwise renew itself forever (`/v1/users`) or read and corrupt another
/// person's passkey (`lookup`, `used`). Only the web app, holding the peer secret, may call them.
#[tokio::test(flavor = "multi_thread")]
async fn peer_only_routes_refuse_a_session_token() {
    let up = upstream(axum::http::StatusCode::OK).await;
    let e = common::env().await;
    let secret = "0123456789012345678901234567890123456789";
    let base = api_with_jwt(&e, &up, secret).await;
    let token = rustic_git::jwt::Jwt::new(secret)
        .unwrap()
        .mint("alice@example.com", "Alice", Some("alice"))
        .unwrap();
    let c = reqwest::Client::new();
    for (path, body) in [
        ("/v1/users", r#"{"email":"alice@example.com","name":"Alice"}"#),
        ("/v1/passkeys/lookup", r#"{"id":"abc"}"#),
        ("/v1/passkeys/abc/used", r#"{"counter":7}"#),
    ] {
        let r = c
            .post(format!("{base}{path}"))
            .bearer_auth(&token)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "{path} accepted a session token");
        // The peer path still reaches the handler. This api has no directory, so the handler's
        // own answer is 503 — which is the proof the gate let the right caller through.
        let r = c
            .post(format!("{base}{path}"))
            .header(rustic_git::proxy::PEER_HEADER, "s")
            .header(rustic_git::proxy::OWNER_HEADER, "alice@example.com")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 503, "{path} refused the peer");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test api_server peer_only_routes_refuse_a_session_token`
Expected: FAIL — `/v1/users` with a Bearer answers 503 (the session was accepted and the handler reached the missing directory), not 401.

- [ ] **Step 3: Split `caller` in `src/api/mod.rs`**

Replace `caller` (lines 190-223) with:

```rust
/// Who is asking.
///
/// A signed token first: it proves the identity by itself, so no trust in the
/// caller is required. The peer secret plus an asserted identity is the fallback
/// for service-to-service calls that have no user token yet — notably sign-in,
/// which is where a token comes FROM.
pub(crate) fn caller(api: &Api, headers: &axum::http::HeaderMap) -> std::result::Result<String, Response> {
    if let Some(bearer) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        let jwt = api
            .jwt
            .as_deref()
            .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "tokens not configured").into_response())?;
        return match jwt.verify(bearer.trim()) {
            Ok(c) => Ok(c.sub),
            // Never say which of signature, algorithm or expiry failed.
            Err(_) => Err((StatusCode::UNAUTHORIZED, "invalid or expired token").into_response()),
        };
    }
    peer_only(api, headers)
}

/// The peer half of `caller`, on its own: the peer secret plus the identity the peer asserts,
/// and NO Bearer path. For the routes that mint or precede a session — sign-in, passkey lookup,
/// the passkey counter — a session must not be enough, or a leaked token renews itself forever
/// and any signed-in person can read or corrupt another's passkey. A Bearer header is simply not
/// looked at here: a caller that also presents the peer secret is the web app, and it is the
/// secret that admits it.
pub(crate) fn peer_only(api: &Api, headers: &axum::http::HeaderMap) -> std::result::Result<String, Response> {
    let peer = headers
        .get(crate::proxy::PEER_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !crate::proxy::secret_eq(peer, &api.secret) {
        return Err((StatusCode::UNAUTHORIZED, "peer secret required").into_response());
    }
    match headers.get(crate::proxy::OWNER_HEADER).and_then(|v| v.to_str().ok()) {
        Some(u) if !u.trim().is_empty() => Ok(u.trim().to_string()),
        _ => Err((StatusCode::BAD_REQUEST, "caller identity required").into_response()),
    }
}
```

- [ ] **Step 4: Use it on the three routes**

`src/api/teams.rs` `upsert_user`, replace lines 89-95 with:

```rust
    // Peer only: this route MINTS a session, so a session must not be able to call it — a leaked
    // token would otherwise renew itself for as long as the holder likes. The peer's assertion of
    // who signed in must still agree with the body, or a caller holding the secret could mint any
    // identity it likes.
    let asserted = match peer_only(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
```

`src/api/passkeys.rs`: in both `lookup_passkey` (line 130) and `passkey_used` (line 160) replace `if let Err(r) = caller(&api, &headers) {` with `if let Err(r) = peer_only(&api, &headers) {`. Update the doc comment on `lookup_passkey` (lines 118-124) to:

```rust
/// Whose passkey is this, and what verifies it?
///
/// PEER ONLY, enforced by `peer_only` rather than merely documented: it is called during
/// sign-in, when there is no session yet, and a session must not be enough — a credential id
/// maps to an email and a public key, which is another person's to keep. Only the web app,
/// holding the peer secret, can ask.
```

- [ ] **Step 5: Run test + suite**

Run: `cargo test --test api_server && cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/api/mod.rs src/api/teams.rs src/api/passkeys.rs tests/api_server.rs
git commit -m "Refuse session tokens on the peer-only sign-in routes"
```

---

## MEDIUM — bugs

### Task 3: `.git`-suffixed pull reads must not conjure a ghost database

**Files:**
- Modify: `src/http/browse_api/pulls.rs:87-127` (`api_pulls`, `api_pull`)
- Test: `tests/browse_http.rs`

**Context:** Both read handlers call `open_ro` (which parses `web.git` → `web` internally) and then hand the RAW path `name` to `ready()`, which opens `repo/alice/web.git` — a database under a key routing never names, on whatever node got the request. `open_ro` already returns the parsed `Repo`; use it.

- [ ] **Step 1: Write the failing test**

Append to `tests/browse_http.rs` (after `a_private_repos_pulls_are_invisible_to_a_stranger`):

```rust
/// Catches: `api_pulls`/`api_pull` handing the RAW path name to `ready()`, which opened a ghost
/// database `repo/alice/widget.git` — a key no routing ever names, on whichever node got asked.
#[tokio::test(flavor = "multi_thread")]
async fn a_dot_git_suffix_browse_read_creates_no_ghost_database() {
    let e = common::env().await;
    let router = rustic_git::http::peer_router(common::app(e.store.clone()).await);
    assert_eq!(post_as(&router, "alice", "/api/alice/widget/create").await, StatusCode::CREATED);
    let (s, list) = get_as(&router, "alice", "/api/alice/widget.git/pulls").await;
    assert_eq!(s, StatusCode::OK, "{list}");
    let (s, _) = get_as(&router, "alice", "/api/alice/widget.git/pulls/1").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(
        !e.store.repo_db_exists("alice", "widget.git").await.unwrap(),
        "a `.git`-suffixed read conjured a database under an unrouted key"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test browse_http a_dot_git_suffix_browse_read_creates_no_ghost_database`
Expected: FAIL on the last assertion — `repo/alice/widget.git` exists.

- [ ] **Step 3: Use the parsed repo**

In `api_pulls` replace lines 93-99 with:

```rust
    // The PARSED repo, not the raw path: `open_ro` strips `.git`, and `ready` opens whatever name
    // it is handed — the raw one would conjure `repo/alice/web.git`, a database under a key no
    // routing ever names.
    let repo = match open_ro(&app, &trusted, &headers, &owner, &name).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    let db = match ready(&app, &repo.owner, &repo.name).await {
        Ok(d) => d,
        Err(r) => return r,
    };
```

Same edit in `api_pull` (lines 115-121), without repeating the comment — a one-liner `// Parsed, not raw: see `api_pulls`.` is enough.

- [ ] **Step 4: Run tests**

Run: `cargo test --test browse_http`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/http/browse_api/pulls.rs tests/browse_http.rs
git commit -m "Open pull reads on the parsed repo name, not the raw path"
```

---

### Task 4: One fence-retry helper for HTTP `open`, SSH and the peer stream

**Files:**
- Modify: `src/lib.rs` (add `App::open_repo_after_fence` next to `on_fenced`, ~line 771)
- Modify: `src/http.rs:699-717` (`open` uses it — this is the spec's "http.rs:702 inline reopen" item)
- Modify: `src/ssh.rs:221-225` (`run`)
- Modify: `src/proxy.rs:297-308` (`serve_peer_stream`)
- Test: `tests/ssh_e2e.rs`

**Interfaces:**
- Produces: `pub async fn App::open_repo_after_fence(&self, owner: &str, name: &str) -> Result<Option<Repo>>`.
- Consumes: `App::on_fenced`, `Store::open_repo`, `pool::is_fenced`.

**Context:** HTTP `open` retries a fenced open once when routing says this node still owns the repo; SSH (`ssh.rs:221`) and the forwarded stream (`proxy.rs:297`) do not, so a stray fence makes SSH fail until an HTTP request happens to evict the handle. Same fix in three places = one helper on `App`. (`http::reopen_after_fence` stays: it is the *run-protocol* retry, a different trigger.)

- [ ] **Step 1: Write the failing test**

Append to `tests/ssh_e2e.rs` (before `gen_host_key`):

```rust
/// The SSH twin of `a_node_fenced_by_a_stray_process_reopens_when_it_is_still_the_owner` in
/// routing.rs: a stray opener fenced this node's handle, routing still says the repo is ours, and
/// the next SSH session must reopen and serve rather than fail until some HTTP request evicts.
#[tokio::test(flavor = "multi_thread")]
async fn ssh_serves_after_a_stray_fence_when_still_the_owner() {
    if !common::have_git()
        || std::process::Command::new("ssh").arg("-V").output().is_err()
    {
        eprintln!("skip: git/ssh missing");
        return;
    }
    let e = common::env().await;
    let s = e.store.clone();
    s.create_repo("alice", "proj").await.unwrap();

    let kd = tempfile::tempdir().unwrap();
    let key = kd.path().join("id_ed25519");
    assert!(std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f", key.to_str().unwrap()])
        .status()
        .unwrap()
        .success());
    let pubkey = std::fs::read_to_string(kd.path().join("id_ed25519.pub")).unwrap();
    s.add_ssh_key("alice", &pubkey).await.unwrap();

    let host_key = gen_host_key(&kd);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let app = common::app(s.clone()).await;
    tokio::spawn(async move { rustic_git::ssh::serve(app, l, host_key).await.unwrap() });

    // This node holds the repo; a stray opener takes the writer epoch out from under it.
    let held = s.pool.get("alice", "proj").await.unwrap();
    let stray = slatedb::Db::builder(rustic_git::pool::path("alice", "proj"), s.os.clone())
        .build()
        .await
        .unwrap();
    stray.put(b"k", b"v").await.unwrap();
    {
        let mut st = held.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while st.borrow().close_reason.is_none() {
                st.changed().await.unwrap();
            }
        })
        .await
        .expect("the held handle must observe the fence");
    }
    drop(held);
    stray.close().await.unwrap();

    let ssh_cmd = format!(
        "ssh -i {} -p {port} -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes",
        key.display()
    );
    let url = format!("ssh://git@127.0.0.1:{port}/alice/proj.git");
    let out = std::process::Command::new("git")
        .args(["ls-remote", &url])
        .env("GIT_SSH_COMMAND", &ssh_cmd)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ssh after a stray fence: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(s.pool.warm_count(), 1, "reopened, not left fenced");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test ssh_e2e ssh_serves_after_a_stray_fence_when_still_the_owner`
Expected: FAIL — stderr carries `rustic-git: alice/proj fenced` and git exits non-zero. (If `ssh`/`git` are missing the test skips; run it on a machine that has them — this is the only coverage.)

- [ ] **Step 3: Add the helper on `App` in `src/lib.rs`** (directly after `on_fenced`)

```rust
    /// `open_repo`, retried once when the first attempt hits a fence that routing says this node
    /// may still own (see `on_fenced`). The one place that rule lives, so HTTP, SSH and the peer
    /// stream cannot drift: SSH did not retry at all, and a stray fence made it fail until some
    /// HTTP request happened to evict the handle. A fence this node must honour comes back as the
    /// original error for the caller to report.
    pub async fn open_repo_after_fence(&self, owner: &str, name: &str) -> Result<Option<store::Repo>> {
        match self.store.open_repo(owner, name).await {
            Err(e) if pool::is_fenced(&e) && self.on_fenced(owner, name).await => {
                self.store.open_repo(owner, name).await
            }
            r => r,
        }
    }
```

- [ ] **Step 4: Use it in the three callers**

`src/ssh.rs` lines 221-225:

```rust
    let repo = app
        .open_repo_after_fence(&owner, &name)
        .await?
        .ok_or_else(|| crate::err("repository not found"))?;
```

`src/proxy.rs` line 297: `let repo = match app.open_repo_after_fence(&ro, &rn).await {` — the three arms below it stay as they are (a fence that survives the retry is still reported as "repository moved; retry").

`src/http.rs` lines 699-717, replace the `Ok(Some)`, `Ok(None)` and `Err(e) if is_fenced` arms with:

```rust
    match app.open_repo_after_fence(&owner, &name).await {
        Ok(Some(repo)) => Ok(repo),
        Ok(None) => Err((StatusCode::NOT_FOUND, "repository not found").into_response()),
        // Routing said another node owns it (or it fenced again): 503 so the client retries
        // against the owner.
        Err(e) if crate::pool::is_fenced(&e) => Err(fenced_elsewhere()),
        Err(e) => {
            // ... the existing release-the-lease arm, unchanged ...
```

- [ ] **Step 5: Run tests**

Run: `cargo test --test ssh_e2e && cargo test --test routing && cargo test`
Expected: PASS (routing.rs's two stray-fence tests still pass — they exercise the HTTP path through the same helper now).

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/http.rs src/ssh.rs src/proxy.rs tests/ssh_e2e.rs
git commit -m "Retry a fenced open once on SSH and the peer stream, as HTTP does"
```

---

### Task 5: Require `RUSTIC_GIT_REPLICAS` in fleet mode

**Files:**
- Modify: `src/main.rs:104-107`

**Context:** In fleet mode (`RUSTIC_GIT_PEER_SVC` set) a missing `RUSTIC_GIT_REPLICAS` silently defaults to 1, so the leader hands every repo to `srv-0`. Both StatefulSets in `deploy/rustic-git.yaml` already set it (lines 128 and 345), so requiring it breaks nothing deployed. No test: `serve()` binds real sockets; the check is a four-line `match` and `cargo build` is the gate.

- [ ] **Step 1: Replace the default**

```rust
    // Required with a fleet: defaulting to 1 made the leader hand every repo to `srv-0`, silently,
    // on any pod whose env lost the variable. Solo mode has nobody else to hand a repo to, so 1.
    let replicas: u32 = match std::env::var("RUSTIC_GIT_REPLICAS").ok().filter(|v| !v.is_empty()) {
        Some(v) => v
            .parse()
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| rustic_git::err("RUSTIC_GIT_REPLICAS must be a positive integer"))?,
        None if svc.is_empty() => 1,
        None => {
            return Err(rustic_git::err(
                "RUSTIC_GIT_REPLICAS is required with RUSTIC_GIT_PEER_SVC (the leader hands repos \
                 to rustic-git-srv-{0..N-1})",
            ))
        }
    };
```

- [ ] **Step 2: Build, then commit**

Run: `cargo build && cargo test --bin rustic-git`
Expected: builds; main's tests pass.

```bash
git add src/main.rs
git commit -m "Require RUSTIC_GIT_REPLICAS when a peer Service is configured"
```

---

### Task 6: Stop holding a std mutex across `.await` in main's tests

**Files:**
- Modify: `src/main.rs:720,745,775` (`ENV_LOCK`)

**Context:** Clippy `await_holding_lock` ×2. The guard serialises two tests that mutate process env; a `tokio::sync::Mutex` does the same job and may be held across awaits. `const_new` keeps it a `static`.

- [ ] **Step 1: Swap the mutex**

Line 720: `static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());`

Lines 745 and 775: `let _guard = ENV_LOCK.lock().await;`

- [ ] **Step 2: Verify the warning is gone, then commit**

Run: `cargo clippy --all-targets 2>&1 | grep -c await_holding_lock` → `0`; then `cargo test --bin rustic-git`.

```bash
git add src/main.rs
git commit -m "Hold the env lock in main's tests with an async mutex"
```

---

### Task 7: A poisoned auth cache must not panic every request

**Files:**
- Modify: `src/auth.rs:39-130` (all four `auth_cache.lock().unwrap()`), `src/store.rs:87-89`
- Test: `src/auth.rs` tests

**Context:** `Mutex::lock().unwrap()` on the credential cache: one panic while holding it (a bug anywhere) turns every later authentication into a panic. The map holds nothing a half-finished insert can corrupt, so `into_inner` is correct.

- [ ] **Step 1: Write the failing test** (in `src/auth.rs` `mod tests`)

```rust
    /// One panic while holding the cache lock — a bug anywhere — must not turn every later
    /// authentication into a panic.
    #[tokio::test]
    async fn a_poisoned_auth_cache_does_not_panic_every_request() {
        let os = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(os, dir.path().to_path_buf(), false).await.unwrap());
        let token = store.create_token("alice").await.unwrap();
        let s = store.clone();
        let _ = std::thread::spawn(move || {
            let _g = s.auth_cache.lock().unwrap();
            panic!("poison the lock on purpose");
        })
        .join();
        assert!(store.auth_cache.is_poisoned());
        assert_eq!(store.owner_for_token(&token).await.unwrap().as_deref(), Some("alice"));
        store.revoke_token_digest(&Store::token_digest(&token)).await.unwrap();
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib a_poisoned_auth_cache_does_not_panic_every_request`
Expected: FAIL — panics on `lock().unwrap()` with `PoisonError`.

- [ ] **Step 3: One accessor, used everywhere**

In `src/auth.rs` inside `impl Store`, first method:

```rust
    /// The credential cache, poisoning ignored: a panic while the lock was held (a bug somewhere
    /// else) must not turn every later authentication into a panic, and the map holds nothing a
    /// half-finished insert can leave inconsistent.
    fn auth_cache(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, (Instant, Option<String>)>> {
        self.auth_cache.lock().unwrap_or_else(|p| p.into_inner())
    }
```

Replace the four `self.auth_cache.lock().unwrap()` in `lookup`, `revoke_token_digest`, `remove_ssh_key` with `self.auth_cache()`. In `src/store.rs:88` make `auth_cache_len` `self.auth_cache().len()` — `auth_cache()` is private to `auth.rs`'s `impl Store` block, which is the same type, so `pub(crate)` it: change `fn auth_cache` to `pub(crate) fn auth_cache`.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib auth`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/auth.rs src/store.rs
git commit -m "Recover the auth cache from a poisoned lock instead of panicking"
```

---

### Task 8: Cap the repo description

**Files:**
- Modify: `src/api/repos.rs` (`create_repo` line ~145, `update_repo` line ~308)
- Test: `src/api/repos.rs` tests

**Context:** The description travels to the owning node as a query parameter. A 2 MiB JSON body becomes a 6 MiB URL and an opaque 502. Cap it where the JSON is parsed.

- [ ] **Step 1: Write the failing test** (append inside the `tests` module in `repos.rs`)

```rust
    #[test]
    fn a_description_past_the_cap_is_refused_before_it_becomes_a_url() {
        assert!(check_description(&"x".repeat(MAX_DESCRIPTION)).is_ok());
        assert!(check_description(&"x".repeat(MAX_DESCRIPTION + 1)).is_err());
        // Counted in characters, not bytes: a 300-character non-ASCII blurb is a blurb.
        assert!(check_description(&"é".repeat(MAX_DESCRIPTION)).is_ok());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib a_description_past_the_cap`
Expected: FAIL — `check_description`/`MAX_DESCRIPTION` undefined.

- [ ] **Step 3: Implement**

Above `NewRepo` in `repos.rs`:

```rust
/// A description is a line under the repo name, not a README. The cap is what keeps it a
/// query parameter: the owning node takes it in the URL, and a 2 MiB body became a 6 MiB URL and
/// an opaque 502.
pub(crate) const MAX_DESCRIPTION: usize = 512;

pub(crate) fn check_description(d: &str) -> std::result::Result<(), Response> {
    if d.chars().count() > MAX_DESCRIPTION {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("description must be {MAX_DESCRIPTION} characters or fewer"),
        )
            .into_response());
    }
    Ok(())
}
```

In `create_repo`, right after the `reserved_repo_name` check: `if let Err(r) = check_description(body.description.trim()) { return r; }`.
In `update_repo`, before the `if let Some(p) = public` block: `if let Some(d) = body.description.as_deref() { if let Err(r) = check_description(d) { return r; } }` — before the visibility flip, so a bad request changes nothing.

- [ ] **Step 4: Run tests, commit**

Run: `cargo test --lib api::repos`
Expected: PASS.

```bash
git add src/api/repos.rs
git commit -m "Cap repo descriptions before they become a URL"
```

---

### Task 9: Every upstream body read goes through `read_bounded`

**Files:**
- Modify: `src/api/forward.rs:4` (`pub`), `:92` (`tell_owner`); `src/api/mod.rs:36` (re-export); `src/api/signatures.rs:99` (`commit_patch`); `src/api/feed.rs:22` (`feed_get`); `src/main.rs:631`

**Context:** `read_bounded` exists (`MAX_BODY` = 8 MiB) and is already used by `read_from_owner`, `list_protection`, `verify_commit` and `handle`; four sites still call `.text()` unbounded. `main.rs` is a separate binary, so the helper becomes `pub` and is re-exported from `api`.

- [ ] **Step 1: Make it public**

`src/api/forward.rs:4`: `pub async fn read_bounded(...)`. `src/api/mod.rs`, after the `use forward::*;` line: `pub use forward::read_bounded;`.

- [ ] **Step 2: Replace the four reads**

A small helper keeps the call sites one line. Add to `forward.rs`:

```rust
/// `read_bounded`, as the text a handler relays. An oversized reply is an empty string, which the
/// relaying status code already explains better than a truncated body would.
pub(crate) async fn text_bounded(r: reqwest::Response) -> String {
    read_bounded(r)
        .await
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}
```

- `forward.rs:92` (`tell_owner`): `let text = text_bounded(r).await;`
- `signatures.rs:99` (`commit_patch`): `let text = text_bounded(r).await;`
- `feed.rs:22` (`feed_get`): `read_bounded(res).await.ok().map(|b| String::from_utf8_lossy(&b).into_owned())`
- `main.rs:631`: `let body = rustic_git::api::read_bounded(res).await.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();`

- [ ] **Step 3: Build + suite, commit**

Run: `cargo build && cargo test --test api_server && cargo test`
Expected: PASS.

```bash
git add src/api/forward.rs src/api/mod.rs src/api/signatures.rs src/api/feed.rs src/main.rs
git commit -m "Bound every upstream body read"
```

---

### Task 10: Fixed messages in 500 bodies (browse admin + merge)

**Files:**
- Modify: `src/http/browse_api/admin.rs:68,154,168,200`, `src/http/browse_api/merge.rs:96-97`

**Context:** `e.to_string()` in a 500 body echoes Redis/SlateDB error text to the caller; `merge`'s `boom` surfaces in the PR UI. `internal(e)` already logs and answers a fixed `"internal error"`. The one message worth keeping is the visibility flip's retry instruction — keep the instruction, drop the raw error.

- [ ] **Step 1: `admin.rs`**

- Line 66-69 (`api_visibility` error arm):

```rust
        Err(e) => {
            // The flag is written; only the cache bump can have failed. The operator's next step
            // is fixed text — the backend's own words stay in the log.
            eprintln!("set-visibility {owner}/{name}: {e}"); // ponytail: eprintln
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("visibility changed but cached answers may be stale; retry with `admin purge-cache {owner}/{name}`"),
            )
                .into_response()
        }
```

- Line 154 and 168 (`api_create`): replace `return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();` with `return internal(e);` (the preceding `eprintln!` lines become redundant — `internal` logs; delete them).
- Line 200 (`api_description`): same, `internal(e)`; delete the `eprintln!` above it.

- [ ] **Step 2: `merge.rs` `perform`**

Replace line 97 with a logging `boom` that never forwards the text (it must be defined AFTER `parse_repo_path` so it can name the repo — move it below line 100):

```rust
    let boom = |e: crate::Error| {
        eprintln!("merge {owner}/{name}: {e}"); // ponytail: eprintln
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
    };
```

- [ ] **Step 3: Build, test, commit**

Run: `cargo build && cargo test --test browse_http`
Expected: PASS.

```bash
git add src/http/browse_api/admin.rs src/http/browse_api/merge.rs
git commit -m "Answer 500s with fixed text instead of backend error strings"
```

---

### Task 11: Run odb work in `merge`/`patch` on a blocking thread

**Files:**
- Modify: `src/http/browse_api/merge.rs:116-157` (`perform`), `:286-329` (`api_patch`)

**Context:** `repo.odb()`, `merge_base` (a 50k-commit walk), `find_commit` and `apply_changes` run synchronously on the async runtime, starving every other request on that worker. `odb_json` in `mod.rs` already shows the pattern: `spawn_blocking(move || repo.odb().map(|odb| f(&odb)))`. `Repo` is `Clone`, `Staging` is plain `Vec`s — both move into a closure. No new test: the existing merge/patch tests in `tests/browse_http.rs` and `tests/pulls.rs` pin behaviour; this is a scheduling change.

- [ ] **Step 1: `perform`** — replace lines 116-157 (from `let odb = match repo.odb()` through the `let time = ...` line, keeping `parents`/`message`/`write_commit` below) with:

```rust
    // Everything that touches the odb — the ancestry walk and the head commit's fields — runs
    // on a blocking thread: `merge_base` is a 50k-commit walk, and doing it on the runtime
    // starves every other request on that worker. `odb_json` makes the same move for reads.
    struct HeadInfo {
        tree: gix_hash::ObjectId,
        who: String,
        mail: String,
        time: i64,
    }
    let need_head = matches!(strategy, "squash" | "merge");
    let r = repo.clone();
    let walked = tokio::task::spawn_blocking(move || -> crate::Result<(bool, Option<HeadInfo>)> {
        let odb = r.odb()?;
        // Re-checked HERE rather than trusted from whatever the caller last read: the branch may
        // have moved since the page was rendered.
        if crate::browse::merge_base(&odb, base_oid, head_oid, 50_000) != Some(base_oid) {
            return Ok((true, None));
        }
        if !need_head {
            return Ok((false, None));
        }
        let mut buf = Vec::new();
        let c = gix_object::FindExt::find_commit(&odb, &head_oid, &mut buf)
            .map_err(|e| crate::err(e.to_string()))?;
        let author = c.author().ok();
        let (who, mail) = match &author {
            Some(a) => (a.name.to_string(), a.email.to_string()),
            None => ("kloudlite".to_string(), "noreply@kloudlite.io".to_string()),
        };
        // The commit time comes from the head commit, not the clock, so merging the same branch
        // twice produces the same id — which is what makes a retried merge idempotent.
        let time = author.as_ref().and_then(|a| a.time().ok()).map(|t| t.seconds).unwrap_or(0);
        Ok((false, Some(HeadInfo { tree: c.tree(), who, mail, time })))
    })
    .await;
    // `head_info`, not `head`: `head` is the branch name and is still needed for the message.
    let (behind, head_info) = match walked {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(boom(e)),
        Err(e) => return Err(boom(crate::err(format!("merge task: {e}")))),
    };
    if behind {
        return Err(bad(StatusCode::CONFLICT, "this branch is behind its base — rebase it and push again"));
    }

    // Which shape to land it in. All three are safe HERE and only here: the base is an ancestor
    // of the head, so the content being landed is exactly the head's tree and no three-way merge
    // is possible or needed. On a diverged branch these would each need a real merge, which is
    // why that case is refused above rather than guessed at.
    let new_tip = match strategy {
        // The ref simply moves; no new object.
        "fast-forward" | "rebase" => head_oid,
        "squash" | "merge" => {
            let Some(HeadInfo { tree, who, mail, time }) = head_info else {
                return Err(boom(crate::err("head commit not read")));
            };
            let parents = if strategy == "squash" { vec![base_oid] } else { vec![base_oid, head_oid] };
            let message = message.unwrap_or_else(|| format!("Merge {head} into {base}\n"));
            match crate::objects::write_commit(
                &app.store,
                &repo,
                crate::objects::NewCommit { tree, parents, message, author_name: who, author_email: mail, time },
            )
            .await
            {
                Ok(oid) => oid,
                Err(e) => return Err(boom(e)),
            }
        }
        _ => {
            return Err(bad(StatusCode::BAD_REQUEST, "strategy must be fast-forward, squash, merge or rebase"))
        }
    };
```

The `boom` closure from Task 10 borrows `owner`/`name`; it is only called outside the blocking closure, so this compiles. The `let update = vec![...]` / `update_refs` tail of `perform` is unchanged.

- [ ] **Step 2: `api_patch`** — replace lines 286-329 (from `let odb = match repo.odb()` through the `if tree == base_tree` check) with:

```rust
    // The base tree, the staged blobs/trees and the "did anything change" answer all need the
    // odb; one blocking task does the three together. `apply_changes`' refusals are the
    // caller's to see (a path that is a directory, a missing parent), so they come back as a
    // message for a 400 rather than as a fault.
    let r = repo.clone();
    let staged = tokio::task::spawn_blocking(move || -> crate::Result<(gix_hash::ObjectId, std::result::Result<(gix_hash::ObjectId, crate::objects::Staging), String>)> {
        let odb = r.odb()?;
        let mut buf = Vec::new();
        let base_tree = gix_object::FindExt::find_commit(&odb, &tip, &mut buf)
            .map_err(|e| crate::err(e.to_string()))?
            .tree();
        let mut staging = crate::objects::Staging::default();
        let applied = crate::objects::apply_changes(&odb, Some(base_tree), &changes, &mut staging)
            .map(|t| (t, staging))
            .map_err(|e| e.to_string());
        Ok((base_tree, applied))
    })
    .await;
    let (base_tree, tree, staging) = match staged {
        Ok(Ok((base_tree, Ok((tree, staging))))) => (base_tree, tree, staging),
        Ok(Ok((_, Err(why)))) => return (StatusCode::BAD_REQUEST, why).into_response(),
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(crate::err(format!("patch task: {e}"))),
    };
    // Nothing actually changed: the same bytes were sent back. A commit here would be an empty
    // one, which is noise in the history rather than a record.
    if tree == base_tree {
        return (StatusCode::BAD_REQUEST, "this changes nothing").into_response();
    }
```

The `changes` map must be built BEFORE this block (it already is — the `for c in patch.changes` loop stays above; move this block to just after it). `staging.write(...)` and everything below stay as they are.

- [ ] **Step 3: Build, run the merge/patch tests, commit**

Run: `cargo build && cargo test --test browse_http && cargo test --test pulls && cargo test`
Expected: PASS.

```bash
git add src/http/browse_api/merge.rs
git commit -m "Run merge and patch odb work on a blocking thread"
```

---

### Task 12: Read image tag rows concurrently

**Files:**
- Modify: `src/http/browse_api/images.rs:101-128` (`imagetags`)

**Context:** Four sequential round trips per tag (tag row, HEAD, GET, pull count) — a 100-tag image is 400 serial object-store calls. `futures` is a dependency. `buffered(8)` keeps the tag order `store.tags` returns (the spec suggests `buffer_unordered`; ordered output with the same parallelism is the smaller change for the page that renders this).

- [ ] **Step 1: Rewrite the loop**

```rust
    // One future per tag, eight in flight: the four reads per tag are independent of every other
    // tag's, and a 100-tag image was 400 serial round trips. `buffered`, not `buffer_unordered`:
    // the page shows them in `tags`' order and re-sorting would cost what it saved.
    use futures::StreamExt;
    let out: Vec<ImageTag> = futures::stream::iter(tags)
        .map(|tag| {
            let (app, owner, name) = (app.clone(), owner.clone(), name.clone());
            async move {
                let d = app.store.tag(&owner, &name, &tag).await.unwrap_or(None)?;
                // The manifest's own bytes, not a maintained size field: nothing writes one, and
                // asking the object store directly can never disagree with what was pushed.
                let path = crate::registry::store::manifest_path(&owner, &name, &d);
                let meta = app.store.os.head(&path).await.ok();
                let size = meta.as_ref().map(|m| m.size).unwrap_or(0);
                let pushed_ms = meta.as_ref().map(|m| m.last_modified.timestamp_millis());
                // Reading the manifest to ADD UP its declared sizes — never to re-emit it. The
                // digest is over the exact bytes, so nothing here may write a manifest back.
                let bytes = match app.store.os.get(&path).await {
                    Ok(r) => r.bytes().await.map(|b| declared_size(&b)).unwrap_or(0),
                    Err(_) => 0,
                };
                let pulls = app.store.pulls(&owner, &name, &tag).await.unwrap_or(0);
                Some(ImageTag { tag, digest: d.to_string(), size, bytes, pushed_ms, pulls })
            }
        })
        .buffered(8)
        .filter_map(|t| async move { t })
        .collect()
        .await;
    Json(out).into_response()
```

- [ ] **Step 2: Build, run the registry HTTP tests, commit**

Run: `cargo build && cargo test --test registry_http && cargo test --test browse_http`
Expected: PASS (the `imagetags` tests there pin the row shape and order).

```bash
git add src/http/browse_api/images.rs
git commit -m "Read image tag rows eight at a time"
```

---

### Task 13: Make `claim_username` a conditional update

**Files:**
- Modify: `src/directory.rs:379-406`

**Context:** Check-then-reserve: two requests for the same user racing past `existing.username.is_none()` both reserve a handle; the second's `$set` overwrites the first and the first handle is held by nobody forever. Filter the `$set` on `username` still being absent, and release the reservation when it matched nothing. Needs Mongo to test; `cargo build` is the gate.

- [ ] **Step 1: Condition the write**

Replace lines 393-405 with:

```rust
        // Conditional on the handle still being unset: two claims for one user can both pass the
        // read above, and an unconditional `$set` would let the second overwrite the first, whose
        // reservation is then held by nobody forever. Zero matched means somebody won first.
        let set = self
            .users
            .update_one(
                doc! { "_id": &email, "username": { "$exists": false } },
                doc! { "$set": { "username": &handle } },
            )
            .await;
        match set {
            Ok(r) if r.matched_count == 1 => self.user(&email).await,
            Ok(_) => {
                let _ = self.release(&handle).await;
                Err(err("username already set"))
            }
            Err(e) => {
                // Compensate, or the handle is reserved for a user who does not carry it —
                // unclaimable by anyone, forever.
                let _ = self.release(&handle).await;
                Err(err(format!("mongo: {e}")))
            }
        }
```

Update the doc comment above `claim_username` (lines 373-378): replace the last sentence with "The write itself is conditional on the username still being absent, so two claims by one person cannot both land; the loser gives its reservation back."

- [ ] **Step 2: Build, commit**

Run: `cargo build && cargo test --lib directory`

```bash
git add src/directory.rs
git commit -m "Claim a username with a conditional update"
```

---

### Task 14: `admin set-image-visibility` posts to the routed endpoint

**Files:**
- Modify: `src/main.rs:381-404` (`fleet_guard` doc), `:588-666` (both visibility commands), tests at `:766-808`

**Context:** The comment at `main.rs:634-666` says there is no routed image-visibility endpoint; `imagevisibility` (`browse_api/images.rs:267`) exists and routes by the image key. So with a fleet configured the command can deliver the flip exactly as `set-visibility` does, instead of refusing. `imagevisibility` authorizes `caller == owner`, which on the peer listener is `Trusted(Some(OWNER_HEADER))` — so the POST must carry the owner header (harmless on the repo route, which ignores it).

- [ ] **Step 1: Update the test**

In `set_image_visibility_writes_it`, replace the final block (from `// An upstream configured but no secret in this shell: still must refuse` to the `remove_var`) with:

```rust
        // An upstream configured but no secret in this shell: must go to the fleet (the routed
        // `imagevisibility` endpoint) and fail loudly when it cannot reach it — never write here.
        std::env::set_var("RUSTIC_GIT_UPSTREAM", "http://127.0.0.1:1");
        let e = run(&["admin", "set-image-visibility", "acme/nginx", "public"], &store)
            .await
            .expect_err("an unreachable fleet must fail, not fall back to a direct write");
        assert!(!store.image_is_public("acme", "nginx").await.unwrap(), "nothing written here: {e}");
        assert!(e.to_string().contains("set-image-visibility"), "{e}");
        std::env::remove_var("RUSTIC_GIT_UPSTREAM");
```

Also update that test's doc comment: drop "Unlike `set-visibility` there's no routed image endpoint, so "fleet configured" means refuse, not redirect" and say it mirrors `set-visibility` exactly.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin rustic-git set_image_visibility_writes_it`
Expected: FAIL — the error text is the "no routed endpoint ... refusing" refusal, and it contains "set-image-visibility" — so make the assertion precise: `assert!(!e.to_string().contains("no routed endpoint"), "{e}")`. With that line the test fails before and passes after.

- [ ] **Step 3: Factor the POST out of `set-visibility` and use it twice**

Add above `fn run`:

```rust
/// Deliver a flip to the node that owns `path`'s database: POST it to the peer Service and let
/// the `route` middleware carry it. Carries the owner as the peer identity because
/// `imagevisibility` authorizes on it (the repo route ignores it). A peer that accepts and never
/// answers must not hang the command forever, so the call is bounded like the api's upstream calls.
async fn post_to_owner(
    cmd: &str,
    owner: &str,
    route: &str,
    upstream: Option<String>,
    secret: Option<String>,
) -> Result<()> {
    let upstream = upstream.unwrap_or_else(|| "http://rustic-git:8081".into());
    let res = reqwest::Client::builder()
        .timeout(rustic_git::api::UPSTREAM_TIMEOUT)
        .build()?
        .post(format!("{}{route}", upstream.trim_end_matches('/')))
        .header(rustic_git::proxy::PEER_HEADER, secret.unwrap_or_default())
        .header(rustic_git::proxy::OWNER_HEADER, owner)
        .send()
        .await
        .map_err(|e| rustic_git::err(format!("{cmd}: {e}")))?;
    let status = res.status();
    if status.is_success() {
        return Ok(());
    }
    let body = rustic_git::api::read_bounded(res)
        .await
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    Err(rustic_git::err(format!("{cmd}: {status}: {body}")))
}
```

In `set-visibility`, replace everything from `let upstream = upstream.unwrap_or_else(...)` to the end of the arm with:

```rust
            post_to_owner("set-visibility", o, &format!("/api/{o}/{n}/visibility?visibility={vis}"), upstream, secret).await
```

Replace the whole `set-image-visibility` arm with:

```rust
        ["admin", "set-image-visibility", path, vis] => {
            let (o, n) = path.split_once('/').ok_or("owner/image")?;
            if !matches!(*vis, "public" | "private") {
                return Err(rustic_git::err("visibility must be public or private"));
            }
            // Mirrors `set-visibility` exactly: `imagevisibility` is a routed browse endpoint
            // (by the IMAGE key), so with a fleet configured the flip is delivered to the node
            // that owns the image's database rather than written here under a live writer.
            // Same either-variable test for "configured", for the same reason.
            let upstream = std::env::var("RUSTIC_GIT_UPSTREAM").ok();
            let secret = std::env::var("RUSTIC_GIT_PEER_SECRET").ok();
            if upstream.is_none() && secret.is_none() {
                eprintln!(
                    "set-image-visibility: no RUSTIC_GIT_UPSTREAM or RUSTIC_GIT_PEER_SECRET set — \
                     writing {path} directly, assuming NO node is currently serving it. If one is, it \
                     keeps answering from its own view for several seconds."
                ); // ponytail: eprintln
                return store.set_image_visibility(o, n, *vis == "public").await;
            }
            post_to_owner(
                "set-image-visibility",
                o,
                &format!("/api/{o}/{n}/imagevisibility?visibility={vis}"),
                upstream,
                secret,
            )
            .await
        }
```

Fix the `fleet_guard` doc comment (lines 381-389): delete the clause "mirroring `set-image-visibility`, which is in the same boat" — it now reads "...unlike `set-visibility` and `set-image-visibility` there is no routed `/api` endpoint to deliver a fork/repack/delete/create to the owning node, so a configured fleet means refuse...".

- [ ] **Step 4: Run tests, commit**

Run: `cargo test --bin rustic-git && cargo test`
Expected: PASS.

```bash
git add src/main.rs
git commit -m "Route admin set-image-visibility through the owning node"
```

---

## LOW

### Task 15: Session tokens carry and require `typ: "session"`

**Files:**
- Modify: `src/jwt.rs:21-33` (`Claims`), `:52-78` (`mint`, `verify`), tests

**Context:** Session claims have no type; a registry token is only rejected by `verify` because it happens to lack `name`. Make the distinction explicit, like `verify_registry` already does for its kind. Consequence to note in the commit body: sessions minted before this deploy lack `typ` and are refused once — users sign in again; TTL is 12h anyway.

- [ ] **Step 1: Write the failing test** (in `jwt.rs` tests)

```rust
    /// A token of ours that is not a SESSION must not open one: today the registry kind is
    /// refused only because it happens to lack `name`, which is an accident, not a rule.
    #[test]
    fn a_token_without_the_session_type_is_refused() {
        let raw = encode(
            &Header::new(Algorithm::HS256),
            &serde_json::json!({"sub": "a@b.com", "name": "A", "iat": 0, "exp": 99999999999u64}),
            &EncodingKey::from_secret("0123456789012345678901234567890123456789".as_bytes()),
        )
        .unwrap();
        assert!(jwt().verify(&raw).is_err(), "no typ");
        let reg = jwt().mint_registry("alice", "repository:alice/web:pull", 60).unwrap();
        assert!(jwt().verify(&reg).is_err(), "registry typ");
        let t = jwt().mint("a@b.com", "A", None).unwrap();
        assert_eq!(jwt().verify(&t).unwrap().typ, "session");
    }
```

Also update `an_expired_token_is_refused`'s `Claims { ... }` literal to include `typ: "session".into()`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib jwt`
Expected: compile error on the missing field first; after adding the field (Step 3's first edit only) the new test FAILS on `"no typ"`.

- [ ] **Step 3: Implement**

In `Claims`, after `username`:

```rust
    /// `"session"`. Explicit, so a registry token (`typ: "registry"`) or anything else we sign
    /// is refused by rule rather than by the accident of lacking `name`.
    #[serde(default)]
    pub typ: String,
```

In `mint`: `typ: "session".into(),` after `username`. In `verify`, replace the `decode(...).map(|d| d.claims)` with:

```rust
        let c = decode::<Claims>(token, &self.decoding, &v)
            .map(|d| d.claims)
            .map_err(|e| err(format!("invalid token: {e}")))?;
        if c.typ != "session" {
            return Err(err("invalid token: not a session"));
        }
        Ok(c)
```

- [ ] **Step 4: Run tests, commit**

Run: `cargo test --lib jwt && cargo test --test api_server`
Expected: PASS.

```bash
git add src/jwt.rs
git commit -m "Type session tokens and refuse any other kind"
```

---

### Task 16: Bound the cost of a sprayed bogus credential (negative cache, evicted on registration)

**Files:**
- Modify: `src/auth.rs:34-63` (`CACHE_TTL` doc, `lookup`), `:104-125` (`create_token`, `add_ssh_key`), tests

**Context:** Every unknown Basic/Bearer token and every unknown ssh key is one object-store GET, uncached, so a spray is one S3 round trip per attempt. The positive cache already exists; cache misses too, bounded. The trap is a key someone registers right after a failed login — the common case — so registration evicts the miss. (The earlier review asked for zero negative caching; the test that pinned that is rewritten to pin the bound instead.)

- [ ] **Step 1: Write the failing test** (replace `negative_auth_cache_is_bounded` in `auth.rs` tests)

```rust
    /// Misses are cached — a sprayed bogus token must not be one object-store GET each — but
    /// bounded, because there is an unbounded supply of bogus tokens and none of valid ones.
    #[tokio::test]
    async fn negative_auth_cache_is_bounded() {
        let os = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(os, dir.path().to_path_buf(), false).await.unwrap();
        for i in 0..10_000 {
            let _ = store.owner_for_token(&format!("bogus-token-{i}")).await;
        }
        assert!(store.auth_cache_len() <= super::NEG_CAP, "{}", store.auth_cache_len());
        assert!(store.auth_cache_len() > 0, "misses are cached at all");
    }

    /// The common sequence is "ssh fails, add the key, ssh again" — the cached miss must not make
    /// the second attempt fail for another minute.
    #[tokio::test]
    async fn a_key_added_after_a_failed_login_works_immediately() {
        let os = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(os, dir.path().to_path_buf(), false).await.unwrap();
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMOC8YcsFBuWUwnSZkPymFzXnbPlZth+fBP34XGNN+d test@example.com";
        let fp = Store::ssh_fingerprint(line).unwrap();
        assert_eq!(store.owner_for_fingerprint(&fp).await.unwrap(), None);
        store.add_ssh_key("alice", line).await.unwrap();
        assert_eq!(store.owner_for_fingerprint(&fp).await.unwrap().as_deref(), Some("alice"));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib auth`
Expected: `negative_auth_cache_is_bounded` FAILS (`NEG_CAP` undefined → after adding the const, fails on "misses are cached at all"); `a_key_added_after_a_failed_login_works_immediately` PASSES today (nothing is cached) — it is the guard for Step 3's eviction.

- [ ] **Step 3: Implement**

Replace the comment + insert at the end of `lookup` (lines 52-62) with:

```rust
        // Misses are cached too, or a sprayed bogus credential is one object-store GET each —
        // but bounded: there is an unbounded supply of bogus tokens and none of valid ones, so
        // when the map fills, every miss is dropped and the (few) hits kept. Registration evicts
        // the miss for its own key (see `create_token`/`add_ssh_key`), which is what makes
        // "ssh failed, add the key, ssh again" work inside one TTL.
        // ponytail: drop-all-misses on overflow, not LRU; an LRU crate only if a profile says so.
        let mut cache = self.auth_cache();
        if owner.is_none() && cache.len() >= NEG_CAP {
            cache.retain(|_, (_, v)| v.is_some());
        }
        cache.insert(cache_key, (Instant::now(), owner.clone()));
        Ok(owner)
```

Add below `CACHE_TTL`: `/// Entries (hits and misses together) past which every cached miss is dropped.\nconst NEG_CAP: usize = 4096;`

In `create_token`, after the `put`: `self.auth_cache().remove(&token_key(&t).to_string());`
In `add_ssh_key`, after the `put`: `self.auth_cache().remove(&sshkey_key(&fp).to_string());`

Update the `CACHE_TTL` doc (line 34-36): append "A miss is cached for the same time, except that registering the credential clears it."

- [ ] **Step 4: Run tests, commit**

Run: `cargo test --lib auth && cargo test`
Expected: PASS.

```bash
git add src/auth.rs
git commit -m "Cache credential misses, bounded, and clear them on registration"
```

---

### Task 17: `admin add-token`/`add-key` validate the owner

**Files:**
- Modify: `src/main.rs:577-583`

- [ ] **Step 1: Add the check**

```rust
        ["admin", "add-token", owner] => {
            // Same rule the api tier applies: a credential for an owner no URL can name is a
            // credential nothing can use, and a reserved name (`api`, `v2`) would be worse.
            if !rustic_git::store::valid_owner(owner) {
                return Err(rustic_git::err(format!("{owner}: not a valid owner name")));
            }
            println!("{}", store.create_token(owner).await?);
            Ok(())
        }
        ["admin", "add-key", owner, file] => {
            if !rustic_git::store::valid_owner(owner) {
                return Err(rustic_git::err(format!("{owner}: not a valid owner name")));
            }
            store.add_ssh_key(owner, &std::fs::read_to_string(file)?).await
        }
```

- [ ] **Step 2: Test it** (in `main.rs`'s `tests` module)

```rust
    #[tokio::test]
    async fn admin_credentials_refuse_an_invalid_owner() {
        let store = store().await;
        assert!(run(&["admin", "add-token", "api"], &store).await.is_err());
        assert!(run(&["admin", "add-token", "no/slash"], &store).await.is_err());
        assert!(run(&["admin", "add-token", "alice"], &store).await.is_ok());
        store.pool.close().await;
    }
```

Run: `cargo test --bin rustic-git admin_credentials_refuse_an_invalid_owner` → PASS.

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "Validate the owner on admin add-token and add-key"
```

---

### Task 18: Build the api client with `expect`, not a silent default

**Files:**
- Modify: `src/api/mod.rs:103-106`

- [ ] **Step 1:** Replace `.unwrap_or_default()` with `.expect("building an HTTP client cannot fail with these options")` — the same sentence `proxy::Forwarder::new` uses. A default client has NO timeout, which silently undid `UPSTREAM_TIMEOUT`.
- [ ] **Step 2:** `cargo build`, then:

```bash
git add src/api/mod.rs
git commit -m "Fail loudly if the api client cannot be built"
```

---

## CLEANUPS / REDUNDANCY

### Task 19: One `basic_token`, one `unauthorized`

**Files:**
- Modify: `src/auth.rs` (add both), `src/http.rs:605-612,660-666`, `src/api/browse.rs:109-131`, `src/registry/auth.rs:45-49`
- Test: `src/auth.rs` tests

**Interfaces:**
- Produces: `pub fn auth::basic_token(&HeaderMap) -> Option<String>`, `pub fn auth::unauthorized() -> Response`.

- [ ] **Step 1: Write the failing test** (in `auth.rs` tests)

```rust
    #[test]
    fn basic_token_reads_gits_shape_only() {
        use axum::http::{header, HeaderMap};
        let mut h = HeaderMap::new();
        // "x:secret"
        h.insert(header::AUTHORIZATION, "Basic eDpzZWNyZXQ=".parse().unwrap());
        assert_eq!(crate::auth::basic_token(&h).as_deref(), Some("secret"));
        h.insert(header::AUTHORIZATION, "Bearer eDpzZWNyZXQ=".parse().unwrap());
        assert_eq!(crate::auth::basic_token(&h), None);
        h.insert(header::AUTHORIZATION, "Basic not-base64!".parse().unwrap());
        assert_eq!(crate::auth::basic_token(&h), None);
        assert_eq!(crate::auth::basic_token(&HeaderMap::new()), None);
    }
```

- [ ] **Step 2: Run** `cargo test --lib basic_token_reads_gits_shape_only` → FAIL (undefined).

- [ ] **Step 3: Add to `src/auth.rs`** (free functions, after `authorize`)

```rust
/// The token inside a `Basic` Authorization header — git's own shape, `x:<token>`, which is what
/// `git clone` over HTTP and `docker login` both send. `None` for no header, another scheme, or
/// anything that does not decode. The one decoder for three callers (git HTTP, the api tier, the
/// registry) — they had drifted into three copies.
pub fn basic_token(headers: &axum::http::HeaderMap) -> Option<String> {
    use base64::Engine;
    let v = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let d = base64::engine::general_purpose::STANDARD.decode(v.strip_prefix("Basic ")?).ok()?;
    String::from_utf8(d).ok()?.split_once(':').map(|(_, p)| p.to_string())
}

/// 401 with the Basic challenge git understands. Shared by the git listener and the api tier —
/// two byte-identical copies are one more place for the realm to drift.
pub fn unauthorized() -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        axum::http::StatusCode::UNAUTHORIZED,
        [(axum::http::header::WWW_AUTHENTICATE, "Basic realm=\"rustic-git\"")],
        "auth required",
    )
        .into_response()
}
```

- [ ] **Step 4: Replace the three copies**

- `src/http.rs`: delete `fn unauthorized` (605-612) and add `use crate::auth::unauthorized;` near the top. In `open`, replace the seven-line `let token = headers.get(...)...` (660-666) with `let token = crate::auth::basic_token(headers);`. Delete `use base64::Engine;` (line 14) if nothing else in the file uses it (`grep -n base64 src/http.rs`).
- `src/api/browse.rs`: `bearer_or_basic` becomes

```rust
pub(crate) fn bearer_or_basic(headers: &HeaderMap) -> Option<String> {
    let bearer = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string);
    bearer.or_else(|| crate::auth::basic_token(headers))
}
```

  and `fn unauthorized` (124-131) becomes `pub(crate) use crate::auth::unauthorized;` (the `use browse::*` glob in `mod.rs` keeps `images.rs` compiling). If `use base64::Engine;` in `src/api/mod.rs` is then unused, remove it.
- `src/registry/auth.rs` lines 45-49:

```rust
    if v.starts_with("Basic ") {
        let Some(token) = crate::auth::basic_token(headers) else { return Err(challenge(None)) };
```

  Remove `use base64::Engine;` there if unused.

- [ ] **Step 5: Run, commit**

Run: `cargo test --lib auth && cargo clippy --lib && cargo test`
Expected: PASS, no new warnings.

```bash
git add src/auth.rs src/http.rs src/api/browse.rs src/api/mod.rs src/registry/auth.rs
git commit -m "Share one Basic-token decoder and one 401 across the three listeners"
```

---

### Task 20: Verify the session JWT once per request

**Files:**
- Modify: `src/api/mod.rs` (`Identity`, `identify`, `caller`), `src/api/repos.rs:241-262` (`settings_caller`), `src/api/pulls.rs` (5 handlers), `src/api/signatures.rs:41-73,110-118`

**Interfaces:**
- Produces: `pub(crate) struct Identity { pub email: String, pub name: Option<String> }`; `pub(crate) fn identify(&Api, &HeaderMap) -> Result<Identity, Response>`; `settings_caller` now returns `Result<(Identity, &Directory), Response>`.
- Consumes: Task 2's `peer_only`.

**Context:** `open_pull`/`comment_on_pull`/`merge_pull`/`close_pull` call `caller` and then `settings_caller` (which calls `caller` again) — two HMAC verifications; `commit_patch` adds a third to read `name`. Resolve once; hand the claims on.

- [ ] **Step 1: `src/api/mod.rs`** — above `caller`:

```rust
/// Who is asking, resolved once. `name` is `Some` only for a session token — the peer path
/// asserts an email and nothing more.
pub(crate) struct Identity {
    pub email: String,
    pub name: Option<String>,
}

pub(crate) fn identify(api: &Api, headers: &axum::http::HeaderMap) -> std::result::Result<Identity, Response> {
    if let Some(bearer) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        let jwt = api
            .jwt
            .as_deref()
            .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, "tokens not configured").into_response())?;
        return match jwt.verify(bearer.trim()) {
            Ok(c) => Ok(Identity { email: c.sub, name: Some(c.name) }),
            // Never say which of signature, algorithm or expiry failed.
            Err(_) => Err((StatusCode::UNAUTHORIZED, "invalid or expired token").into_response()),
        };
    }
    peer_only(api, headers).map(|email| Identity { email, name: None })
}

/// `identify`, for the many callers that only need the email.
pub(crate) fn caller(api: &Api, headers: &axum::http::HeaderMap) -> std::result::Result<String, Response> {
    identify(api, headers).map(|i| i.email)
}
```

(Delete the old body of `caller`; its doc comment moves onto `identify`.)

- [ ] **Step 2: `settings_caller`** in `repos.rs`:

```rust
/// The caller may act under `owner`, and `owner/name` is a well-formed repo path there. Returns
/// the resolved identity so a handler that needs it does not verify the token a second time.
pub(crate) async fn settings_caller<'a>(
    api: &'a Api,
    headers: &axum::http::HeaderMap,
    owner: &str,
    name: &str,
) -> std::result::Result<(Identity, &'a crate::directory::Directory), Response> {
    let who = identify(api, headers)?;
    let db = directory(api)?;
    if !crate::store::valid_owner(owner) || !crate::store::valid_segment(name) {
        return Err((StatusCode::BAD_REQUEST, "invalid repository name").into_response());
    }
    match may_act_under(db, &who.email, owner).await {
        Ok(true) => {}
        Ok(false) => return Err((StatusCode::NOT_FOUND, "no such repository").into_response()),
        Err(e) => {
            eprintln!("repo authorization: {e}"); // ponytail: eprintln
            return Err((StatusCode::BAD_GATEWAY, "could not read the repository").into_response());
        }
    }
    Ok((who, db))
}
```

The four `if let Err(r) = settings_caller(...)` sites in `repos.rs` compile unchanged.

- [ ] **Step 3: `pulls.rs`** — in `open_pull`, `comment_on_pull`, `merge_pull`, `close_pull` delete the leading `let user = match caller(...)` block and change the `settings_caller` line to

```rust
    let (who, _) = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
```

then use `who.email` where `user` was (`Value::String(who.email)` / `encode(&who.email)`). `list_pulls`, `get_pull`, `compare_branches` stay `if let Err(r) = ...`.

- [ ] **Step 4: `signatures.rs`** — `commit_patch` lines 47-68 become:

```rust
    let (who, _) = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    // The author is WHO IS SIGNED IN, never what the request said. A caller that could name its
    // own author could write history as somebody else. The display name comes from the session;
    // the peer path has none, so the email stands in.
    let name_of = who.name.unwrap_or_else(|| who.email.clone());
```

and the two inserts use `name_of` / `who.email`. `verify_commit` line 115: `let (_, db) = match settings_caller(...)`.

- [ ] **Step 5: Build, test, commit**

Run: `cargo build && cargo test --test api_server && cargo test`

```bash
git add src/api/mod.rs src/api/repos.rs src/api/pulls.rs src/api/signatures.rs
git commit -m "Resolve the caller once per api request"
```

---

### Task 21: Move the test-only event helper next to its only users

**Files:**
- Modify: `src/api/pulls.rs:34-69,245-292` (delete), `src/api/feed.rs` tests (add)

**Context:** `publish_pull_event` is `#[cfg(test)]` in `pulls.rs` and used only by `feed.rs` tests; `pulls.rs`'s two tests test nothing but the helper. Move it, delete them.

- [ ] **Step 1:** Cut lines 34-69 and the whole `#[cfg(test)] mod tests` (245-292) from `src/api/pulls.rs`.
- [ ] **Step 2:** Paste the helper into `src/api/feed.rs` inside `mod tests`, after the `use` lines, dropping the `#[cfg(test)]` attribute (the module already is) and keeping `#[allow(clippy::too_many_arguments)]`. Its doc becomes: `/// Puts one entry on the stream the way an owning node does, so the feed tests need no fleet.`
- [ ] **Step 3:** `cargo test --lib api` → PASS (the five feed tests still compile). If `Cache`/`Kind`/`events` imports in `api/mod.rs` go unused, they are still used by `feed.rs` — leave them.

```bash
git add src/api/pulls.rs src/api/feed.rs
git commit -m "Keep the test-only event helper beside the feed tests that use it"
```

---

### Task 22: Drop the dead `split('?')` in routing

**Files:**
- Modify: `src/http.rs:333-334`

- [ ] **Step 1:** `req.uri().path()` never carries a query. Replace the two lines with one:

```rust
        let tail = path.trim_start_matches('/').trim_start_matches("v2").trim_start_matches('/');
```

- [ ] **Step 2:** `cargo test --lib http && cargo test --test registry_http` → PASS.

```bash
git add src/http.rs
git commit -m "Drop the query split on a path that never has one"
```

---

### Task 23: `App::leader` returns `&str`

**Files:**
- Modify: `src/lib.rs:214-216,233,651,719`

- [ ] **Step 1:**

```rust
    fn leader(&self) -> &str {
        &self.leader_name
    }
```

`is_leader`: `self.leader() == self.self_name`. Line 651: `let leader = self.leader();` (drop the `?`; `(self.addr_of)(leader)` takes `&str`). Line 719: `let asker = if asker == self.leader() {`.

- [ ] **Step 2:** `cargo build && cargo test --lib && cargo test --test ownership` → PASS.

```bash
git add src/lib.rs
git commit -m "Return the leader name as a borrow; it cannot fail"
```

---

### Task 24: Name the negative cache's ceiling in its ponytail marker

**Files:**
- Modify: `src/lib.rs:92-95`

- [ ] **Step 1:** Replace the two-line `// ponytail:` comment with:

```rust
    // ponytail: 5s negative cache; a repo created within the window still 404s briefly —
    // acceptable, it's just-created. Ceiling: expired entries are only swept on insert past
    // 1024 entries, so within one TTL the map holds every distinct bad name seen in 5s — at
    // 10k rps of distinct names that is ~50k entries, ~5 MB. Cap by count if that ever shows.
```

- [ ] **Step 2:** `cargo build`, then:

```bash
git add src/lib.rs
git commit -m "State the negative route cache's memory ceiling"
```

---

### Task 25: Negative credential paths — a revoked credential stops working at once

**Files:**
- Test: `src/auth.rs` tests, `tests/browse_http.rs`

**Context:** Spec §6 asks for negative paths for teams and credentials. "Wrong team" (`may_act_under`) needs Mongo and has no fixture; it is NOT covered here and is listed as a gap. The store side — a revoked token and a removed ssh key no longer authenticate, on this node immediately — is testable and was not.

- [ ] **Step 1: Store-level test** (in `auth.rs` tests)

```rust
    /// Revocation is immediate on the node that performed it: the cached hit is dropped with the
    /// object, so a revoked token does not keep working for the rest of the cache TTL here.
    #[tokio::test]
    async fn a_revoked_credential_stops_authenticating_at_once() {
        let os = Arc::new(InMemory::new());
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(os, dir.path().to_path_buf(), false).await.unwrap();
        let token = store.create_token("alice").await.unwrap();
        assert_eq!(store.owner_for_token(&token).await.unwrap().as_deref(), Some("alice"));
        store.revoke_token_digest(&Store::token_digest(&token)).await.unwrap();
        assert_eq!(store.owner_for_token(&token).await.unwrap(), None);
        // Twice is not an error: the desired end state is the same.
        store.revoke_token_digest(&Store::token_digest(&token)).await.unwrap();

        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMOC8YcsFBuWUwnSZkPymFzXnbPlZth+fBP34XGNN+d test@example.com";
        let fp = Store::ssh_fingerprint(line).unwrap();
        store.add_ssh_key("alice", line).await.unwrap();
        assert_eq!(store.owner_for_fingerprint(&fp).await.unwrap().as_deref(), Some("alice"));
        store.remove_ssh_key(&fp).await.unwrap();
        assert_eq!(store.owner_for_fingerprint(&fp).await.unwrap(), None);
    }
```

- [ ] **Step 2: HTTP-level test** (append to `tests/browse_http.rs`)

```rust
/// The public listener: a token that was revoked is a 401, not the 403 a stranger gets — the
/// client must learn to present a different credential, not that this repo is closed to it.
#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_token_is_refused_on_the_public_listener() {
    let e = common::env().await;
    e.store.create_repo("alice", "web").await.unwrap();
    let token = e.store.create_token("alice").await.unwrap();
    let router = rustic_git::http::router(common::app(e.store.clone()).await);
    let get = |token: String| {
        let router = router.clone();
        async move {
            let req = Request::builder()
                .uri("/alice/web.git/info/refs?service=git-upload-pack")
                .header("git-protocol", "version=2")
                .header("authorization", {
                    use base64::Engine;
                    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("x:{token}")))
                })
                .body(axum::body::Body::empty())
                .unwrap();
            router.oneshot(req).await.unwrap().status()
        }
    };
    assert_eq!(get(token.clone()).await, StatusCode::OK);
    e.store.revoke_token_digest(&rustic_git::store::Store::token_digest(&token)).await.unwrap();
    assert_eq!(get(token).await, StatusCode::UNAUTHORIZED);
}
```

`base64` is a dependency of the crate; if it is not visible to integration tests (`grep base64 Cargo.toml` — it is a normal `[dependencies]` entry, so it is), add it under `[dev-dependencies]` with the same version.

- [ ] **Step 3: Run, commit**

Run: `cargo test --lib auth && cargo test --test browse_http`
Expected: PASS (these pin current behaviour; no implementation change).

```bash
git add src/auth.rs tests/browse_http.rs
git commit -m "Pin that a revoked credential is refused at once"
```

---

## Final verification (after all tasks)

- [ ] `cargo test` — full suite green.
- [ ] `cargo clippy --lib` and `cargo clippy --all-targets 2>&1 | grep -E "auth.rs|jwt.rs|http.rs|browse_api|api/|proxy.rs|ssh.rs|main.rs|directory.rs|lib.rs"` — nothing new in touched files; `await_holding_lock` count is 0.
- [ ] On a machine with `git` and `ssh`: `cargo test --test ssh_e2e` actually ran (no "skip" line) — Task 4's only coverage.
- [ ] Re-read spec §1 Medium (teams/passkeys), §2 Medium (pulls `.git`, ssh/proxy fence, `claim_username`), §3 (`imagetags`, merge/patch), §4 Rust rows (Basic ×3, `unauthorized` ×2, `http.rs:702`, JWT ×3, `publish_pull_event`, `split('?')`, `leader()`), §5 (`REPLICAS`, `add-token`, `auth.rs` mutex, `await_holding_lock`, `unwrap_or_default`, description cap, `read_bounded`, 500 bodies, `main.rs:634-666`), §6 rows (`verify_signature`, passkeys, `upsert_user` via Bearer, `.git` reads, SSH after fence, credential negative path) — each maps to a task above. Known gap, deliberate: "wrong team" negative path needs a Mongo fixture this suite does not have.
- [ ] Commit body for Task 15 mentions that pre-existing sessions are refused once after deploy.
