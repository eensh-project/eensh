//! Base64 conversion of **already encoded** image bytes.
//!
//! This stage is deliberately downstream of the encoder. Base64 is applied to
//! a finished PNG or JPEG byte stream, never to raw framebuffer data, so the
//! payload an agent receives is a real image file by any other name.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

use crate::error::Error;

/// Standard (RFC 4648, padded) base64 alphabet, which is what `image/*;base64`
/// data URIs and most agent tool schemas expect.
pub const ALPHABET: &str = "standard";

/// Base64 encode encoded image bytes.
pub fn encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Decode base64 text back into bytes.
///
/// Used by the test suite to prove that the emitted payload really does decode
/// to the encoded image.
pub fn decode(text: &str) -> Result<Vec<u8>, Error> {
    STANDARD
        .decode(text)
        .map_err(|e| Error::OutputFailed(format!("base64 payload could not be decoded: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_arbitrary_bytes() {
        let original: Vec<u8> = (0..=255u8).collect();
        let encoded = encode(&original);
        assert_eq!(decode(&encoded).unwrap(), original);
    }

    #[test]
    fn uses_the_padded_standard_alphabet() {
        // "any carnal pleasure." is the classic RFC 4648 test vector.
        assert_eq!(
            encode(b"any carnal pleasure"),
            "YW55IGNhcm5hbCBwbGVhc3VyZQ=="
        );
    }

    #[test]
    fn decoding_rejects_malformed_input() {
        assert!(decode("not valid base64!!").is_err());
    }

    #[test]
    fn empty_input_encodes_to_empty_string() {
        assert_eq!(encode(b""), "");
        assert_eq!(decode("").unwrap(), Vec::<u8>::new());
    }
}
