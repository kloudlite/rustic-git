use super::*;

// ── commit signatures ───────────────────────────────────────────────────────

/// What a signature amounts to.
///
/// The three answers are deliberately distinct. "Signed by a key we do not know"
/// is not the same as "signed by a key that is not this author's" — the first is
/// a stranger, the second is a mismatch worth looking at — and neither is
/// "unsigned", which is simply the common case and not a warning.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Verification {
    /// `unsigned` | `verified` | `unverified`
    state: &'static str,
    /// The same vocabulary GitHub uses — `valid`, `unknown_key`, `expired_key`,
    /// `bad_email` and so on — so a client branches on a fixed set rather than on
    /// prose that can be reworded.
    reason_code: &'static str,
    /// Who the key belongs to, when we know them.
    #[serde(skip_serializing_if = "Option::is_none")]
    signer: Option<String>,
    /// Why it is not verified, in words meant for a person.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(serde::Deserialize)]
pub(crate) struct SignatureOf {
    signature: String,
    payload_base64: String,
    author_email: String,
}

/// The api tier's half of a patch: authorize, name the author, forward.
///
/// The api tier never writes objects itself — the owning node does, because one
/// writer per repo is what makes branch protection and ref updates decidable. So
/// this establishes WHO is committing and hands the patch on; the node's
/// `update_refs` still has the last word on whether the branch may move.
pub(crate) async fn commit_patch(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    axum::Json(mut body): axum::Json<serde_json::Value>,
) -> Response {
    let user = match caller(&api, &headers) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if let Err(r) = settings_caller(&api, &headers, &owner, &name).await {
        return r;
    }

    // The author is WHO IS SIGNED IN, never what the request said. A caller that
    // could name its own author could write history as somebody else.
    let name_of = api
        .jwt
        .as_deref()
        .and_then(|j| {
            headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .and_then(|t| j.verify(t.trim()).ok())
        })
        .map(|c| c.name)
        .unwrap_or_else(|| user.clone());
    let Some(obj) = body.as_object_mut() else {
        return (StatusCode::BAD_REQUEST, "expected an object").into_response();
    };
    obj.insert("authorName".into(), serde_json::Value::String(name_of));
    obj.insert("authorEmail".into(), serde_json::Value::String(user));

    let url = format!("{}/api/{}/{}/patch", api.upstream, encode(&owner), encode(&name));
    let sent = api
        .client
        .post(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .header(crate::proxy::OWNER_HEADER, &owner)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(match serde_json::to_vec(&body) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("commit patch: {e}"); // ponytail: eprintln
                return (StatusCode::BAD_REQUEST, "could not read the patch").into_response();
            }
        })
        .send()
        .await;
    let r = match sent {
        Ok(r) => r,
        Err(e) => {
            eprintln!("commit patch: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "could not reach the repository").into_response();
        }
    };
    let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let text = text_bounded(r).await;
    // The node's own words: "this branch has moved since you started editing", or
    // the protection rule that refused it. Both are written for the person at the
    // editor, so they are passed through rather than replaced.
    if status.is_success() {
        (status, [(axum::http::header::CONTENT_TYPE, "application/json")], text).into_response()
    } else {
        (status, text).into_response()
    }
}

pub(crate) async fn verify_commit(
    State(api): State<Arc<Api>>,
    axum::extract::Path((owner, name, sha)): axum::extract::Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    let db = match settings_caller(&api, &headers, &owner, &name).await {
        Ok(d) => d,
        Err(r) => return r,
    };

    let url = format!(
        "{}/api/{}/{}/signature/{}",
        api.upstream,
        encode(&owner),
        encode(&name),
        encode(&sha)
    );
    // The peer secret alone is not an identity: this route reads a repo, so the
    // node applies the same read check it applies to any browse request and needs
    // to be told WHO is reading. `settings_caller` has already established that
    // the caller may act under this owner, which is what is asserted here.
    let r = match api
        .client
        .get(url)
        .header(crate::proxy::PEER_HEADER, &api.secret)
        .header(crate::proxy::OWNER_HEADER, &owner)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("signature upstream: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response();
        }
    };
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        return (StatusCode::NOT_FOUND, "no such commit").into_response();
    }
    let body = match read_bounded(r).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("signature body: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response();
        }
    };
    let signed: Option<SignatureOf> = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("signature parse: {e}"); // ponytail: eprintln
            return (StatusCode::BAD_GATEWAY, "the service is unavailable").into_response();
        }
    };
    let Some(signed) = signed else {
        return axum::Json(Verification {
            state: "unsigned",
            reason_code: "unsigned",
            signer: None,
            reason: None,
        })
        .into_response();
    };

    axum::Json(verify_signature(db, &signed).await).into_response()
}

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
        // The stored spelling IS the spelling a signature presents — no lowercasing in the
        // lookup in between. Without this the `lookup` mock's own `to_lowercase` would paper
        // over registration going back to storing `SHA256:<base64>` verbatim.
        assert_eq!(cred.fingerprints[0], ssh_signature_fingerprint(&sig));
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
        assert_eq!(v.state, "verified");

        let other = SignatureOf { author_email: "bob@example.com".into(), ..signed };
        assert_eq!(judge_pgp(lookup(&[cred], &issuers), &other, payload).reason_code, "bad_email");
        assert_eq!(judge_pgp(None, &other, payload).reason_code, "unknown_key");
    }
}
