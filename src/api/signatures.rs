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
    let text = r.text().await.unwrap_or_default();
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

/// A GPG signature.
///
/// The lookup runs on the fingerprints the SIGNATURE names, because a commit is
/// normally signed by a subkey while the person is the primary key behind it.
/// `signer_by_any` walks that back.
pub(crate) async fn verify_pgp(
    db: &crate::directory::Directory,
    signed: &SignatureOf,
    payload: &[u8],
) -> Verification {
    let issuers = match crate::gpg::issuers(&signed.signature) {
        Ok(i) => i,
        Err(_) => {
            return Verification {
                state: "unverified",
                reason_code: "unknown_signature_type",
                signer: None,
                reason: Some("the signature could not be read".into()),
            }
        }
    };
    let known = match db.signer_by_any(&issuers).await {
        Ok(Some(k)) => k,
        Ok(None) => {
            return Verification {
                state: "unverified",
                reason_code: "unknown_key",
                signer: None,
                reason: Some("signed by a key nobody here has registered".into()),
            }
        }
        Err(e) => {
            eprintln!("signer lookup: {e}"); // ponytail: eprintln
            return Verification {
                state: "unverified",
                reason_code: "invalid",
                signer: None,
                reason: Some("the signing key could not be looked up".into()),
            };
        }
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

/// Judge one signature.
///
/// Two things have to hold for "verified": the signature is good, AND the key
/// belongs to the person the commit says wrote it. A valid signature by somebody
/// else's key is exactly what a forged authorship line looks like, so it reports
/// as unverified with the reason spelled out.
pub(crate) async fn verify_signature(db: &crate::directory::Directory, signed: &SignatureOf) -> Verification {
    use base64::Engine;

    let unverified = |code: &'static str, reason: &str| Verification {
        state: "unverified",
        reason_code: code,
        signer: None,
        reason: Some(reason.to_string()),
    };

    let Ok(payload) = base64::engine::general_purpose::STANDARD.decode(&signed.payload_base64)
    else {
        return unverified("invalid", "the signed content could not be read");
    };

    if crate::gpg::is_pgp(&signed.signature) {
        return verify_pgp(db, signed, &payload).await;
    }
    let Ok(sig) = signed.signature.parse::<russh::keys::ssh_key::SshSig>() else {
        return unverified("unknown_signature_type", "the signature could not be read");
    };

    let fingerprint = sig
        .public_key()
        .fingerprint(russh::keys::HashAlg::Sha256)
        .to_string();
    // Looked up the same way as a GPG key, so one index serves both kinds.
    let known = match db.signer_by_any(&[fingerprint.to_lowercase()]).await {
        Ok(k) => k,
        Err(e) => {
            eprintln!("signer lookup: {e}"); // ponytail: eprintln
            return unverified("invalid", "the signing key could not be looked up");
        }
    };
    let Some(known) = known else {
        return unverified("unknown_key", "signed by a key nobody here has registered");
    };

    // The cryptography last: an unknown key is not worth verifying against, and
    // this order means a bad signature and an unknown signer are never confused.
    let key = russh::keys::PublicKey::from(sig.public_key().clone());
    // `git` is the namespace git signs commits under; a signature made for
    // anything else is not a commit signature.
    if key.verify("git", &payload, &sig).is_err() {
        return unverified("invalid", "the signature does not match the commit");
    }
    if !known.created_by.eq_ignore_ascii_case(signed.author_email.trim()) {
        return Verification {
            state: "unverified",
            reason_code: "bad_email",
            signer: Some(known.created_by.clone()),
            reason: Some(format!(
                "signed by {}, but the commit says {} wrote it",
                known.created_by, signed.author_email
            )),
        };
    }
    Verification { state: "verified", reason_code: "valid", signer: Some(known.created_by), reason: None }
}
