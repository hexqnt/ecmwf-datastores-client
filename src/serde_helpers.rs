use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Deserializer, de::Error as DeError};

pub struct ParsedDateTime(pub DateTime<Utc>);

impl<'de> Deserialize<'de> for ParsedDateTime {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DateTimeVisitor;

        impl serde::de::Visitor<'_> for DateTimeVisitor {
            type Value = ParsedDateTime;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an RFC 3339 or ECMWF UTC datetime string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                parse_datetime(value).map(ParsedDateTime).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(DateTimeVisitor)
    }
}
/// Parses a datetime string in RFC3339 or fallback formats into UTC.
pub fn parse_datetime(value: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Ok(dt.with_timezone(&Utc));
    }
    NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
        .map(|naive| DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
}

/// Deserializes an optional datetime string into `Option<DateTime<Utc>>`.
pub fn deserialize_option_datetime<'de, D>(
    deserializer: D,
) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<ParsedDateTime>::deserialize(deserializer)?.map(|value| value.0))
}
