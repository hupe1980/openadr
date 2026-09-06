//! Constrained string newtypes.
//!
//! The specification expresses most identifiers as `string` with `minLength`/`maxLength` and, for
//! `objectID`, a character-class `pattern`. Parsing those constraints once, at the edge, means the
//! rest of the crate never has to ask whether a name is well formed.

use crate::std_shim::{String, ToString};
use core::{fmt, str::FromStr};
use serde::{Deserialize, Deserializer, Serialize, de::Unexpected};

/// Why a constrained string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentifierError {
    /// Length outside the schema's `minLength`..=`maxLength`.
    #[error("{kind} length {len} is outside the allowed range {min}..={max}")]
    Length {
        /// Type name, for the message.
        kind: &'static str,
        /// Actual length in bytes.
        len: usize,
        /// Schema minimum.
        min: usize,
        /// Schema maximum.
        max: usize,
    },
    /// Character outside `^[a-zA-Z0-9_-]*$` (applies to `objectID` only).
    #[error("{kind} contains characters outside [a-zA-Z0-9_-]")]
    Charset {
        /// Type name, for the message.
        kind: &'static str,
    },
    /// A value that would be ambiguous on the wire or in a URL path.
    #[error("{kind} must not be {value:?}")]
    Reserved {
        /// Type name, for the message.
        kind: &'static str,
        /// The offending value.
        value: String,
    },
}

/// Values that would be ambiguous in a path segment or in JSON.
const RESERVED: &[&str] = &["null", "undefined", ".", ".."];

macro_rules! constrained_string {
    (
        $(#[$meta:meta])*
        $name:ident, min = $min:expr, max = $max:expr, url_safe = $url_safe:expr
    ) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Minimum length accepted by the schema.
            pub const MIN_LEN: usize = $min;
            /// Maximum length accepted by the schema.
            pub const MAX_LEN: usize = $max;

            /// Validate and wrap.
            pub fn new(value: impl Into<String>) -> Result<Self, IdentifierError> {
                let value = value.into();
                Self::validate(&value)?;
                Ok(Self(value))
            }

            /// Check a candidate without allocating a wrapper.
            pub fn validate(value: &str) -> Result<(), IdentifierError> {
                let len = value.len();
                if !($min..=$max).contains(&len) {
                    return Err(IdentifierError::Length {
                        kind: stringify!($name),
                        len,
                        min: $min,
                        max: $max,
                    });
                }
                // Reserved words are checked before the character set so that `null` and `..`
                // report why they are refused rather than merely which byte offended.
                if RESERVED.iter().any(|r| value.eq_ignore_ascii_case(r)) {
                    return Err(IdentifierError::Reserved {
                        kind: stringify!($name),
                        value: value.to_string(),
                    });
                }
                if $url_safe
                    && !value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    return Err(IdentifierError::Charset {
                        kind: stringify!($name),
                    });
                }
                Ok(())
            }

            /// Borrow as `&str`.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Unwrap into the inner `String`.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl FromStr for $name {
            type Err = IdentifierError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdentifierError;
            fn try_from(s: String) -> Result<Self, Self::Error> {
                Self::new(s)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({:?})", stringify!($name), self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                Self::validate(&s).map_err(|e| {
                    serde::de::Error::invalid_value(
                        Unexpected::Str(&s),
                        &e.to_string().as_str(),
                    )
                })?;
                Ok(Self(s))
            }
        }
    };
}

constrained_string!(
    /// URL-safe, VTN-assigned object identifier (`objectID`).
    ///
    /// Schema: `^[a-zA-Z0-9_-]*$`, 1..=128. The empty string is excluded because it cannot address
    /// an object in a path.
    ObjectId, min = 1, max = 128, url_safe = true
);

constrained_string!(
    /// Identity of an API client, as provisioned by the authorization service (`clientID`).
    ///
    /// This is the key of the whole object-privacy model: the VTN stamps it on VEN-created objects
    /// and matches it on read.
    ClientId, min = 1, max = 128, url_safe = false
);

constrained_string!(
    /// A targeting label (`target`).
    ///
    /// In OpenADR 3.1 targets are plain strings; 3.0's `{type, values}` pairs are gone.
    Target, min = 1, max = 128, url_safe = false
);

constrained_string!(
    /// Human-chosen VEN name, unique within a VTN.
    VenName, min = 1, max = 128, url_safe = false
);

constrained_string!(
    /// Human-chosen resource name, unique within its VEN.
    ResourceName, min = 1, max = 128, url_safe = false
);

constrained_string!(
    /// Client-chosen name carried on reports and subscriptions.
    ClientName, min = 1, max = 128, url_safe = false
);

constrained_string!(
    /// Program name, unique within a VTN.
    ProgramName, min = 1, max = 128, url_safe = false
);

impl ResourceName {
    /// The reserved name meaning "this report aggregates several resources".
    pub const AGGREGATED: &'static str = "AGGREGATED_REPORT";

    /// Whether this is the reserved aggregate marker.
    pub fn is_aggregated(&self) -> bool {
        self.0 == Self::AGGREGATED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_id_charset_is_enforced() {
        assert!(ObjectId::new("abc-123_XYZ").is_ok());
        assert!(matches!(
            ObjectId::new("has space"),
            Err(IdentifierError::Charset { .. })
        ));
        assert!(matches!(
            ObjectId::new("slash/es"),
            Err(IdentifierError::Charset { .. })
        ));
    }

    #[test]
    fn lengths_follow_the_schema() {
        assert!(ObjectId::new("").is_err());
        assert!(ObjectId::new("x".repeat(128)).is_ok());
        assert!(ObjectId::new("x".repeat(129)).is_err());
    }

    #[test]
    fn reserved_words_are_rejected_case_insensitively() {
        for bad in ["null", "NULL", "..", "."] {
            assert!(
                matches!(ObjectId::new(bad), Err(IdentifierError::Reserved { .. })),
                "{bad} should be reserved"
            );
        }
    }

    #[test]
    fn targets_allow_punctuation_that_object_ids_do_not() {
        // Real deployments use EAN18 codes, wildcards and dotted paths as targets.
        assert!(Target::new("871685900000000000").is_ok());
        assert!(Target::new("BATTERY-*").is_ok());
        assert!(Target::new("zone.a/feeder-3").is_ok());
    }

    #[test]
    fn deserialization_rejects_invalid_values() {
        assert!(serde_json::from_str::<ObjectId>("\"ok-1\"").is_ok());
        assert!(serde_json::from_str::<ObjectId>("\"not ok\"").is_err());
    }
}
