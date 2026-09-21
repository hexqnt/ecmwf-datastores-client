use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::error::{Error, Result};

fn parse_id(value: &str) -> Result<&str> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~'))
    {
        return Err(Error::InvalidIdentifier(value.into()));
    }
    Ok(value)
}

macro_rules! define_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(Box<str>);

        impl $name {
            /// Parses an identifier as a single safe URL path segment.
            pub fn parse(value: impl AsRef<str>) -> Result<Self> {
                Ok(Self(parse_id(value.as_ref())?.into()))
            }

            /// Returns the validated identifier as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(value: &str) -> Result<Self> {
                Self::parse(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;

            fn try_from(value: String) -> Result<Self> {
                parse_id(&value)?;
                Ok(Self(value.into_boxed_str()))
            }
        }

        impl TryFrom<Box<str>> for $name {
            type Error = Error;

            fn try_from(value: Box<str>) -> Result<Self> {
                parse_id(&value)?;
                Ok(Self(value))
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(
                &self,
                serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(
                deserializer: D,
            ) -> std::result::Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                Self::try_from(value).map_err(D::Error::custom)
            }
        }
    };
}

define_id!(
    CollectionId,
    "A validated collection or process identifier."
);
define_id!(JobId, "A validated server-assigned job identifier.");
define_id!(LicenceId, "A validated licence identifier.");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_injection() {
        assert!(JobId::parse("abc/../other").is_err());
        assert!(CollectionId::parse("x?query=true").is_err());
        assert!(JobId::parse("").is_err());
        assert!(JobId::parse("..").is_err());
        assert!(CollectionId::parse("dataset.v2").is_ok());
        assert!(LicenceId::parse("../other").is_err());
    }
}
