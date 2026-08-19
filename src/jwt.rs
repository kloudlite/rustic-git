//! Identity tokens.
//!
//! The api server mints these; every other service only verifies them. That is
//! the point of using a signed token rather than a header: a caller asserting
//! `x-rustic-git-owner: alice` is only as trustworthy as the caller, so every
//! service that reads it has to hold the peer secret and be trusted not to lie.
//! A signature moves the trust to the key — a service can verify who the user is
//! without being able to mint a different answer.

use crate::{err, Result};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Twelve hours. Long enough that a working day rarely needs a re-issue, short
/// enough that a leaked token is not a permanent credential. There is no
/// revocation list: shortening the life is the whole mitigation, so this cannot
/// grow without adding one.
pub const TTL_SECS: u64 = 12 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Claims {
    /// The user's email — the same identity the directory keys on.
    pub sub: String,
    pub name: String,
    pub iat: u64,
    pub exp: u64,
}

pub struct Jwt {
    encoding: EncodingKey,
    decoding: DecodingKey,
}

impl Jwt {
    pub fn new(secret: &str) -> Result<Jwt> {
        // A short secret is a weak signature, and HS256 gives no warning about it.
        if secret.len() < 32 {
            return Err(err("jwt secret must be at least 32 bytes"));
        }
        Ok(Jwt {
            encoding: EncodingKey::from_secret(secret.as_bytes()),
            decoding: DecodingKey::from_secret(secret.as_bytes()),
        })
    }

    pub fn mint(&self, email: &str, name: &str) -> Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| err("clock before epoch"))?
            .as_secs();
        let claims = Claims {
            sub: email.trim().to_lowercase(),
            name: name.to_string(),
            iat: now,
            exp: now + TTL_SECS,
        };
        encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)
            .map_err(|e| err(format!("minting token: {e}")))
    }

    /// `Err` for a bad signature, a wrong algorithm, or an expired token — the
    /// caller cannot tell which, and should not: each one means "not signed in".
    pub fn verify(&self, token: &str) -> Result<Claims> {
        // Explicitly HS256: leaving the algorithm open is how `alg: none` and
        // key-confusion attacks get in.
        let mut v = Validation::new(Algorithm::HS256);
        v.validate_exp = true;
        decode::<Claims>(token, &self.decoding, &v)
            .map(|d| d.claims)
            .map_err(|e| err(format!("invalid token: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt() -> Jwt {
        Jwt::new("0123456789012345678901234567890123456789").unwrap()
    }

    #[test]
    fn round_trips_and_normalises_the_subject() {
        let t = jwt().mint("Karthik@Kloudlite.io", "Karthik").unwrap();
        let c = jwt().verify(&t).unwrap();
        assert_eq!(c.sub, "karthik@kloudlite.io");
        assert_eq!(c.name, "Karthik");
    }

    #[test]
    fn a_short_secret_is_refused() {
        assert!(Jwt::new("too-short").is_err());
    }

    #[test]
    fn another_key_cannot_verify() {
        let t = jwt().mint("a@b.com", "A").unwrap();
        let other = Jwt::new("abcdefghijabcdefghijabcdefghijabcdefghij").unwrap();
        assert!(other.verify(&t).is_err());
    }

    #[test]
    fn an_expired_token_is_refused() {
        // Mint by hand so the expiry is in the past.
        let past = Claims { sub: "a@b.com".into(), name: "A".into(), iat: 0, exp: 1 };
        let raw = encode(
            &Header::new(Algorithm::HS256),
            &past,
            &EncodingKey::from_secret("0123456789012345678901234567890123456789".as_bytes()),
        )
        .unwrap();
        assert!(jwt().verify(&raw).is_err());
    }

    #[test]
    fn an_unsigned_token_is_refused() {
        // alg: none, the classic forgery.
        let forged = format!(
            "{}.{}.",
            base64_url(br#"{"alg":"none","typ":"JWT"}"#),
            base64_url(br#"{"sub":"admin@x.com","name":"A","iat":0,"exp":99999999999}"#)
        );
        assert!(jwt().verify(&forged).is_err());
    }

    fn base64_url(b: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    }
}
