//! Identifiers for Becky-managed effects.
//!
//! An [`FxId`] is the stable identity Becky uses to refer to a managed
//! function/effect instance across engine, metadata, storage, and provider
//! boundaries. The enum keeps common ID shapes explicit while still allowing
//! callers to carry provider-specific string identifiers.

use std::convert::Infallible;
use std::fmt::{Debug, Display, Formatter};
use std::hash::Hash;
use std::str::FromStr;

/// A unique identifier for a Becky-managed function/effect instance.
///
/// [`FxId`] supports generated UUID v4 IDs, caller-provided strings, and numeric
/// IDs. Its [`Display`] and [`FromStr`] implementations are intended for simple
/// path, log, and metadata keys:
///
/// - UUID strings parse as [`FxId::UuidV4`].
/// - Unsigned integer strings parse as [`FxId::U64`].
/// - Any other string is preserved as [`FxId::String`].
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FxId {
    /// A generated or externally supplied UUID v4 identifier.
    UuidV4(uuid::Uuid),

    /// A provider-specific or caller-defined textual identifier.
    String(String),

    /// A numeric identifier.
    U64(u64),
}

impl FxId {
    /// Generates a new random UUID v4 effect identifier.
    pub fn new_uuid_v4() -> Self {
        FxId::UuidV4(uuid::Uuid::new_v4())
    }
}

impl FromStr for FxId {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(uuid) = uuid::Uuid::from_str(s) {
            return Ok(FxId::UuidV4(uuid));
        }

        if let Ok(n) = u64::from_str(s) {
            return Ok(FxId::U64(n));
        }

        Ok(FxId::String(s.to_string()))
    }
}

impl Display for FxId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            FxId::UuidV4(uuid) => {
                write!(f, "{}", uuid)
            }
            FxId::String(s) => {
                write!(f, "{}", s)
            }
            FxId::U64(n) => {
                write!(f, "{}", n)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_uuid_v4_creates_uuid_v4_id() {
        let FxId::UuidV4(uuid) = FxId::new_uuid_v4() else {
            panic!("expected UUID-backed FxId");
        };

        assert_eq!(uuid.get_version_num(), 4);
    }

    #[test]
    fn parses_uuid_strings_as_uuid_ids() {
        let uuid = match uuid::Uuid::parse_str("67e55044-10b1-426f-9247-bb680e5fe0c8") {
            Ok(uuid) => uuid,
            Err(err) => panic!("test UUID should parse: {err}"),
        };
        let parsed = match "67e55044-10b1-426f-9247-bb680e5fe0c8".parse::<FxId>() {
            Ok(id) => id,
            Err(err) => match err {},
        };

        assert_eq!(parsed, FxId::UuidV4(uuid));
    }

    #[test]
    fn parses_u64_strings_as_numeric_ids() {
        let parsed = match "18446744073709551615".parse::<FxId>() {
            Ok(id) => id,
            Err(err) => match err {},
        };

        assert_eq!(parsed, FxId::U64(u64::MAX));
    }

    #[test]
    fn preserves_non_uuid_non_numeric_strings() {
        let parsed = match "worker-01".parse::<FxId>() {
            Ok(id) => id,
            Err(err) => match err {},
        };

        assert_eq!(parsed, FxId::String("worker-01".to_string()));
    }

    #[test]
    fn display_round_trips_variant_representations() {
        let cases = [
            FxId::UuidV4(match uuid::Uuid::parse_str("67e55044-10b1-426f-9247-bb680e5fe0c8") {
                Ok(uuid) => uuid,
                Err(err) => panic!("test UUID should parse: {err}"),
            }),
            FxId::String("worker-01".to_string()),
            FxId::U64(42),
        ];

        for id in cases {
            let parsed = match id.to_string().parse::<FxId>() {
                Ok(parsed) => parsed,
                Err(err) => match err {},
            };

            assert_eq!(parsed, id);
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_round_trips_all_variants() {
        let cases = [
            FxId::UuidV4(match uuid::Uuid::parse_str("67e55044-10b1-426f-9247-bb680e5fe0c8") {
                Ok(uuid) => uuid,
                Err(err) => panic!("test UUID should parse: {err}"),
            }),
            FxId::String("worker-01".to_string()),
            FxId::U64(42),
        ];

        for id in cases {
            let json = match serde_json::to_string(&id) {
                Ok(json) => json,
                Err(err) => panic!("FxId should serialize: {err}"),
            };
            let parsed = match serde_json::from_str::<FxId>(&json) {
                Ok(parsed) => parsed,
                Err(err) => panic!("FxId should deserialize: {err}"),
            };

            assert_eq!(parsed, id);
        }
    }
}
