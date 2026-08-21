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
///
/// A user id is free text the holder controls, so only one whose SELF-SIGNATURE
/// verifies against the primary key is trusted: without that check a third party
/// could staple someone else's address onto a key and have us vouch for it.
pub fn emails_of(armoured: &str) -> Result<Vec<String>> {
    let (key, _) = SignedPublicKey::from_string(armoured)
        .map_err(|e| crate::err(format!("public key: {e}")))?;
    Ok(verified_emails(&key))
}

fn verified_emails(key: &SignedPublicKey) -> Vec<String> {
    key.details
        .users
        .iter()
        // `verify_bindings` checks every certification on the user id against the
        // primary key (and fails an id carrying none).
        .filter(|u| u.verify_bindings(&key.primary_key).is_ok())
        .filter_map(|u| {
            let id = u.id.id();
            let s = String::from_utf8_lossy(id);
            // `Name <email@host>`, or a bare address.
            s.rsplit_once('<')
                .and_then(|(_, rest)| rest.split_once('>'))
                .map(|(email, _)| email.trim().to_lowercase())
                .or_else(|| s.contains('@').then(|| s.trim().to_lowercase()))
        })
        .collect()
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
    match validity(&key, std::time::SystemTime::now()) {
        Validity::Revoked => return Reason::RevokedKey,
        Validity::Expired => return Reason::ExpiredKey,
        Validity::Valid => {}
    }

    // The primary key, then each subkey WHOSE BINDING VERIFIES: a subkey with no
    // valid binding signature is not part of this key, and (for a signing subkey)
    // the embedded back-signature is what proves the subkey agreed to be bound —
    // without both checks an attacker could graft any subkey under a trusted
    // primary. Commits are normally signed by a signing subkey.
    let ok = sig.verify(&key.primary_key, payload).is_ok()
        || key
            .public_subkeys
            .iter()
            .filter(|s| s.verify_bindings(&key.primary_key).is_ok())
            .any(|s| sig.verify(&s.key, payload).is_ok());
    if !ok {
        return Reason::Invalid;
    }

    let author = author_email.trim().to_lowercase();
    if verified_emails(&key).contains(&author) {
        Reason::Valid
    } else {
        Reason::BadEmail
    }
}

/// The trust state of the primary key, judged before any signature maths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Validity {
    Valid,
    Expired,
    Revoked,
}

/// Is the key revoked or expired at `now`?
///
/// Revocation is honoured only when the revocation signature VERIFIES against the
/// key it revokes: an unverified revocation packet is anyone's to forge, and
/// treating it as authoritative is a denial-of-service on the real owner.
fn validity(key: &SignedPublicKey, now: std::time::SystemTime) -> Validity {
    if key
        .details
        .revocation_signatures
        .iter()
        .any(|s| s.verify_key(&key.primary_key).is_ok())
    {
        return Validity::Revoked;
    }

    if let Some(d) = effective_expiry(key) {
        use pgp::types::KeyDetails;
        let created: std::time::SystemTime = key.primary_key.created_at().into();
        // Expiry is a DURATION from key creation, not an absolute date; reading it
        // as a timestamp would make every key look decades expired.
        let d = std::time::Duration::from(d);
        if created + d < now {
            return Validity::Expired;
        }
    }
    Validity::Valid
}

/// The key-expiry duration on the NEWEST valid self-signature, if any.
///
/// GPG semantics: the most recent self-signature wins, so a later one that
/// extends (or removes) the expiry supersedes an earlier short one. The old code
/// tripped on ANY duration ever set, so extending a key still read as expired.
/// Only self-signatures that actually verify are considered — an unverified one
/// must not get a vote on the key's lifetime.
fn effective_expiry(key: &SignedPublicKey) -> Option<pgp::types::Duration> {
    use pgp::types::Tag;
    let primary = &key.primary_key;

    let direct = key
        .details
        .direct_signatures
        .iter()
        .filter(|s| s.verify_key(primary).is_ok());
    let uid = key.details.users.iter().flat_map(|u| {
        u.signatures
            .iter()
            .filter(move |s| s.verify_certification(primary, Tag::UserId, &u.id).is_ok())
    });

    direct
        .chain(uid)
        .filter_map(|s| Some((s.created()?, s.key_expiration_time())))
        .max_by_key(|(created, _)| *created)
        .and_then(|(_, expiry)| expiry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pgp::composed::{
        EncryptionCaps, KeyType, SecretKeyParamsBuilder, SignedSecretKey, SubkeyParamsBuilder,
    };
    use pgp::packet::{SignatureConfig, SignatureType, Subpacket, SubpacketData};
    use pgp::types::{Duration as PgpDuration, KeyDetails, Password, Timestamp};
    use std::time::{Duration, SystemTime};

    /// Subkeys with a valid binding — the only ones a signature may ride on.
    fn signing_capable_subkeys(key: &SignedPublicKey) -> Vec<String> {
        key.public_subkeys
            .iter()
            .filter(|s| s.verify_bindings(&key.primary_key).is_ok())
            .map(|s| hex(s.key.fingerprint().as_bytes()))
            .collect()
    }

    fn gen(uid: &str, created: SystemTime) -> SignedSecretKey {
        let mut sub = SubkeyParamsBuilder::default();
        sub.key_type(KeyType::Ed25519Legacy)
            .can_sign(true)
            .can_encrypt(EncryptionCaps::None)
            .can_authenticate(false);
        let mut params = SecretKeyParamsBuilder::default();
        params
            .key_type(KeyType::Ed25519Legacy)
            .can_certify(true)
            .can_sign(false)
            .can_encrypt(EncryptionCaps::None)
            .created_at(Timestamp::try_from(created).unwrap())
            .primary_user_id(uid.into())
            .subkeys(vec![sub.build().unwrap()]);
        params
            .build()
            .unwrap()
            .generate(rand::thread_rng())
            .unwrap()
    }

    // Primary of one key, subkey of another: the subkey's binding verifies against
    // the FOREIGN primary, so it is not a signer for this key.
    fn key_with_unbound_subkey() -> (SignedPublicKey, String) {
        let mine: SignedPublicKey = gen("a@example.com", SystemTime::now()).into();
        let other: SignedPublicKey = gen("b@example.com", SystemTime::now()).into();
        let stolen = other.public_subkeys[0].clone();
        let unbound_id = hex(stolen.key.fingerprint().as_bytes());
        let mut tampered = mine;
        tampered.public_subkeys = vec![stolen];
        (tampered, unbound_id)
    }

    // Two self-signatures: an old one capping the key at 1y, a newer one extending
    // it to 10y. The key is 2y old, so the old-cap logic would call it expired.
    fn key_expiry_extended() -> SignedPublicKey {
        let two_years = Duration::from_secs(2 * 365 * 86400);
        let one_year = Duration::from_secs(365 * 86400);
        let mut sk = gen("c@example.com", SystemTime::now() - two_years);

        let sign = |sk: &SignedSecretKey, at: SystemTime, expiry_secs: u32| {
            let mut cfg =
                SignatureConfig::from_key(rand::thread_rng(), &sk.primary_key, SignatureType::Key)
                    .unwrap();
            cfg.hashed_subpackets = vec![
                Subpacket::regular(SubpacketData::SignatureCreationTime(
                    Timestamp::try_from(at).unwrap(),
                ))
                .unwrap(),
                Subpacket::regular(SubpacketData::IssuerFingerprint(sk.primary_key.fingerprint()))
                    .unwrap(),
                Subpacket::regular(SubpacketData::KeyExpirationTime(PgpDuration::from_secs(
                    expiry_secs,
                )))
                .unwrap(),
            ];
            cfg.sign_key(&sk.primary_key, &Password::empty(), &sk.primary_key.public_key())
                .unwrap()
        };

        let now = SystemTime::now();
        let old = sign(&sk, now - two_years, 365 * 86400);
        let new = sign(&sk, now - one_year, 10 * 365 * 86400);
        sk.details.direct_signatures.push(old);
        sk.details.direct_signatures.push(new);
        sk.into()
    }

    #[test]
    fn subkey_without_valid_binding_is_not_a_signer() {
        let (key, unbound) = key_with_unbound_subkey();
        assert!(!signing_capable_subkeys(&key).iter().any(|s| *s == unbound));
    }

    #[test]
    fn bound_subkey_is_a_signer() {
        let key: SignedPublicKey = gen("d@example.com", SystemTime::now()).into();
        assert_eq!(signing_capable_subkeys(&key).len(), 1);
    }

    #[test]
    fn newest_self_sig_expiry_wins() {
        let key = key_expiry_extended();
        assert_eq!(validity(&key, SystemTime::now()), Validity::Valid);
    }

    #[test]
    fn unverified_revocation_is_ignored() {
        // A revocation from a foreign key must not revoke this one.
        let mut mine: SignedPublicKey = gen("e@example.com", SystemTime::now()).into();
        let other = gen("f@example.com", SystemTime::now());
        let mut cfg = SignatureConfig::from_key(
            rand::thread_rng(),
            &other.primary_key,
            SignatureType::KeyRevocation,
        )
        .unwrap();
        cfg.hashed_subpackets = vec![
            Subpacket::regular(SubpacketData::SignatureCreationTime(Timestamp::now())).unwrap(),
            Subpacket::regular(SubpacketData::IssuerFingerprint(other.primary_key.fingerprint()))
                .unwrap(),
        ];
        let forged = cfg
            .sign_key(&other.primary_key, &Password::empty(), &mine.primary_key)
            .unwrap();
        mine.details.revocation_signatures.push(forged);
        assert_eq!(validity(&mine, SystemTime::now()), Validity::Valid);
    }

    #[test]
    fn only_verified_uid_emails() {
        let key: SignedPublicKey = gen("g@example.com", SystemTime::now()).into();
        assert_eq!(verified_emails(&key), vec!["g@example.com".to_string()]);
    }
}
