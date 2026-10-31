//! Evidence export: signed bundles over runs/effects/events (Phase 1).
//!
//! A JSON export is `{schema, exported_at, run, effects, events,
//! checkpoints, manifest}`. The manifest carries `bundle_sha256`
//! (SHA-256 over the canonical JSON of the bundle without the
//! manifest) and `signature` (HMAC-SHA256 of that digest, keyed by
//! the server's export signing key). Auditors verify offline with
//! `scripts/verify_export.py`, which shares no code with this
//! module: it recomputes the per-run event hash chain, the bundle
//! digest, and (given the key) the signature. Tampering with any
//! exported record breaks at least one of the three.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use calod_ledger::{canonical_json, sha256_digest, RunExport};

/// Export bundle schema identifier.
pub const EXPORT_SCHEMA: &str = "calod.export/1";

/// File holding the hex export signing key inside the ledger dir.
pub const EXPORT_KEY_FILE: &str = "export_signing.key";

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    let bytes = hex.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

/// HMAC-SHA256 (RFC 2104) over `msg`, hex encoded.
pub fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let inner_pad: Vec<u8> = key_block.iter().map(|b| b ^ 0x36).collect();
    let outer_pad: Vec<u8> = key_block.iter().map(|b| b ^ 0x5c).collect();
    let mut inner = Sha256::new();
    inner.update(&inner_pad);
    inner.update(msg);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&outer_pad);
    outer.update(inner_hash);
    hex_encode(&outer.finalize())
}

/// Load the export signing key from `dir` (hex file), creating and
/// persisting a fresh 256-bit key on first use. The file must be
/// protected like any signing secret; it never leaves the server
/// except to auditors verifying exports.
pub fn load_or_create_export_key(dir: &Path) -> std::io::Result<Vec<u8>> {
    let path: PathBuf = dir.join(EXPORT_KEY_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Some(key) = hex_decode(text.trim()) {
            if key.len() >= 16 {
                return Ok(key);
            }
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not a valid hex signing key", path.display()),
        ));
    }
    std::fs::create_dir_all(dir)?;
    let mut key = Vec::with_capacity(32);
    key.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    key.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    std::fs::write(&path, hex_encode(&key))?;
    Ok(key)
}

/// Render a run export as the signed JSON bundle.
pub fn render_json_export(export: &RunExport, signing_key: &[u8], exported_at: &str) -> Value {
    let bundle = json!({
        "schema": EXPORT_SCHEMA,
        "exported_at": exported_at,
        "run": export.run,
        "effects": export.effects,
        "events": export.events,
        "checkpoints": export.checkpoints,
    });
    let bundle_sha256 = sha256_digest(&canonical_json(&bundle));
    let signature = hmac_sha256_hex(signing_key, bundle_sha256.as_bytes());
    let mut out = bundle;
    out["manifest"] = json!({
        "algorithm": "HMAC-SHA256",
        "bundle_sha256": bundle_sha256,
        "signature": signature,
        "records": {
            "effects": export.effects.len(),
            "events": export.events.len(),
            "checkpoints": export.checkpoints.len(),
        },
        "events_head_hash": export.events.last().map(|e| e.hash.clone()),
    });
    out
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// Render a run export as CSV (one row per event; the evidence
/// timeline auditors open in a spreadsheet). Hash fields stay
/// intact so the chain can be re-verified from the CSV alone.
pub fn render_events_csv(export: &RunExport) -> String {
    let mut out =
        String::from("run_id,sequence,type,occurred_at,recorded_at,previous_hash,hash,payload\n");
    for event in &export.events {
        let payload = String::from_utf8_lossy(&canonical_json(&event.payload)).into_owned();
        let row = [
            event.run_id.to_string(),
            event.sequence.to_string(),
            event.event_type.clone(),
            event.occurred_at.clone(),
            event.recorded_at.clone(),
            event.previous_hash.clone(),
            event.hash.clone(),
            payload,
        ]
        .iter()
        .map(|f| csv_field(f))
        .collect::<Vec<_>>()
        .join(",");
        out.push_str(&row);
        out.push('\n');
    }
    out
}

/// Digest + signature headers for a CSV export.
pub fn csv_manifest(csv: &str, signing_key: &[u8]) -> (String, String) {
    let digest = sha256_digest(csv.as_bytes());
    let signature = hmac_sha256_hex(signing_key, digest.as_bytes());
    (digest, signature)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case1() {
        // RFC 4231 test case 1: key = 20 x 0x0b, data = "Hi There".
        let key = vec![0x0bu8; 20];
        assert_eq!(
            hmac_sha256_hex(&key, b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }
}
