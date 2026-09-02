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

use crate::{hex, Result};
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

/// Parse an armoured detached signature once, so a caller that needs both the issuers and the
/// verification result (`verify_signature`) does not pay the parse twice per request.
pub fn parse_signature(signature: &str) -> Result<DetachedSignature> {
    Ok(DetachedSignature::from_string(signature)
        .map_err(|e| crate::err(format!("signature: {e}")))?
        .0)
}

/// The fingerprints a signature says made it, longest-lived first.
///
/// A signature names its issuer by fingerprint, or on older keys only by key id
/// (the last eight bytes of the fingerprint). Both are returned as lowercase hex
/// so a lookup can match either against a registered key.
pub fn issuers(sig: &DetachedSignature) -> Vec<String> {
    let mut out: Vec<String> = sig
        .signature
        .issuer_fingerprint()
        .into_iter()
        .map(|f| hex(f.as_bytes()))
        .collect();
    out.extend(sig.signature.issuer_key_id().into_iter().map(|k| hex(k.as_ref())));
    out
}

/// Parse an armoured OpenPGP public key once, so a caller that needs more than one fact about it
/// (fingerprints, emails, verification) does not pay the parse — and the self-signature checks
/// inside it — more than once per request.
pub fn parse_key(armoured: &str) -> Result<SignedPublicKey> {
    Ok(SignedPublicKey::from_string(armoured)
        .map_err(|e| crate::err(format!("public key: {e}")))?
        .0)
}

/// The primary key's fingerprint, plus every subkey's — what a registered key
/// answers to. Also includes each fingerprint's 16-hex key-id suffix (the last
/// eight bytes), because `issuers` above returns bare key ids for signatures
/// that don't name a full fingerprint; indexing the suffix at registration
/// keeps the lookup a single `$in` rather than a suffix scan. Stored at
/// registration so a lookup is one indexed query rather than a scan that
/// parses every key.
pub fn fingerprints_of(key: &SignedPublicKey) -> Vec<String> {
    use pgp::types::KeyDetails;
    let mut full = vec![hex(key.fingerprint().as_bytes())];
    full.extend(
        key.public_subkeys
            .iter()
            .map(|s| hex(s.key.fingerprint().as_bytes())),
    );
    let mut out = full.clone();
    out.extend(full.iter().filter(|f| f.len() > 16).map(|f| f[f.len() - 16..].to_string()));
    out
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
pub fn verified_emails(key: &SignedPublicKey) -> Vec<String> {
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
/// a key its owner has already retired. The signature's own timestamps are checked
/// there too — when it was made against when the key existed and expired, and its
/// own expiry against now.
pub fn verify(armoured_key: &str, sig: &DetachedSignature, payload: &[u8], author_email: &str) -> Reason {
    let Ok(key) = parse_key(armoured_key) else {
        return Reason::UnknownKey;
    };

    let now = std::time::SystemTime::now();

    // Both judged BEFORE the maths. An expired or revoked key that still verifies
    // is not a good signature, and reporting it as valid would vouch for a key its
    // owner has already retired.
    match validity(&key, now) {
        Validity::Revoked => return Reason::RevokedKey,
        Validity::Expired => return Reason::ExpiredKey,
        Validity::Valid => {}
    }

    // Judged at the moment the signature was MADE, not only now. One dated before its key
    // existed can only be forged or misattributed; one dated past the key's expiry was made with
    // a retired key however the clock reads today; one carrying its own expiry that has passed
    // says, in the signer's words, not to trust it any more.
    use pgp::types::KeyDetails;
    let key_created: std::time::SystemTime = key.primary_key.created_at().into();
    let Some(made) = sig.signature.created() else {
        return Reason::Invalid;
    };
    let made: std::time::SystemTime = made.into();
    if made < key_created {
        return Reason::Invalid;
    }
    if let Some(d) = effective_expiry(&key) {
        if key_created + std::time::Duration::from(d) < made {
            return Reason::ExpiredKey;
        }
    }
    if let Some(d) = sig.signature.signature_expiration_time() {
        if made + std::time::Duration::from(d) < now {
            return Reason::Invalid;
        }
    }

    // The primary key, then each subkey that is bound AND still live: a subkey with
    // no valid binding signature is not part of this key, and (for a signing subkey)
    // the embedded back-signature is what proves the subkey agreed to be bound —
    // without both checks an attacker could graft any subkey under a trusted
    // primary. `subkey_live` additionally rejects a revoked or self-expired subkey,
    // which the binding crypto alone does not. Commits are normally signed by a
    // signing subkey.
    let ok = sig.verify(&key.primary_key, payload).is_ok()
        || key
            .public_subkeys
            .iter()
            .filter(|s| s.verify_bindings(&key.primary_key).is_ok() && subkey_live(s, &key.primary_key, now))
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

    // A direct-key signature outranks any user-id self-signature (RFC 9580 §5.2.3.10), whatever
    // the timestamps say: key generation stamps the uid self-signature at the wall clock, so
    // ranking purely by creation time let a uid binding silence the key's own expiry.
    fn newest<'a>(
        it: impl Iterator<Item = &'a pgp::packet::Signature>,
    ) -> Option<&'a pgp::packet::Signature> {
        it.filter_map(|s| Some((s.created()?, s))).max_by_key(|(c, _)| *c).map(|(_, s)| s)
    }
    let picked = newest(direct).or_else(|| newest(uid));
    picked.and_then(|s| s.key_expiration_time())
}

/// Is a signing subkey live at `now`: bound, not revoked, not past its OWN expiry?
///
/// `verify_bindings` proves the binding crypto and the back-signature, but it
/// treats a `SubkeyRevocation` as just another satisfying "binding" and never
/// looks at the subkey's own `KeyExpirationTime` — which lives on the binding
/// signature, not on the primary. A commit signed under an expired or revoked
/// signing subkey must not read as valid, so both are enforced here. Newest valid
/// binding wins, matching the primary-key expiry semantics.
fn subkey_live(
    subkey: &pgp::composed::SignedPublicSubKey,
    primary: &pgp::packet::PublicKey,
    now: std::time::SystemTime,
) -> bool {
    use pgp::packet::SignatureType;
    use pgp::types::{Duration, KeyDetails, Timestamp};

    let created: std::time::SystemTime = subkey.key.created_at().into();
    let mut newest: Option<(Timestamp, Option<Duration>)> = None;
    for sig in &subkey.signatures {
        if sig.verify_subkey_binding(primary, &subkey.key).is_err() {
            continue;
        }
        match sig.typ() {
            Some(SignatureType::SubkeyRevocation) => return false,
            Some(SignatureType::SubkeyBinding) => {
                if let Some(c) = sig.created() {
                    if newest.is_none_or(|(nc, _)| c > nc) {
                        newest = Some((c, sig.key_expiration_time()));
                    }
                }
            }
            _ => {}
        }
    }
    match newest {
        Some((_, Some(d))) => created + std::time::Duration::from(d) >= now,
        Some((_, None)) => true,
        None => false,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use pgp::composed::{
        EncryptionCaps, KeyType, SecretKeyParamsBuilder, SignedSecretKey, SubkeyParamsBuilder,
    };
    use pgp::composed::{ArmorOptions, DetachedSignature};
    use pgp::crypto::hash::HashAlgorithm;
    use pgp::packet::{KeyFlags, SignatureConfig, SignatureType, Subpacket, SubpacketData};
    use pgp::types::{Duration as PgpDuration, KeyDetails, Password, Timestamp};
    use std::time::{Duration, SystemTime};

    // Rebuild the signing subkey's binding with a chosen creation time and expiry
    // (and optionally a valid SubkeyRevocation on top), returning the public key.
    // A fresh back-signature keeps the binding acceptable to `verify_bindings`, so
    // the test isolates the subkey-own-validity checks.
    pub(crate) fn reforge_subkey(
        sk: &SignedSecretKey,
        created: SystemTime,
        expiry_secs: Option<u32>,
        revoke: bool,
    ) -> SignedPublicKey {
        let primary = &sk.primary_key;
        let primary_pub = primary.public_key();
        let sub = &sk.secret_subkeys[0];
        let sub_pub = sub.key.public_key();

        let backsig = sub
            .key
            .sign_primary_key_binding(rand::thread_rng(), &primary_pub, &Password::empty())
            .unwrap();

        let mut flags = KeyFlags::default();
        flags.set_sign(true);
        let mut subpkts = vec![
            Subpacket::regular(SubpacketData::SignatureCreationTime(
                Timestamp::try_from(created).unwrap(),
            ))
            .unwrap(),
            Subpacket::regular(SubpacketData::IssuerFingerprint(primary.fingerprint())).unwrap(),
            Subpacket::regular(SubpacketData::KeyFlags(flags)).unwrap(),
            Subpacket::regular(SubpacketData::EmbeddedSignature(Box::new(backsig))).unwrap(),
        ];
        if let Some(e) = expiry_secs {
            subpkts.push(
                Subpacket::regular(SubpacketData::KeyExpirationTime(PgpDuration::from_secs(e)))
                    .unwrap(),
            );
        }
        let mut cfg =
            SignatureConfig::from_key(rand::thread_rng(), primary, SignatureType::SubkeyBinding)
                .unwrap();
        cfg.hashed_subpackets = subpkts;
        let binding = cfg
            .sign_subkey_binding(primary, &primary_pub, &Password::empty(), &sub_pub)
            .unwrap();

        let mut sigs = vec![binding];
        if revoke {
            let mut rcfg = SignatureConfig::from_key(
                rand::thread_rng(),
                primary,
                SignatureType::SubkeyRevocation,
            )
            .unwrap();
            rcfg.hashed_subpackets = vec![
                Subpacket::regular(SubpacketData::SignatureCreationTime(Timestamp::now())).unwrap(),
                Subpacket::regular(SubpacketData::IssuerFingerprint(primary.fingerprint()))
                    .unwrap(),
            ];
            let rev = rcfg
                .sign_subkey_binding(primary, &primary_pub, &Password::empty(), &sub_pub)
                .unwrap();
            sigs.push(rev);
        }

        let mut pk: SignedPublicKey = sk.clone().into();
        pk.public_subkeys[0].signatures = sigs;
        pk
    }

    // A detached binary signature over `payload`, made by the key's signing subkey.
    pub(crate) fn subkey_signature(sk: &SignedSecretKey, payload: &[u8]) -> String {
        DetachedSignature::sign_binary_data(
            rand::thread_rng(),
            &sk.secret_subkeys[0].key,
            &Password::empty(),
            HashAlgorithm::Sha256,
            payload,
        )
        .unwrap()
        .to_armored_string(ArmorOptions::default())
        .unwrap()
    }

    /// Subkeys with a valid binding — the only ones a signature may ride on.
    fn signing_capable_subkeys(key: &SignedPublicKey) -> Vec<String> {
        key.public_subkeys
            .iter()
            .filter(|s| s.verify_bindings(&key.primary_key).is_ok())
            .map(|s| hex(s.key.fingerprint().as_bytes()))
            .collect()
    }

    pub(crate) fn gen(uid: &str, created: SystemTime) -> SignedSecretKey {
        let mut sub = SubkeyParamsBuilder::default();
        sub.key_type(KeyType::Ed25519Legacy)
            .can_sign(true)
            .can_encrypt(EncryptionCaps::None)
            .can_authenticate(false)
            .created_at(Timestamp::try_from(created).unwrap());
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
        assert!(!signing_capable_subkeys(&key).contains(&unbound));
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

    #[test]
    fn foreign_signed_uid_email_is_not_returned() {
        // Graft a user id self-signed by a FOREIGN key: its email must not appear.
        let mut mine: SignedPublicKey = gen("h@example.com", SystemTime::now()).into();
        let other: SignedPublicKey = gen("evil@example.com", SystemTime::now()).into();
        mine.details.users = other.details.users.clone();
        assert!(!verified_emails(&mine).contains(&"evil@example.com".to_string()));
    }

    #[test]
    fn expired_signing_subkey_does_not_verify() {
        // Subkey binding created 2y ago, self-expiring after 1y: expired now.
        let two_years = Duration::from_secs(2 * 365 * 86400);
        let sk = gen("i@example.com", SystemTime::now() - two_years);
        let pk = reforge_subkey(&sk, SystemTime::now() - two_years, Some(365 * 86400), false);
        let armored = pk.to_armored_string(ArmorOptions::default()).unwrap();
        let payload = b"commit body";
        let sig = parse_signature(&subkey_signature(&sk, payload)).unwrap();
        assert_ne!(
            verify(&armored, &sig, payload, "i@example.com"),
            Reason::Valid
        );
    }

    #[test]
    fn revoked_signing_subkey_does_not_verify() {
        let sk = gen("j@example.com", SystemTime::now());
        let pk = reforge_subkey(&sk, SystemTime::now(), None, true);
        let armored = pk.to_armored_string(ArmorOptions::default()).unwrap();
        let payload = b"commit body";
        let sig = parse_signature(&subkey_signature(&sk, payload)).unwrap();
        assert_ne!(
            verify(&armored, &sig, payload, "j@example.com"),
            Reason::Valid
        );
    }

    #[test]
    fn fingerprints_of_includes_key_id_suffix() {
        // `directory::signer_by_any` does an exact `$in` lookup; a signature
        // naming its issuer by bare 16-hex key id (Task 19) only finds this
        // key if registration indexed that suffix alongside the full
        // fingerprint.
        let key: SignedPublicKey = gen("m@example.com", SystemTime::now()).into();
        let armored = key.to_armored_string(ArmorOptions::default()).unwrap();
        let full = hex(key.primary_key.fingerprint().as_bytes());
        let all = fingerprints_of(&parse_key(&armored).unwrap());
        assert!(all.contains(&full), "full fingerprint still present: {all:?}");
        let suffix = &full[full.len() - 16..];
        assert!(all.contains(&suffix.to_string()), "16-hex key id suffix indexed: {all:?}");
    }

    #[test]
    fn live_signing_subkey_still_verifies() {
        // Control: a freshly-bound, non-expired subkey signature is Valid.
        let sk = gen("k@example.com", SystemTime::now());
        let pk = reforge_subkey(&sk, SystemTime::now(), Some(10 * 365 * 86400), false);
        let armored = pk.to_armored_string(ArmorOptions::default()).unwrap();
        let payload = b"commit body";
        let sig = parse_signature(&subkey_signature(&sk, payload)).unwrap();
        assert_eq!(verify(&armored, &sig, payload, "k@example.com"), Reason::Valid);
    }

    #[test]
    fn a_signature_that_predates_its_key_is_invalid() {
        // The key comes into existence tomorrow; the signature is made now.
        let sk = gen("l@example.com", SystemTime::now() + Duration::from_secs(86_400));
        let pk: SignedPublicKey = sk.clone().into();
        let armored = pk.to_armored_string(ArmorOptions::default()).unwrap();
        let payload = b"commit body";
        let sig = parse_signature(&subkey_signature(&sk, payload)).unwrap();
        assert_eq!(verify(&armored, &sig, payload, "l@example.com"), Reason::Invalid);
    }

    #[test]
    fn a_signature_past_its_own_expiry_is_invalid() {
        use pgp::composed::SubpacketConfig;
        let sk = gen("n@example.com", SystemTime::now() - Duration::from_secs(30 * 86_400));
        let pk: SignedPublicKey = sk.clone().into();
        let armored = pk.to_armored_string(ArmorOptions::default()).unwrap();
        let payload = b"commit body";
        let signer = &sk.secret_subkeys[0].key;
        // Made two days ago, valid for one.
        let hashed = vec![
            Subpacket::regular(SubpacketData::SignatureCreationTime(
                Timestamp::try_from(SystemTime::now() - Duration::from_secs(2 * 86_400)).unwrap(),
            ))
            .unwrap(),
            Subpacket::regular(SubpacketData::IssuerFingerprint(signer.fingerprint())).unwrap(),
            Subpacket::regular(SubpacketData::SignatureExpirationTime(PgpDuration::from_secs(86_400))).unwrap(),
        ];
        let sig = DetachedSignature::sign_binary_data_with_subpackets(
            rand::thread_rng(),
            signer,
            &Password::empty(),
            HashAlgorithm::Sha256,
            &payload[..],
            SubpacketConfig::UserDefined { hashed, unhashed: vec![] },
        )
        .unwrap()
        .to_armored_string(ArmorOptions::default())
        .unwrap();
        let sig = parse_signature(&sig).unwrap();
        assert_eq!(verify(&armored, &sig, payload, "n@example.com"), Reason::Invalid);
    }

    #[test]
    fn a_signature_made_after_the_key_expired_is_expired_key() {
        // Key created 2y ago with a 1y expiry (so expired now); the signature is dated 18
        // months ago — inside "expired" territory even though nobody has moved the clock.
        let two_years = Duration::from_secs(2 * 365 * 86_400);
        let mut sk = gen("o@example.com", SystemTime::now() - two_years);
        let mut cfg = SignatureConfig::from_key(rand::thread_rng(), &sk.primary_key, SignatureType::Key).unwrap();
        cfg.hashed_subpackets = vec![
            Subpacket::regular(SubpacketData::SignatureCreationTime(
                Timestamp::try_from(SystemTime::now() - two_years).unwrap(),
            ))
            .unwrap(),
            Subpacket::regular(SubpacketData::IssuerFingerprint(sk.primary_key.fingerprint())).unwrap(),
            Subpacket::regular(SubpacketData::KeyExpirationTime(PgpDuration::from_secs(365 * 86_400))).unwrap(),
        ];
        let direct = cfg.sign_key(&sk.primary_key, &Password::empty(), &sk.primary_key.public_key()).unwrap();
        sk.details.direct_signatures.push(direct);
        let pk: SignedPublicKey = sk.clone().into();
        let armored = pk.to_armored_string(ArmorOptions::default()).unwrap();
        let sig = parse_signature(&subkey_signature(&sk, b"commit body")).unwrap();
        assert_eq!(verify(&armored, &sig, b"commit body", "o@example.com"), Reason::ExpiredKey);
    }
}
