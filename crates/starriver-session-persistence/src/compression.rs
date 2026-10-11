//! Threshold zstd compression for large event `data` bodies (WI-26).
//!
//! Rows whose serialized JSON reaches [`COMPRESS_THRESHOLD_BYTES`] are stored
//! as `zstd1:<base64(zstd frame)>` directly in the TEXT column — a prefix
//! marker instead of a schema change, so old databases (plain JSON, no
//! marker) stay readable and the schema itself never moves. Payloads that
//! do not shrink stay verbatim; incompressible data is never enlarged.
//! Reads are transparent: [`decode_data_text`] strips and inflates marked
//! rows and passes everything else through.

use base64::Engine as _;

/// Serialization threshold before compression is attempted.
pub const COMPRESS_THRESHOLD_BYTES: usize = 2048;
/// Compression level (zstd default-fast sweet spot).
const LEVEL: i32 = 3;
/// Marker prefix riding the TEXT column; `1` is the frame format version.
pub const MARKER: &str = "zstd1:";

/// Serialize for storage: compress large texts, pass small or
/// incompressible ones through verbatim.
pub fn encode_data_text(json_text: &str) -> Result<String, String> {
    let bytes = json_text.as_bytes();
    if bytes.len() < COMPRESS_THRESHOLD_BYTES {
        return Ok(json_text.to_string());
    }
    let compressed =
        zstd::bulk::compress(bytes, LEVEL).map_err(|e| format!("zstd compression failed: {e}"))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&compressed);
    // Only commit when the framed form is actually smaller.
    if encoded.len() + MARKER.len() < bytes.len() {
        Ok(format!("{MARKER}{encoded}"))
    } else {
        Ok(json_text.to_string())
    }
}

/// Parse from storage: inflate marker rows, pass plain JSON through.
/// A malformed marker row is storage corruption (loud at the caller).
pub fn decode_data_text(text: &str) -> Result<String, String> {
    let Some(encoded) = text.strip_prefix(MARKER) else {
        return Ok(text.to_string());
    };
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| format!("compressed row base64 is corrupt: {e}"))?;
    let reader = std::io::Cursor::new(compressed);
    let plain = zstd::stream::decode_all(reader)
        .map_err(|e| format!("compressed row zstd frame is corrupt: {e}"))?;
    String::from_utf8(plain).map_err(|e| format!("compressed row is not UTF-8: {e}"))
}

/// Whether a stored text is a compressed row (diagnostics/tests).
pub fn is_compressed_text(text: &str) -> bool {
    text.starts_with(MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_texts_stay_verbatim() {
        let text = r#"{"turn":1}"#;
        assert_eq!(encode_data_text(text).unwrap(), text);
        assert!(!is_compressed_text(text));
    }

    #[test]
    fn large_compressible_texts_round_trip() {
        // Repetitive JSON compresses far below the threshold.
        let big = format!(
            "{{\"content\":\"{}\"}}",
            "The quick brown fox jumps over the lazy dog. ".repeat(200)
        );
        assert!(big.len() >= COMPRESS_THRESHOLD_BYTES);
        let stored = encode_data_text(&big).unwrap();
        assert!(is_compressed_text(&stored), "must compress");
        assert!(stored.len() < big.len());
        assert_eq!(decode_data_text(&stored).unwrap(), big);
    }

    #[test]
    fn incompressible_texts_stay_verbatim() {
        // High-entropy payload (xorshift over printable ASCII): the base64
        // zstd frame cannot shrink it, so the encoder must not enlarge it.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let entropy: String = (0..4096)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                char::from_u32(33 + (state % 94) as u32).expect("printable ASCII")
            })
            .collect();
        let big = format!("{{\"blob\":\"{entropy}\"}}");
        assert!(big.len() >= COMPRESS_THRESHOLD_BYTES);
        let stored = encode_data_text(&big).unwrap();
        assert!(!is_compressed_text(&stored), "must not enlarge");
        assert_eq!(stored, big);
    }

    #[test]
    fn malformed_marker_rows_are_loud() {
        assert!(decode_data_text("zstd1:not-base64!!").is_err());
        assert!(
            decode_data_text("zstd1:aGVsbG8=").is_err(),
            "not a zstd frame"
        );
    }
}
