use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use uuid::Uuid;

const UUID_BYTES: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CraftIdParseError {
    #[error("empty id")]
    Empty,
    #[error("invalid base58 character {0:?} at {1}")]
    InvalidBase58(char, usize),
    #[error("base58 string has a length that cannot decode to whole bytes")]
    InvalidBase58Length,
    #[error("id decoded to {0} bytes, expected {UUID_BYTES}")]
    InvalidByteLen(usize),
}

/// The canonical unique id for anything in craft: time-ordered, base58-encoded, backed by a
/// UUIDv7.
///
/// Serializes as base58. Accepts legacy v4-hex-uuid strings on parse
/// (either hyphenated 8-4-4-4-12 or the unhyphenated 32 hex variant)
/// so existing on-disk sessions resume; the canonical form is base58.
///
/// Note: base58 encoding is variable-length (21-22 chars for 16 bytes).
/// New v7 ids encode to a stable 21 chars, so lexical sort orders them
/// chronologically; legacy v4 ids (no embedded timestamp) mix 21-22 chars
/// and don't sort by time regardless. Nothing in craft sorts by the string
/// form today; storage uses the embedded timestamp directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CraftId([u8; UUID_BYTES]);

impl CraftId {
    #[allow(clippy::disallowed_methods)]
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
            CraftIdParseError::InvalidBase58(character, index)
        }
        bs58::decode::Error::NonAsciiCharacter { index } => {
            CraftIdParseError::InvalidBase58('\u{FFFD}', index)
        }
        _ => CraftIdParseError::InvalidBase58Length,
    })?;
    if bytes.len() != UUID_BYTES {
        return Err(CraftIdParseError::InvalidByteLen(bytes.len()));
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

/// A reference to a session as provided at an application boundary (ACP,
/// session resume, SDK mode).
///
/// Preserves the caller's exact string verbatim (legacy hex ids resume
/// unchanged) so wire echo and client correlation hold. The parsed [`CraftId`]
/// is cached so [`id`](Self::id) is infallible. Canonical when self-generated
/// via [`from_id`](Self::from_id) (base58).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionRef {
    id: CraftId,
    raw: String,
}

impl SessionRef {
    pub fn from_id(id: CraftId) -> Self {
        Self {
            id,
            raw: id.to_string(),
        }
    }

    pub fn generate() -> Self {
        Self::from_id(CraftId::generate())
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn id(&self) -> CraftId {
        self.id
    }
}

impl From<CraftId> for SessionRef {
    fn from(id: CraftId) -> Self {
        Self::from_id(id)
    }
}

impl fmt::Display for SessionRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl FromStr for SessionRef {
    type Err = CraftIdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let id = s.parse::<CraftId>()?;
        Ok(Self {
            id,
            raw: s.to_string(),
        })
    }
}

impl<'de> Deserialize<'de> for SessionRef {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl Serialize for SessionRef {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const SAMPLE_HEX: &str = "01965087-4c71-7f00-8000-000000000000";

    fn parse(s: &str) -> CraftId {
        s.parse().unwrap()
    }

    #[test]
    fn generate_is_v7() {
        let id = CraftId::generate();
        let uuid = Uuid::from_bytes(id.0);
        assert_eq!(uuid.get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn roundtrip_base58() {
        let id = CraftId::generate();
        let s = id.to_string();
        assert!((21..=22).contains(&s.len()));
        assert_eq!(s.parse::<CraftId>().unwrap(), id);
    }

    #[test_case("00000000-0000-7000-8000-000000000000")]
    #[test_case("00000001-0002-7000-8000-000000000000")]
    fn roundtrips_leading_zero_bytes(hex: &str) {
        let id: CraftId = hex.parse().unwrap();
        assert_eq!(id.to_string().parse::<CraftId>().unwrap(), id);
    }

    #[test_case(SAMPLE_HEX)]
    #[test_case("019650874c717f008000000000000000")]
    fn parses_legacy_and_canonical(s: &str) {
        let expected = CraftId(Uuid::parse_str(SAMPLE_HEX).unwrap().into_bytes());
        assert_eq!(parse(s), expected);
    }

    #[test_case("" => matches Err(CraftIdParseError::Empty))]
    #[test_case("O" => matches Err(CraftIdParseError::InvalidBase58('O', 0)))]
    #[test_case("2j87v4grC" => matches Err(CraftIdParseError::InvalidByteLen(_)))]
    fn rejects_bad(s: &str) -> Result<CraftId, CraftIdParseError> {
        s.parse()
    }

    #[test]
    fn serde_keyed_base58() {
        let id = CraftId::generate();
        let s = serde_json::to_string(&id).unwrap();
        assert!((23..=24).contains(&s.len()));
        let back: CraftId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, id);
    }

    #[test_case(SAMPLE_HEX)]
    #[test_case("019650874c717f008000000000000000")]
    fn ref_preserves_caller_string(s: &str) {
        let session_ref: SessionRef = s.parse().unwrap();
        assert_eq!(session_ref.as_str(), s);
        assert_eq!(session_ref.id(), parse(s));
    }

    #[test]
    fn ref_from_id_is_canonical_base58() {
        let id = CraftId::generate();
        let session_ref = SessionRef::from(id);
        assert_eq!(session_ref.as_str(), id.to_string());
        assert_eq!(session_ref.id(), id);
    }
}
