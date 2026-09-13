//! Canonical unique ids: time-ordered, base58-encoded, backed by a UUIDv7.
//!
//! Ported from the reference `craft-storage/src/id.rs` (thiserror → snafu).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use snafu::Snafu;
use uuid::Uuid;

const UUID_BYTES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum CraftIdParseError {
    #[snafu(display("empty id"))]
    Empty,
    #[snafu(display("invalid base58 character {character:?} at {index}"))]
    InvalidBase58 { character: char, index: usize },
    #[snafu(display("base58 string has a length that cannot decode to whole bytes"))]
    InvalidBase58Length,
    #[snafu(display("id decoded to {bytes} bytes, expected {UUID_BYTES}"))]
    InvalidByteLen { bytes: usize },
}

/// The canonical unique id for anything in craft: time-ordered, base58-encoded,
/// backed by a UUIDv7.
///
/// Serializes as base58. Accepts legacy v4-hex-uuid strings on parse
/// (either hyphenated 8-4-4-4-12 or the unhyphenated 32 hex variant)
/// so existing on-disk sessions resume; the canonical form is base58.
///
/// Note: base58 encoding is variable-length (21-22 chars for 16 bytes).
/// New v7 ids encode to a stable 21 chars, so lexical sort orders them
/// chronologically; legacy v4 ids (no embedded timestamp) mix 21-22 chars
/// and don't sort by time regardless.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CraftId([u8; UUID_BYTES]);

impl CraftId {
    pub fn generate() -> Self {
        Self(Uuid::now_v7().into_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; UUID_BYTES] {
        &self.0
    }
}

impl fmt::Display for CraftId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&bs58::encode(&self.0).into_string())
    }
}

impl FromStr for CraftId {
    type Err = CraftIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(CraftIdParseError::Empty);
        }
        if let Ok(u) = Uuid::parse_str(s) {
            return Ok(Self(u.into_bytes()));
        }
        decode_base58(s)
    }
}

fn decode_base58(s: &str) -> Result<CraftId, CraftIdParseError> {
    let bytes = bs58::decode(s).into_vec().map_err(|e| match e {
        bs58::decode::Error::InvalidCharacter { character, index } => {
            CraftIdParseError::InvalidBase58 { character, index }
        }
        bs58::decode::Error::NonAsciiCharacter { index } => CraftIdParseError::InvalidBase58 {
            character: '\u{FFFD}',
            index,
        },
        _ => CraftIdParseError::InvalidBase58Length,
    })?;
    if bytes.len() != UUID_BYTES {
        return Err(CraftIdParseError::InvalidByteLen { bytes: bytes.len() });
    }
    let mut arr = [0u8; UUID_BYTES];
    arr.copy_from_slice(&bytes);
    Ok(CraftId(arr))
}

impl Serialize for CraftId {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CraftId {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_HEX: &str = "01965087-4c71-7f00-8000-000000000000";

    #[test]
    fn generate_is_v7() {
        let id = CraftId::generate();
        let uuid = Uuid::from_bytes(*id.as_bytes());
        assert_eq!(uuid.get_version_num(), 7);
    }

    #[test]
    fn roundtrip_base58() {
        let id = CraftId::generate();
        let s = id.to_string();
        assert!((21..=22).contains(&s.len()));
        assert_eq!(s.parse::<CraftId>().unwrap(), id);
    }

    #[test]
    fn roundtrips_leading_zero_bytes() {
        for hex in [
            "00000000-0000-7000-8000-000000000000",
            "00000001-0002-7000-8000-000000000000",
        ] {
            let id: CraftId = hex.parse().unwrap();
            assert_eq!(id.to_string().parse::<CraftId>().unwrap(), id);
        }
    }

    #[test]
    fn parses_legacy_hex() {
        let expected = CraftId(Uuid::parse_str(SAMPLE_HEX).unwrap().into_bytes());
        for s in [SAMPLE_HEX, "019650874c717f008000000000000000"] {
            assert_eq!(s.parse::<CraftId>().unwrap(), expected);
        }
    }

    #[test]
    fn rejects_bad() {
        assert!(matches!(
            "".parse::<CraftId>(),
            Err(CraftIdParseError::Empty)
        ));
        assert!(matches!(
            "O".parse::<CraftId>(),
            Err(CraftIdParseError::InvalidBase58 {
                character: 'O',
                index: 0
            })
        ));
        assert!(matches!(
            "2j87v4grC".parse::<CraftId>(),
            Err(CraftIdParseError::InvalidByteLen { .. })
        ));
    }

    #[test]
    fn serde_base58_roundtrip() {
        let id = CraftId::generate();
        let s = serde_json::to_string(&id).unwrap();
        assert!((23..=24).contains(&s.len()));
        let back: CraftId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, id);
    }
}
