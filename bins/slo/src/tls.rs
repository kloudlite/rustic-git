//! How long the certificate one hostname serves is still valid for.
//!
//! Read with `openssl` rather than by opening a TLS session in-process on purpose: `edge.cert` is
//! about the certificate the EDGE presents to an ordinary client, and rustls hands back a verified
//! session rather than the leaf's dates — a probe that could only say "the handshake worked" would
//! go green until the morning the certificate expired.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{NaiveDateTime, TimeZone, Utc};

/// `notAfter=Sep 18 12:00:00 2026 GMT` — the one line `openssl x509 -enddate -noout` prints.
///
/// Anything else is an error, never a guessed date: a certificate whose expiry we could not read
/// must fail the step, since "no date" and "a date far away" are the same value to the caller.
pub fn days_left(enddate: &str, now: chrono::DateTime<Utc>) -> Result<i64> {
    let raw = enddate
        .lines()
        .find_map(|l| l.trim().strip_prefix("notAfter="))
        .ok_or_else(|| anyhow!("no notAfter= line in openssl's answer"))?;
    // `%e` is the space-padded day openssl prints for the first nine of a month; the GMT suffix is
    // literal, because openssl prints that zone and no other for a certificate's dates.
    let at = NaiveDateTime::parse_from_str(raw.trim(), "%b %e %H:%M:%S %Y GMT")
        .with_context(|| format!("could not read the expiry {raw:?}"))?;
    Ok((Utc.from_utc_datetime(&at) - now).num_days())
}

/// The certificate `host` serves on 443, as openssl's own `notAfter=` line.
///
/// One `bash -c` with the pipe openssl needs: `s_client` writes the PEM chain to stdout and reads
/// stdin until EOF, so `</dev/null` is what makes it exit at all.
pub async fn enddate(bash: &str, openssl: &str, host: &str, timeout: Duration) -> Result<String> {
    let script = format!(
        "{openssl} s_client -servername {host} -connect {host}:443 </dev/null 2>/dev/null \
         | {openssl} x509 -enddate -noout"
    );
    crate::tools::plain(bash, &["-c", &script], timeout).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_days_parses_openssl_enddate() {
        let now = Utc.with_ymd_and_hms(2026, 9, 5, 0, 0, 0).unwrap();
        assert_eq!(days_left("notAfter=Sep 18 12:00:00 2026 GMT\n", now).unwrap(), 13);
        // The single-digit day openssl space-pads, and an already-expired certificate.
        assert_eq!(days_left("notAfter=Sep  8 00:00:00 2026 GMT", now).unwrap(), 3);
        assert!(days_left("notAfter=Sep  1 00:00:00 2026 GMT", now).unwrap() < 0);
        // Nothing to read is an error, never "far away".
        assert!(days_left("unable to load certificate", now).is_err());
        assert!(days_left("notAfter=nonsense", now).is_err());
    }
}
