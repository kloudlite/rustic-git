use super::*;

// ── credentials ─────────────────────────────────────────────────────────────
//
// A credential acts in exactly ONE namespace, chosen when it is made, because
// that is what the git fleet enforces: `auth::authorize` compares the credential's
// owner to the repo's owner, with no membership lookup — the nodes have no
// directory. Scoping here to a namespace the caller belongs to keeps the two ends
// saying the same thing, and means a leaked laptop key cannot reach a team's repos
// unless it was made for them.

use crate::directory::{CliLogin, Credential, CredentialKind};

#[derive(serde::Deserialize)]
pub(crate) struct NewCredential {
    owner: String,
    #[serde(default)]
    name: String,
    /// ssh keys only: the OpenSSH public key line.
    #[serde(default)]
    key: String,
    /// Register this key for SIGNING rather than for access. The same key may be
    /// added both ways; they are separate entries because they grant separate
    /// things.
    #[serde(default)]
    signing: bool,
}

/// A token, the one time it is readable. Everything else about it can be looked up
/// forever; the secret cannot, because only its digest is kept.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IssuedToken {
    token: String,
    #[serde(flatten)]
    meta: Credential,
}

/// The caller, and their right to act in `owner`. Every credential route starts
/// here, so none of them can be reached for a namespace that is not the caller's.
pub(crate) async fn credential_caller<'a>(
    api: &'a Api,
    headers: &axum::http::HeaderMap,
    owner: &str,
) -> std::result::Result<(String, &'a crate::directory::Directory), Response> {
    // `user_identity`, not `caller`: managing your own credentials is exactly what the CLI is
    // for, and this route pays for the revocation lookup a CLI token needs.
    let user = user_identity(api, headers).await?.email;
    credential_caller_as(api, user, owner).await
}

/// The membership half of `credential_caller`, for a route that has already resolved who is
/// asking (to default the owner from their handle) and must not pay for it twice.
pub(crate) async fn credential_caller_as<'a>(
    api: &'a Api,
    user: String,
    owner: &str,
) -> std::result::Result<(String, &'a crate::directory::Directory), Response> {
    let db = directory(api)?;
    match may_act_under(db, &user, owner).await {
        Ok(true) => Ok((user, db)),
        Ok(false) => Err((StatusCode::NOT_FOUND, "no such owner").into_response()),
        Err(e) => {
            tracing::error!(owner = %owner, error = %e, "credential authorization");
            Err((StatusCode::BAD_GATEWAY, "could not read credentials").into_response())
        }
    }
}

/// `?owner=` for the list routes.
pub(crate) fn owner_param(q: &std::collections::HashMap<String, String>) -> std::result::Result<String, Response> {
    q.get("owner")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "owner is required").into_response())
}

pub(crate) async fn create_token(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewCredential>,
) -> Response {
    let owner = body.owner.trim().to_string();
    let (user, db) = match credential_caller(&api, &headers, &owner).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let name = body.name.trim();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "give the token a name").into_response();
    }
    if name.chars().count() > 60 {
        return (StatusCode::BAD_REQUEST, "that name is too long").into_response();
    }

    // The secret is created FIRST and the index second, so a crash between them
    // leaves a working token nobody can see rather than a listed token that does
    // not work. The unwind below closes that window in the ordinary case.
    let token = match api.store.create_token(&owner).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(owner = %owner, error = %e, "create token");
            return (StatusCode::BAD_GATEWAY, "could not create the token").into_response();
        }
    };
    let meta = Credential {
        id: crate::store::Store::token_digest(&token),
        kind: CredentialKind::Token,
        owner: owner.clone(),
        created_by: user,
        name: name.to_string(),
        material: String::new(),
        fingerprints: Vec::new(),
        created_at: mongodb::bson::DateTime::now(),
    };
    match db.add_credential(&meta).await {
        Ok(Some(())) => {}
        // A digest collision is not a thing that happens; treat it as our failure.
        Ok(None) | Err(_) => {
            if let Err(e) = api.store.revoke_token_digest(&meta.id).await {
                tracing::warn!(error = %e, "unwinding token");
            }
            return (StatusCode::BAD_GATEWAY, "could not create the token").into_response();
        }
    }
    // The only time the token is ever readable.
    (StatusCode::CREATED, axum::Json(IssuedToken { token, meta })).into_response()
}

pub(crate) async fn list_tokens(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let owner = match owner_param(&q) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (_, db) = match credential_caller(&api, &headers, &owner).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    match db.credentials_for(&owner, CredentialKind::Token).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => {
            tracing::error!(owner = %owner, error = %e, "list tokens");
            (StatusCode::BAD_GATEWAY, "could not list tokens").into_response()
        }
    }
}

/// Revoke by id. The index is deleted LAST: if the object delete fails the
/// credential stays listed and revocable, which is the safe direction — a listed
/// token that still works can be revoked again, an unlisted one that still works
/// cannot be revoked at all.
pub(crate) async fn revoke_token(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    revoke(api, headers, id, CredentialKind::Token).await
}

pub(crate) async fn remove_key(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    revoke(api, headers, id, CredentialKind::SshKey).await
}

/// Push the owner's keys out to their workspaces, off the request path: the rows are already
/// written (or forgotten), so the answer does not depend on a cluster this tier only nudges.
fn spawn_keys_changed(api: &Arc<Api>, owner: &str) {
    let Some(hook) = api.on_keys_changed.clone() else { return };
    let owner = owner.to_string();
    tokio::spawn(async move { hook(owner).await });
}

pub(crate) async fn revoke(
    api: Arc<Api>,
    headers: axum::http::HeaderMap,
    id: String,
    kind: CredentialKind,
) -> Response {
    // `user_identity`, not `caller`: revoking your own key or your own login is exactly what
    // the CLI is for. Nothing weakens — authorization below is against `found.owner`, not
    // against how the caller proved who they are.
    let user = match user_identity(&api, &headers).await {
        Ok(i) => i.email,
        Err(r) => return r,
    };
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let found = match db.credential(&id).await {
        Ok(Some(c)) if c.kind == kind => c,
        // A credential of the wrong kind is reported as missing rather than as a
        // mistake: the id space is shared, and saying "that is an ssh key" tells a
        // caller something about a credential that may not be theirs.
        Ok(_) => return (StatusCode::NOT_FOUND, "no such credential").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "revoke lookup");
            return (StatusCode::BAD_GATEWAY, "could not revoke").into_response();
        }
    };
    // Authorized against the credential's OWNER, never against holding its id.
    match may_act_under(db, &user, &found.owner).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such credential").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "revoke authorization");
            return (StatusCode::BAD_GATEWAY, "could not revoke").into_response();
        }
    }
    let gone = match kind {
        CredentialKind::Token => api.store.revoke_token_digest(&id).await,
        CredentialKind::SshKey => api.store.remove_ssh_key(&id).await,
        // Neither a signing key nor a CLI login was ever written to the store the fleet
        // reads — a CLI token is a signed JWT and is only honoured while its row exists.
        // Forgetting the row is the whole of it.
        CredentialKind::SigningKey | CredentialKind::CliToken => Ok(()),
    };
    if let Err(e) = gone {
        tracing::error!(error = %e, "revoke");
        return (StatusCode::BAD_GATEWAY, "could not revoke").into_response();
    }
    if let Err(e) = db.forget_credential(&id).await {
        // The credential no longer works, which is what was asked for. It will
        // linger in the list until the next attempt succeeds.
        tracing::warn!(credential = %id, error = %e, "forget credential");
    }
    // AFTER the row is gone: the hook re-reads the owner's keys, and running it first would write
    // back the very key that was just revoked.
    if kind == CredentialKind::SshKey {
        spawn_keys_changed(&api, &found.owner);
    }
    StatusCode::NO_CONTENT.into_response()
}

/// The fingerprint of an OpenSSH public key line, or an error naming what is wrong with it.
/// The production `ssh_fingerprint` (a test-only twin lives in `tests/common`): the only consumer in
/// this crate, and duplicating eight lines is cheaper than adding a shared axum-free home for a
/// function that needs `russh` only here.
fn ssh_fingerprint(line: &str) -> crate::Result<String> {
    let key = russh::keys::PublicKey::from_openssh(line.trim())
        .map_err(|_| crate::err("that does not look like an OpenSSH public key"))?;
    Ok(key.fingerprint(russh::keys::HashAlg::Sha256).to_string())
}

/// The credential id and the fingerprints an ssh SIGNING key answers to. Kept beside `add_key`
/// and used by it, so a test can build exactly the row registration writes.
pub(crate) fn ssh_signing_fingerprints(key_line: &str) -> crate::Result<(String, Vec<String>)> {
    let f = ssh_fingerprint(key_line)?;
    // Lowercased: `signer_by_any` lowercases what a signature presents and Mongo's `$in` is an
    // exact match, while `SHA256:<base64>` is mixed case. Stored as-is, no ssh signature ever
    // found its key. The id keeps the original spelling — it is only ever matched by itself.
    Ok((f.clone(), vec![f.to_lowercase()]))
}

/// What is stored as a credential's material.
///
/// A GPG key keeps its armour verbatim — verification needs the bytes. An ssh key keeps its
/// public line because `authorized_keys` needs it: a fingerprint is one-way, so a key added
/// before this shipped can never be written into a workspace and has to be re-added.
///
/// Normalized to the first three whitespace fields — type, base64, comment — so a pasted line
/// with trailing options or stray whitespace cannot smuggle anything into `authorized_keys`,
/// where a leading field is `command=`/`from=` and changes what the key can do.
fn key_material(key: &str, is_gpg: bool) -> String {
    if is_gpg {
        return key.to_string();
    }
    key.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}

/// One `authorized_keys` line per access key the owner has.
///
/// Keys registered before material was kept contribute nothing — there is no way back from a
/// fingerprint — so they are skipped rather than emitted as a blank line, which sshd would
/// read as a syntax error and refuse the whole file over.
fn authorized_keys_lines(keys: &[Credential]) -> String {
    keys.iter()
        .map(|c| c.material.trim())
        .filter(|m| !m.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `authorized_keys` file for an owner: every ssh key they have registered for access.
pub async fn authorized_keys_for(db: &crate::directory::Directory, owner: &str) -> crate::Result<String> {
    let keys = db.credentials_for(owner, CredentialKind::SshKey).await?;
    Ok(authorized_keys_lines(&keys))
}

/// `(name, email)` for git to commit as inside the owner's workspaces. A handle that is not a
/// person's (never claimed) gets empty strings: git then asks, which is the right answer.
pub async fn git_identity_for(db: &crate::directory::Directory, owner: &str) -> crate::Result<(String, String)> {
    Ok(db.user_by_handle(owner).await?.map(|u| (u.name, u.email)).unwrap_or_default())
}

pub(crate) async fn add_key(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<NewCredential>,
) -> Response {
    let owner = body.owner.trim().to_string();
    let (user, db) = match credential_caller(&api, &headers, &owner).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    // An armoured OpenPGP block is a signing key and nothing else — it cannot
    // authenticate an ssh connection, so it is only accepted for signing.
    let is_gpg = body.key.contains("BEGIN PGP PUBLIC KEY BLOCK");
    if is_gpg && !body.signing {
        return (
            StatusCode::BAD_REQUEST,
            "a GPG key can only be added as a signing key",
        )
            .into_response();
    }

    // Parsed before anything is written, so a malformed key is a 400 rather than a row
    // describing a key the fleet never accepted. Parsing an OpenPGP key runs the self-signature
    // checks (real RSA/EdDSA maths) inside `gpg::parse_key`/`verified_emails`, so it goes to a blocking
    // thread rather than tying up the async executor; done once here and reused for both the
    // fingerprints and the default name, rather than parsing the armour twice.
    let (fingerprint, fingerprints, gpg_email) = if is_gpg {
        let armoured = body.key.clone();
        let parsed = tokio::task::spawn_blocking(move || {
            let key = crate::gpg::parse_key(&armoured)?;
            let fps = crate::gpg::fingerprints_of(&key);
            let email = crate::gpg::verified_emails(&key).into_iter().next();
            crate::Result::Ok((fps, email))
        })
        .await;
        match parsed {
            // The primary key names the credential; every subkey is indexed, so a
            // signature made by one finds its owner without a scan.
            Ok(Ok((fps, email))) if !fps.is_empty() => (fps[0].clone(), fps, email),
            _ => return (StatusCode::BAD_REQUEST, "that is not an OpenPGP public key").into_response(),
        }
    } else {
        match ssh_signing_fingerprints(&body.key) {
            Ok((fp, fps)) => (fp, fps, None),
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        }
    };
    // The comment at the end of the key line, when they did not name it — which is
    // usually `user@machine` and is exactly what they would have typed.
    let name = match body.name.trim() {
        "" if is_gpg => gpg_email.unwrap_or_else(|| "GPG key".to_string()),
        "" => body.key.split_whitespace().nth(2).unwrap_or("ssh key").to_string(),
        n => n.to_string(),
    };

    let meta = Credential {
        // Prefixed, so one key registered for both purposes is two rows.
        id: if body.signing { format!("sign:{fingerprint}") } else { fingerprint.clone() },
        kind: if body.signing { CredentialKind::SigningKey } else { CredentialKind::SshKey },
        owner: owner.clone(),
        created_by: user,
        name,
        material: key_material(&body.key, is_gpg),
        fingerprints,
        created_at: mongodb::bson::DateTime::now(),
    };
    // Index first here, unlike a token: the id is the key's own fingerprint rather
    // than a fresh secret, so the insert is what makes "already added" detectable.
    match db.add_credential(&meta).await {
        Ok(Some(())) => {}
        Ok(None) => return (StatusCode::CONFLICT, "that key is already added").into_response(),
        Err(e) => {
            tracing::error!(owner = %owner, error = %e, "add key");
            return (StatusCode::BAD_GATEWAY, "could not add the key").into_response();
        }
    }
    // Only an ACCESS key goes to the store the git nodes authenticate against. A
    // signing key there would silently grant push rights to anyone who added a key
    // to prove authorship.
    // An access key is also the only kind that lands in a workspace's `authorized_keys` — a
    // signing key proves authorship and opens no connection.
    if !body.signing && !is_gpg {
        if let Err(e) = api.store.add_ssh_key(&owner, &fingerprint).await {
            let _ = db.forget_credential(&meta.id).await;
            tracing::error!(owner = %owner, error = %e, "add key");
            return (StatusCode::BAD_GATEWAY, "could not add the key").into_response();
        }
        spawn_keys_changed(&api, &owner);
    }
    (StatusCode::CREATED, axum::Json(meta)).into_response()
}

pub(crate) async fn list_keys(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let owner = match owner_param(&q) {
        Ok(o) => o,
        Err(r) => return r,
    };
    let (_, db) = match credential_caller(&api, &headers, &owner).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let kind = match q.get("kind").map(String::as_str) {
        Some("signing") => CredentialKind::SigningKey,
        _ => CredentialKind::SshKey,
    };
    match db.credentials_for(&owner, kind).await {
        Ok(list) => axum::Json(list).into_response(),
        Err(e) => {
            tracing::error!(owner = %owner, error = %e, "list keys");
            (StatusCode::BAD_GATEWAY, "could not list keys").into_response()
        }
    }
}


// ── the CLI login handshake ─────────────────────────────────────────────────
//
// The device-code flow, because the CLI cannot receive a redirect and must not ever see a
// password: it asks for a code, the person types that code into a page they are already signed
// in to, and the CLI polls until a token appears. Nothing the CLI holds before approval is
// worth anything, so `/v1/cli/code` needs no credentials at all.

/// How long an unapproved code is worth typing in.
const CLI_CODE_TTL: std::time::Duration = std::time::Duration::from_secs(600);

/// No vowels (so a code cannot spell anything) and no `0/O/1/I` (so it cannot be mistyped) —
/// this gets read off one screen and typed into another.
const CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXYZ23456789";

#[derive(serde::Deserialize)]
pub(crate) struct DeviceCodeRequest {
    #[serde(default)]
    device: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeviceCode {
    code: String,
    poll: String,
    expires_in: u64,
}

fn random_code() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    // `gen_range`, not `% len`: 256 does not divide 29, so the modulo would make the first
    // few letters likelier than the rest.
    let raw: Vec<char> =
        (0..8).map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char).collect();
    format!("{}-{}", raw[..4].iter().collect::<String>(), raw[4..].iter().collect::<String>())
}

/// Anonymous: this is what a machine with no credentials asks for, and it grants nothing.
///
/// Answers `{ code, poll, expiresIn }` — `expiresIn` in SECONDS, the one duration on these
/// routes, so it is a number where every instant is an RFC3339 string.
pub(crate) async fn cli_code(
    State(api): State<Arc<Api>>,
    axum::Json(body): axum::Json<DeviceCodeRequest>,
) -> Response {
    let device = match body.device.trim() {
        "" => "a computer".to_string(),
        d => d.chars().take(60).collect(),
    };
    let code = random_code();
    let poll = hex::encode(&rand::random::<[u8; 16]>());
    // A row, not memory: the api has more than one replica, and the browser that approves this
    // code is routed independently of the CLI that asked for it.
    // The per-address bucket on this route (`ratelimit`, sized to `CLI_CODE_TTL`) is what caps
    // how many of these rows one flood can hold open at a time.
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let row = CliLogin {
        code: code.clone(),
        poll: poll.clone(),
        device,
        expires_at: mongodb::bson::DateTime::from_millis(
            mongodb::bson::DateTime::now().timestamp_millis() + CLI_CODE_TTL.as_millis() as i64,
        ),
        token: None,
        token_exp: 0,
    };
    if let Err(e) = db.create_cli_login(&row).await {
        tracing::error!(error = %e, "recording cli login code");
        return (StatusCode::BAD_GATEWAY, "could not start a login").into_response();
    }
    (
        StatusCode::CREATED,
        axum::Json(DeviceCode { code, poll, expires_in: CLI_CODE_TTL.as_secs() }),
    )
        .into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct ApproveRequest {
    #[serde(default)]
    code: String,
}

/// Approve a code, in the browser, as the person the token will belong to.
///
/// A browser SESSION only — `identify`, not `user_identity`: a CLI token that could approve
/// another would let one leaked login mint fresh ones forever, outliving its own revocation.
pub(crate) async fn cli_approve(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<ApproveRequest>,
) -> Response {
    let who = match identify(&api, &headers) {
        Ok(i) => i,
        Err(r) => return r,
    };
    // The person types it; case and the dash are theirs to get wrong.
    let code = body.code.trim().to_uppercase();
    let Some(username) = who.username.filter(|u| !u.trim().is_empty()) else {
        return (StatusCode::BAD_REQUEST, "pick a handle before signing in from the CLI").into_response();
    };
    let jwt = match api.jwt.as_deref() {
        Some(j) => j,
        None => return (StatusCode::SERVICE_UNAVAILABLE, "tokens not configured").into_response(),
    };

    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let device = match db.cli_login_pending(&code).await {
        Ok(Some(p)) => p.device,
        // Already approved, expired or never issued all look the same from here: a wrong
        // code must not tell a guesser that some other code exists.
        Ok(None) => return (StatusCode::NOT_FOUND, "no such code").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "looking up cli login code");
            return (StatusCode::BAD_GATEWAY, "could not sign you in").into_response();
        }
    };

    let (token, claims) = match jwt.mint_cli(&who.email, who.name.as_deref().unwrap_or_default(), Some(&username)) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "mint cli token");
            return (StatusCode::BAD_GATEWAY, "could not sign you in").into_response();
        }
    };
    // The row is written BEFORE the token is handed out: `user_identity` honours a `cli` token
    // only while its row stands, so a token whose row was never written is inert rather than a
    // 30-day credential nobody can revoke.
    let row = Credential {
        id: claims.jti.clone(),
        kind: CredentialKind::CliToken,
        owner: username,
        created_by: who.email,
        name: device,
        material: String::new(),
        fingerprints: Vec::new(),
        created_at: mongodb::bson::DateTime::now(),
    };
    match db.add_credential(&row).await {
        Ok(Some(())) => {}
        Ok(None) => {
            tracing::error!(jti = %row.id, "recording cli token: id already taken");
            return (StatusCode::BAD_GATEWAY, "could not sign you in").into_response();
        }
        Err(e) => {
            tracing::error!(error = %e, "recording cli token");
            return (StatusCode::BAD_GATEWAY, "could not sign you in").into_response();
        }
    }
    // The pending check above was a separate read, so two approvals of one code can both reach
    // here. The winner is decided by the update's own filter; the loser deletes the row it
    // wrote, because a live row for a token that will never be delivered is a login nobody made
    // and nobody can recognise to revoke.
    let stored = match db.approve_cli_login(&code, &token, claims.exp).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "approving cli login code");
            false
        }
    };
    if !stored {
        if let Err(e) = db.forget_credential(&row.id).await {
            tracing::warn!(jti = %row.id, error = %e, "unwinding a cli token nobody will collect");
        }
        return (StatusCode::CONFLICT, "that code was already used").into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingCode {
    /// What the CLI called itself when it asked for the code. Free text from an unauthenticated
    /// caller, so the page renders it as text and never as markup.
    device: String,
    /// RFC3339, like every other instant these routes answer with.
    expires_at: String,
}

/// What the approval page shows before it offers a button: WHICH machine is asking.
///
/// A page that approves a prefilled code with one click and names no device asks the person to
/// confirm nothing — the only check they can make is "is this my terminal", and that needs the
/// device on screen. Browser SESSION only, same rule as `cli_approve`.
///
/// 404 for unknown, expired and already-approved alike, exactly as `cli_approve` does: a guesser
/// must not learn that some other code exists.
pub(crate) async fn cli_pending_code(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(code): axum::extract::Path<String>,
) -> Response {
    if let Err(r) = identify(&api, &headers) {
        return r;
    }
    let code = code.trim().to_uppercase();
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.cli_login_pending(&code).await {
        Ok(Some(p)) => axum::Json(PendingCode {
            device: p.device,
            expires_at: rfc3339(p.expires_at.timestamp_millis()),
        })
        .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such code").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "looking up cli login code");
            (StatusCode::BAD_GATEWAY, "could not look that up").into_response()
        }
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CliToken {
    token: String,
    /// RFC3339, like every other instant `/v1/cli/*` answers with — the CLI writes it into its
    /// config file, where an epoch number is unreadable and a BSON `$date` is not JSON anyone
    /// else parses.
    expires_at: String,
}

/// The CLI polls this. 202 while nobody has approved it, 200 with the token exactly once, 410
/// after that — a token handed to two pollers is a token stolen by whoever asked twice.
///
/// 200 answers `{ token, expiresAt }`, `expiresAt` an RFC3339 string.
pub(crate) async fn cli_token(
    State(api): State<Arc<Api>>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let poll = q.get("poll").map(String::as_str).unwrap_or_default();
    if poll.is_empty() {
        return (StatusCode::GONE, "that login expired").into_response();
    }
    let db = match directory(&api) {
        Ok(d) => d,
        Err(r) => return r,
    };
    match db.take_cli_login(poll).await {
        Ok(None) => (StatusCode::GONE, "that login expired").into_response(),
        Ok(Some(CliLogin { token: None, .. })) => StatusCode::ACCEPTED.into_response(),
        Ok(Some(CliLogin { token: Some(token), token_exp, .. })) => {
            axum::Json(CliToken { token, expires_at: rfc3339(token_exp as i64 * 1000) }).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "collecting cli login token");
            (StatusCode::BAD_GATEWAY, "could not sign you in").into_response()
        }
    }
}

/// Epoch milliseconds as RFC3339. One spelling for every instant these routes answer with.
fn rfc3339(ms: i64) -> String {
    mongodb::bson::DateTime::from_millis(ms).try_to_rfc3339_string().unwrap_or_default()
}

/// The signed-in person's CLI logins.
///
/// Answers `[{ id, name, createdAt, expiresAt }]`, both instants RFC3339 strings.
///
/// Defaults to the caller's own handle — a CLI token is personal, and asking someone to name
/// themselves in a query string to see their own logins is a footgun the CLI would just get
/// wrong. `?owner=` stays as an override, still gated by `may_act_under`.
pub(crate) async fn list_cli_tokens(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    // Resolved once: the identity both defaults the owner and gates it, and resolving it is a
    // token verify plus a revocation lookup.
    let who = match user_identity(&api, &headers).await {
        Ok(i) => i,
        Err(r) => return r,
    };
    let owner = match owner_param(&q) {
        Ok(o) => o,
        Err(_) => match who.username.filter(|u| !u.trim().is_empty()) {
            Some(u) => u,
            None => return (StatusCode::BAD_REQUEST, "owner is required").into_response(),
        },
    };
    let (_, db) = match credential_caller_as(&api, who.email, &owner).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    match db.credentials_for(&owner, CredentialKind::CliToken).await {
        // Not the raw row: `expiresAt` is what the settings page shows, and it is not stored —
        // the token's TTL is fixed, so it is the creation instant plus that.
        Ok(list) => axum::Json(
            list.iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id,
                        "name": c.name,
                        "createdAt": rfc3339(c.created_at.timestamp_millis()),
                        "expiresAt": rfc3339(
                            c.created_at.timestamp_millis() + crate::jwt::CLI_TTL_SECS as i64 * 1000,
                        ),
                    })
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => {
            tracing::error!(owner = %owner, error = %e, "list cli tokens");
            (StatusCode::BAD_GATEWAY, "could not list logins").into_response()
        }
    }
}

pub(crate) async fn revoke_cli_token(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    revoke(api, headers, id, CredentialKind::CliToken).await
}


// ── the platform-issued key ──────────────────────────────────────────────
//
// One keypair per user, generated by us rather than supplied by them. The private half is written
// into every workspace so `git push` from inside one works without anybody pasting a credential;
// the public half is registered exactly like a user-added key, so the auth path is unchanged.
//
// The blast radius is real and worth stating where someone will read it: the key is the user's git
// identity, and it sits in every workspace they own, readable by anything running there — a
// malicious postinstall included. Regenerating is the remedy, which is why revocation is part of
// rotation rather than a separate chore.
// ponytail: one key for every workspace; a key per workspace would confine a compromise to one,
// at the cost of a fingerprint per workspace to list and revoke.

/// Generated in-process with `russh`'s own `ssh-key`, not by shelling out to `ssh-keygen`.
///
/// The shell-out was defended as keeping a second `rand_core` out of the graph; it does not —
/// `cargo tree -i rand_core` already reports 0.6, 0.9 and 0.10, and ssh-key's `random` (behind
/// russh's `ed25519` feature) speaks the 0.10 one that russh itself pulls. Doing it here also
/// drops the scratch-directory dance these read-only-rootfs pods needed for `ssh-keygen -f`.
fn generate_ed25519() -> std::io::Result<(String, String)> {
    use russh::keys::ssh_key::{Algorithm, LineEnding, PrivateKey};

    let mut key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    key.set_comment("rustic-git");
    let private = key.to_openssh(LineEnding::LF).map_err(|e| std::io::Error::other(e.to_string()))?;
    let public = key.public_key().to_openssh().map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok((private.to_string(), public))
}

/// `rand` 0.8's `OsRng` behind `rand_core` 0.10's traits: the two `rand` lines in the lock share
/// no traits, and ssh-key speaks 0.10. Entropy still comes from the OS, never from a seed.
struct OsRng;

impl rand_core::TryRng for OsRng {
    type Error = std::convert::Infallible;

    fn try_next_u32(&mut self) -> std::result::Result<u32, Self::Error> {
        let mut b = [0u8; 4];
        self.try_fill_bytes(&mut b)?;
        Ok(u32::from_ne_bytes(b))
    }

    fn try_next_u64(&mut self) -> std::result::Result<u64, Self::Error> {
        let mut b = [0u8; 8];
        self.try_fill_bytes(&mut b)?;
        Ok(u64::from_ne_bytes(b))
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> std::result::Result<(), Self::Error> {
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, dst);
        Ok(())
    }
}

impl rand_core::TryCryptoRng for OsRng {}

#[derive(serde::Serialize)]
pub(crate) struct PlatformKey {
    pub public: String,
    pub fingerprint: String,
}

/// The user's platform key, generating one on first read.
///
/// Lazy rather than hooked into user creation: an account that never opens a workspace never needs
/// one, and "generate on signup" is a migration for every account that already exists.
pub(crate) async fn platform_key(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let owner = match owner_param(&q) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = credential_caller(&api, &headers, &owner).await {
        return r;
    }
    match ensure_platform_key(&api, &owner, false).await {
        Ok(k) => axum::Json(k).into_response(),
        Err(r) => r,
    }
}

/// Replace the key, revoking the old one.
pub(crate) async fn regenerate_platform_key(
    State(api): State<Arc<Api>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let owner = match owner_param(&q) {
        Ok(o) => o,
        Err(r) => return r,
    };
    if let Err(r) = credential_caller(&api, &headers, &owner).await {
        return r;
    }
    match ensure_platform_key(&api, &owner, true).await {
        Ok(k) => axum::Json(k).into_response(),
        Err(r) => r,
    }
}

async fn ensure_platform_key(api: &Api, owner: &str, force: bool) -> std::result::Result<PlatformKey, Response> {
    // Every arm below logs before it answers. The first cut returned a bare 502, so a pod that
    // could not write a key looked identical to one that was never asked — the logs said nothing
    // at all while the page showed "Could not load the key".
    let bad = |what: &str| {
        tracing::error!(%owner, reason = what, "platform key");
        (StatusCode::BAD_GATEWAY, what.to_string()).into_response()
    };

    let existing = api.store.user_key(owner).await.map_err(|_| bad("could not read the key"))?;
    let old_fp = existing.as_deref().and_then(|p| fingerprint_of_private(p).ok());

    if !force {
        if let Some(private) = &existing {
            let (public, fingerprint) =
                public_of_private(private).map_err(|_| bad("stored key is unreadable"))?;
            return Ok(PlatformKey { public, fingerprint });
        }
    }

    let (private, public) = generate_ed25519().map_err(|_| bad("could not generate a key"))?;
    // The same fingerprint the auth path indexes by, so a generated key is looked up exactly
    // like a user-added one.
    let fingerprint = ssh_fingerprint(&public).map_err(|_| bad("generated key is unreadable"))?;
    api.store
        .rotate_user_key(owner, &private, &fingerprint, old_fp.as_deref())
        .await
        .map_err(|_| bad("could not install the key"))?;
    tracing::info!(%owner, replaced = old_fp.is_some(), "installed a platform key");
    Ok(PlatformKey { public, fingerprint })
}

/// The OpenSSH public line and fingerprint for a private key.
fn public_of_private(private: &str) -> std::result::Result<(String, String), ()> {
    let key = russh::keys::PrivateKey::from_openssh(private).map_err(|_| ())?;
    let public = key.public_key().to_openssh().map_err(|_| ())?;
    let fp = ssh_fingerprint(&public).map_err(|_| ())?;
    Ok((public, fp))
}

fn fingerprint_of_private(private: &str) -> std::result::Result<String, ()> {
    public_of_private(private).map(|(_, fp)| fp)
}


#[cfg(test)]
mod tests {
    use super::*;

    fn cred(name: &str, material: &str) -> Credential {
        Credential {
            id: name.into(),
            kind: CredentialKind::SshKey,
            owner: "alice".into(),
            created_by: "alice@example.com".into(),
            name: name.into(),
            material: material.into(),
            fingerprints: Vec::new(),
            created_at: mongodb::bson::DateTime::now(),
        }
    }

    /// An ssh key has to keep its public line now — `authorized_keys` cannot be rebuilt from a
    /// fingerprint — without disturbing the GPG case, which has always kept its armour whole.
    #[test]
    fn an_ssh_key_keeps_its_material_and_a_gpg_key_still_keeps_its_own() {
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample alice@laptop";
        assert_eq!(key_material(line, false), line);
        // Whitespace and anything past the comment are dropped: a fourth field in
        // `authorized_keys` is not a comment, and a leading one would be `command=`.
        assert_eq!(
            key_material("  ssh-ed25519   AAAAC3Nz alice@laptop\nfrom=\"evil\" ssh-rsa AAAA x", false),
            "ssh-ed25519 AAAAC3Nz alice@laptop"
        );
        let armour = "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nmDMEY\n-----END PGP PUBLIC KEY BLOCK-----\n";
        assert_eq!(key_material(armour, true), armour);
    }

    /// One line per key, and nothing at all for the keys registered before material was kept —
    /// a blank line in `authorized_keys` is a syntax error sshd rejects the whole file over.
    #[test]
    fn authorized_keys_is_one_line_per_key_and_skips_keys_added_before_material_was_kept() {
        let keys = [
            cred("new", "ssh-ed25519 AAAA alice@laptop"),
            cred("old", ""),
            cred("newer", "ssh-rsa BBBB alice@desktop"),
        ];
        assert_eq!(
            authorized_keys_lines(&keys),
            "ssh-ed25519 AAAA alice@laptop\nssh-rsa BBBB alice@desktop"
        );
        assert_eq!(authorized_keys_lines(&[cred("old", "  ")]), "");
    }

    async fn cli_api() -> Arc<Api> {
        let mut api = crate::testing::test_api_with_secret("peer").await;
        api.jwt = Some(Arc::new(crate::jwt::Jwt::new("0123456789012345678901234567890123456789").unwrap()));
        Arc::new(api)
    }

    fn session(api: &Api) -> axum::http::HeaderMap {
        let tok = api.jwt.as_ref().unwrap().mint("alice@example.com", "Alice", Some("alice")).unwrap();
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::AUTHORIZATION, format!("Bearer {tok}").parse().unwrap());
        h
    }

    /// Every step of the handshake lives in the directory now (the api has more than one
    /// replica, so memory was the bug). Without one, each step fails CLOSED — a 503, never a
    /// code that looks issued or a token that looks collectable. The exactly-once handover is
    /// the `take_cli_login` delete filter — Mongo's, not ours, and the one thing here no
    /// in-process test can reach.
    #[tokio::test]
    async fn the_cli_code_flow_fails_closed_without_a_directory() {
        use axum::extract::Path;
        let api = cli_api().await;
        let r = cli_code(State(api.clone()), axum::Json(DeviceCodeRequest { device: "karthik-mbp".into() })).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);

        let q = |p: &str| axum::extract::Query(std::collections::HashMap::from([("poll".into(), p.to_string())]));
        // An empty poll id names nothing and never reaches storage.
        assert_eq!(cli_token(State(api.clone()), q("")).await.status(), StatusCode::GONE);
        assert_eq!(cli_token(State(api.clone()), q("abc")).await.status(), StatusCode::SERVICE_UNAVAILABLE);

        // Approval and the device lookup are a signed-in person's acts, checked BEFORE storage.
        let anon = axum::http::HeaderMap::new();
        let r = cli_approve(State(api.clone()), anon.clone(), axum::Json(ApproveRequest { code: "ZZZZ-ZZZZ".into() })).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let r = cli_approve(State(api.clone()), session(&api), axum::Json(ApproveRequest { code: "zzzz-zzzz".into() })).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        let r = cli_pending_code(State(api.clone()), anon, Path("ZZZZ-ZZZZ".into())).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "the device is not anonymous to read");
        let r = cli_pending_code(State(api.clone()), session(&api), Path("ZZZZ-ZZZZ".into())).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Shaped so a human can read it aloud: four and four, a dash between, nothing that can be
    /// mistyped or spells anything.
    #[test]
    fn a_device_code_is_readable_aloud() {
        let code = random_code();
        assert_eq!(code.len(), 9, "{code}");
        assert_eq!(&code[4..5], "-");
        assert!(code.bytes().filter(|b| *b != b'-').all(|b| CODE_ALPHABET.contains(&b)), "{code}");
    }

    /// A CLI login has to be able to revoke ITSELF — otherwise `kl logout` needs a browser.
    /// `revoke` used to go through `identify`, which refuses a `cli` token outright.
    ///
    /// The proof this test can make without a directory is where it STOPS: a session or a cli
    /// token both get past authentication and stop at the missing directory (503), while a
    /// caller with no token at all never gets that far (401).
    #[tokio::test]
    async fn a_cli_token_gets_past_auth_to_revoke_its_own_jti() {
        let api = cli_api().await;
        let (token, claims) =
            api.jwt.as_ref().unwrap().mint_cli("alice@example.com", "Alice", Some("alice")).unwrap();
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());

        let r = revoke_cli_token(State(api.clone()), h, axum::extract::Path(claims.jti.clone())).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE, "a cli token must reach the lookup");

        let r = revoke_cli_token(
            State(api.clone()),
            axum::http::HeaderMap::new(),
            axum::extract::Path(claims.jti),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }
}

#[cfg(test)]
mod platform_key_tests {
    /// The bytes a user's git client will read: the OpenSSH private-key envelope, a matching
    /// `ssh-ed25519` public line, and a fingerprint the auth path can index by. The generator
    /// touches no filesystem now, which is what made the read-only root a non-issue.
    #[test]
    fn keys_are_openssh_format() {
        let (private, public) = super::generate_ed25519().expect("generate");

        assert!(private.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(public.starts_with("ssh-ed25519 "));
        // The fingerprint the auth path indexes by has to be derivable from what we hand back.
        let fp = super::ssh_fingerprint(&public).expect("fingerprint");
        assert!(fp.starts_with("SHA256:"), "{fp}");
        // The private half has to round-trip to the same public line: rotation reads the stored
        // private key to answer "what is my key" on every later page load.
        let (again, fp2) = super::public_of_private(&private).expect("round trip");
        assert_eq!(again, public);
        assert_eq!(fp2, fp);
    }
}
