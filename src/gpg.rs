//! Verifying GPG-signed commits.
//!
//! Separate from the ssh path because the two are not the same shape. An ssh
//! signature carries its own public key, so the key IS the identity and one
//! fingerprint answers everything. An OpenPGP key is a primary key with SUBKEYS,
//! several user ids, an expiry and possibly a revocation — commits are normally
//! signed by a signing subkey, and the person is the primary key behind it.
//!
//! That is why a registered gpg key stores the whole armoured key rather than a
//! fingerprint: verification needs the material, and the answer to "whose is
//! this?" is a walk from subkey to primary.

use crate::Result;
use pgp::composed::{Deserializable, DetachedSignature, SignedPublicKey};

/// Why a signature is or is not good.
///
/// The names follow GitHub's, because a client that already branches on theirs
/// should not have to learn a second vocabulary — and because each of these is a
/// genuinely different situation to a person reading it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Valid,
    /// Signed by a key nobody has registered here.
    UnknownKey,
    /// The key was registered, but had expired when this was checked.
    ExpiredKey,
    /// Its owner published a revocation: the key is not to be trusted, whatever
    /// the maths says.
    RevokedKey,
    /// The signature does not match the bytes — tampering, or corruption.
    Invalid,
    /// The signature is good, but the key is not the commit author's.
    BadEmail,
    /// Not a form we can check.
    UnknownSignatureType,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Valid => "valid",
            Reason::UnknownKey => "unknown_key",
            Reason::ExpiredKey => "expired_key",
            Reason::RevokedKey => "revoked_key",
            Reason::Invalid => "invalid",
            Reason::BadEmail => "bad_email",
            Reason::UnknownSignatureType => "unknown_signature_type",
        }
    }
}

/// Is this armour an OpenPGP signature?
pub fn is_pgp(signature: &str) -> bool {
    signature.contains("BEGIN PGP SIGNATURE")
}

/// The fingerprints a signature says made it, longest-lived first.
///
/// A signature names its issuer by fingerprint, or on older keys only by key id
/// (the last eight bytes of the fingerprint). Both are returned as lowercase hex
/// so a lookup can match either against a registered key.
pub fn issuers(signature: &str) -> Result<Vec<String>> {
    let (sig, _) = DetachedSignature::from_string(signature)
        .map_err(|e| crate::err(format!("signature: {e}")))?;
    let mut out: Vec<String> = sig
        .signature
        .issuer_fingerprint()
        .into_iter()
        .map(|f| hex(f.as_bytes()))
        .collect();
    out.extend(sig.signature.issuer_key_id().into_iter().map(|k| hex(k.as_ref())));
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The primary key's fingerprint, plus every subkey's — what a registered key
/// answers to. Stored at registration so a lookup is one indexed query rather
/// than a scan that parses every key.
pub fn fingerprints_of(armoured: &str) -> Result<Vec<String>> {
    let (key, _) = SignedPublicKey::from_string(armoured)
        .map_err(|e| crate::err(format!("public key: {e}")))?;
    use pgp::types::KeyDetails;
    let mut out = vec![hex(key.fingerprint().as_bytes())];
    out.extend(
        key.public_subkeys
            .iter()
            .map(|s| hex(s.key.fingerprint().as_bytes())),
    );
    Ok(out)
}

/// Every email this key claims, lowercased.
///
/// A key may carry several user ids; a commit matches if ANY of them is the
/// author. Matching only the first would call a legitimate signature bad_email
/// for anyone who has ever added a second address.
pub fn emails_of(armoured: &str) -> Result<Vec<String>> {
    let (key, _) = SignedPublicKey::from_string(armoured)
        .map_err(|e| crate::err(format!("public key: {e}")))?;
    Ok(key
        .details
        .users
        .iter()
        .filter_map(|u| {
            let id = u.id.id();
            let s = String::from_utf8_lossy(id);
            // `Name <email@host>`, or a bare address.
            s.rsplit_once('<')
                .and_then(|(_, rest)| rest.split_once('>'))
                .map(|(email, _)| email.trim().to_lowercase())
                .or_else(|| s.contains('@').then(|| s.trim().to_lowercase()))
        })
        .collect())
}

/// Check a signature against a registered key.
///
/// Expiry is judged BEFORE the maths: an expired key that still verifies is not
/// a valid signature, and reporting it as good would make the badge a lie about
/// a key its owner has already retired.
pub fn verify(armoured_key: &str, signature: &str, payload: &[u8], author_email: &str) -> Reason {
    let Ok((key, _)) = SignedPublicKey::from_string(armoured_key) else {
        return Reason::UnknownKey;
    };
    let Ok((sig, _)) = DetachedSignature::from_string(signature) else {
        return Reason::UnknownSignatureType;
    };

    // Both judged BEFORE the maths. An expired or revoked key that still verifies
    // is not a good signature, and reporting it as valid would vouch for a key its
    // owner has already retired.
    if !key.details.revocation_signatures.is_empty() {
        return Reason::RevokedKey;
    }
    if is_expired(&key) {
        return Reason::ExpiredKey;
    }

    // The primary key, then each subkey: commits are normally signed by a signing
    // subkey, and only the primary carries the identity.
    let ok = sig.verify(&key.primary_key, payload).is_ok()
        || key
            .public_subkeys
            .iter()
            .any(|s| sig.verify(&s.key, payload).is_ok());
    if !ok {
        return Reason::Invalid;
    }

    let author = author_email.trim().to_lowercase();
    match emails_of(armoured_key) {
        Ok(emails) if emails.iter().any(|e| *e == author) => Reason::Valid,
        _ => Reason::BadEmail,
    }
}

/// Has the primary key passed its expiry?
///
/// OpenPGP stores expiry as a DURATION from the key's creation, not as a date —
/// so this is creation + duration, compared to now. Reading the duration as an
/// absolute timestamp would make every key look decades expired.
fn is_expired(key: &SignedPublicKey) -> bool {
    use pgp::types::KeyDetails;
    let created: std::time::SystemTime = key.primary_key.created_at().into();
    let now = std::time::SystemTime::now();

    // The expiry lives on a self-signature: a direct one, or the binding
    // signature of a user id.
    key.details
        .direct_signatures
        .iter()
        .chain(key.details.users.iter().flat_map(|u| u.signatures.iter()))
        .filter_map(|s| s.key_expiration_time())
        .any(|d| match std::time::Duration::try_from(d) {
            Ok(d) => created + d < now,
            Err(_) => false,
        })
}
