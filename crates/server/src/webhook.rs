//! Webhook intake: verify the HMAC before parsing anything, store the
//! delivery, answer at once. Nothing slow happens here.

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// Verify `X-Hub-Signature-256: sha256=<hex>` over the raw body.
pub fn verify_signature(secret: &[u8], body: &[u8], header: &str) -> bool {
    let Some(hex_sig) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(given) = hex::decode(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    let expected = mac.finalize().into_bytes();
    expected.ct_eq(given.as_slice()).into()
}

/// The signature GitHub would send for `body`; used by tests and replay.
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("hmac accepts any key");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// The thread a delivery names directly, when it names one. Commit-level
/// events are resolved later by a worker.
pub fn thread_of(event: &str, payload: &Value) -> Option<i64> {
    match event {
        "pull_request"
        | "pull_request_review"
        | "pull_request_review_comment"
        | "pull_request_review_thread" => payload["pull_request"]["number"].as_i64(),
        "issue_comment" | "issues" => payload["issue"]["number"].as_i64(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_and_rejects_invalid_signatures() {
        let body = b"payload-bytes";
        let good = sign(b"s3cret", body);
        assert!(verify_signature(b"s3cret", body, &good));
        assert!(!verify_signature(b"wrong", body, &good));
        assert!(!verify_signature(b"s3cret", b"other body", &good));
        assert!(!verify_signature(b"s3cret", body, "sha256=deadbeef"));
        assert!(!verify_signature(b"s3cret", body, "sha1=whatever"));
        assert!(!verify_signature(b"s3cret", body, "sha256=nothex!"));
        assert!(!verify_signature(b"s3cret", body, ""));
    }
}
