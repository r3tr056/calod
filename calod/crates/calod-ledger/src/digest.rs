//! Canonical JSON rendering and SHA-256 digests.
//!
//! The idempotency input digest, the event hash chain, and checkpoint state
//! digests all depend on one deterministic byte representation of a JSON
//! value: object keys sorted recursively, no insignificant whitespace,
//! numbers in serde_json's shortest round-trip form, strings JSON-escaped.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Hex-encoded SHA-256 of the empty byte string; used as the
/// `previous_hash` of every run's first event (sequence 1).
pub const GENESIS_HASH: &str =
    "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Append the canonical byte representation of `value` to `out`.
pub fn canonical_json_into(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Number(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::String(s) => {
            // serde_json's string rendering is the minimal JSON escaping.
            let encoded = serde_json::to_string(s).expect("string serialization is infallible");
            out.extend_from_slice(encoded.as_bytes());
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                canonical_json_into(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            out.push(b'{');
            // Sort explicitly: canonical form must not depend on the
            // underlying map's iteration order (preserve_order feature).
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                let encoded_key =
                    serde_json::to_string(key).expect("string serialization is infallible");
                out.extend_from_slice(encoded_key.as_bytes());
                out.push(b':');
                canonical_json_into(&map[*key], out);
            }
            out.push(b'}');
        }
    }
}

/// The canonical byte representation of `value`.
pub fn canonical_json(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    canonical_json_into(value, &mut out);
    out
}

/// Lowercase hex encoding.
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Decode 64 lowercase hex characters into 32 bytes.
pub(crate) fn hex_decode_32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, chunk) in hex.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[index] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// SHA-256 of `bytes`, rendered `sha256:<64 lowercase hex>`.
pub fn sha256_digest(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    format!("sha256:{}", hex_encode(&hash))
}

/// Strip the `sha256:` prefix and decode the 32 raw digest bytes.
pub(crate) fn digest_bytes(rendered: &str) -> Option<[u8; 32]> {
    hex_decode_32(rendered.strip_prefix("sha256:")?)
}

/// Idempotency input digest: `sha256:<hex>` over the canonical bytes of the
/// canonical input JSON.
pub fn input_digest(input: &Value) -> String {
    sha256_digest(&canonical_json(input))
}

/// Render milliseconds since the Unix epoch as RFC 3339 (`YYYY-MM-DDTHH:MM:SS[.mmm]Z`).
/// Millisecond precision is preserved, so the rendering round-trips.
pub fn format_timestamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let sub_ms = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    if sub_ms == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{sub_ms:03}Z")
    }
}

/// Howard Hinnant's civil-from-days algorithm: days since 1970-01-01 →
/// (year, month, day) in the proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_json_sorts_keys_recursively() {
        let a = json!({"b": 1, "a": {"d": [3, {"y": 2, "x": 1}], "c": 2}});
        let b = json!({"a": {"c": 2, "d": [3, {"x": 1, "y": 2}]}, "b": 1});
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(
            String::from_utf8(canonical_json(&a)).unwrap(),
            r#"{"a":{"c":2,"d":[3,{"x":1,"y":2}]},"b":1}"#
        );
    }

    #[test]
    fn canonical_json_is_whitespace_insensitive() {
        let compact: Value = serde_json::from_str(r#"{"a":1,"b":[1,2]}"#).unwrap();
        let pretty: Value = serde_json::from_str("{ \"a\" : 1, \"b\" : [ 1, 2 ] }").unwrap();
        assert_eq!(input_digest(&compact), input_digest(&pretty));
        assert!(input_digest(&compact).starts_with("sha256:"));
        assert_eq!(input_digest(&compact).len(), 7 + 64);
    }

    #[test]
    fn canonical_json_distinguishes_values() {
        assert_ne!(canonical_json(&json!(1)), canonical_json(&json!("1")));
        assert_ne!(canonical_json(&json!(1.0)), canonical_json(&json!(1)));
        assert_ne!(
            canonical_json(&json!({"a": 1})),
            canonical_json(&json!({"a": 2}))
        );
    }

    #[test]
    fn genesis_hash_matches_sha256_of_empty() {
        assert_eq!(sha256_digest(b""), GENESIS_HASH);
    }

    #[test]
    fn digest_bytes_round_trip() {
        let rendered = sha256_digest(b"hello");
        let raw = digest_bytes(&rendered).expect("decode");
        assert_eq!(raw.to_vec(), Sha256::digest(b"hello").to_vec());
        assert!(digest_bytes("sha256:zz").is_none());
        assert!(digest_bytes("md5:0000").is_none());
    }

    #[test]
    fn timestamp_formatting() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_timestamp(1_753_296_600_000), "2025-07-23T18:50:00Z");
        assert_eq!(
            format_timestamp(1_753_296_600_042),
            "2025-07-23T18:50:00.042Z"
        );
        // Negative times (pre-epoch) stay correct.
        assert_eq!(format_timestamp(-1), "1969-12-31T23:59:59.999Z");
    }
}
