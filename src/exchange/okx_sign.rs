//! okx_sign.rs - OKX V5 HMAC-SHA256 signing (base64 output).
//! prehash = timestamp(ISO8601) + METHOD + requestPath(+query) + body
//! headers: OK-ACCESS-KEY / OK-ACCESS-SIGN / OK-ACCESS-TIMESTAMP / OK-ACCESS-PASSPHRASE
//! demo mode adds: x-simulated-trading: 1

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub const DEMO_HEADER: &str = "x-simulated-trading";

/// ISO8601 UTC with milliseconds: 2026-01-01T00:00:00.123Z
pub fn iso_timestamp() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

pub fn sign(secret: &str, timestamp_iso: &str, method: &str, request_path: &str, body: &str) -> String {
    let payload = format!("{}{}{}{}", timestamp_iso, method.to_uppercase(), request_path, body);
    sign_hmac(secret, &payload)
}

pub fn sign_hmac(secret: &str, payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC can take key of any size");
    mac.update(payload.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_format() {
        let s = sign("secret", "2026-01-01T00:00:00.000Z", "GET", "/api/v5/account/balance?ccy=USDT", "");
        assert!(!s.is_empty());
        assert!(s.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='));
    }

    #[test]
    fn test_iso_format() {
        let ts = iso_timestamp();
        assert_eq!(ts.len(), 24);
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[10..11], "T");
    }
}