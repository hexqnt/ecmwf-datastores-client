use std::{fmt, str::FromStr};

use reqwest::header::HeaderValue;
use url::Url;

use crate::error::{Error, Result};

#[cfg(feature = "discovery")]
mod discovery;

/// A validated API key whose formatting implementations never reveal the secret.
#[derive(Clone)]
pub struct ApiKey(HeaderValue);

impl ApiKey {
    /// Parses and validates an API key for use in an HTTP header.
    pub fn parse(value: impl AsRef<str>) -> Result<Self> {
        value.as_ref().parse()
    }

    pub(crate) fn as_header_value(&self) -> &HeaderValue {
        &self.0
    }
}

impl FromStr for ApiKey {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        if value.is_empty() {
            return Err(Error::InvalidApiKey("API key must not be empty".into()));
        }
        let mut value =
            HeaderValue::from_str(value).map_err(|err| Error::InvalidApiKey(err.to_string()))?;
        value.set_sensitive(true);
        Ok(Self(value))
    }
}

impl TryFrom<String> for ApiKey {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        value.parse()
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey([REDACTED])")
    }
}

/// Explicit endpoint and optional credentials used to construct a client.
#[derive(Clone)]
pub struct Credentials {
    api_key: Option<ApiKey>,

    endpoint: Url,
}

impl Credentials {
    /// Creates credentials from already parsed values.
    pub fn new(endpoint: Url, api_key: Option<ApiKey>) -> Self {
        Self { api_key, endpoint }
    }

    /// Returns the configured API key, if any.
    pub fn api_key(&self) -> Option<&ApiKey> {
        self.api_key.as_ref()
    }

    /// Returns the API endpoint.
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Splits the credentials into their endpoint and API key.
    pub fn into_parts(self) -> (Url, Option<ApiKey>) {
        (self.endpoint, self.api_key)
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("origin", &self.endpoint.origin())
            .field("api_key_configured", &self.api_key.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_api_key() {
        let err = ApiKey::parse("").unwrap_err();
        assert!(matches!(err, Error::InvalidApiKey(_)));
    }

    #[test]
    fn debug_output_redacts_api_key() {
        let key = ApiKey::parse("SECRET").unwrap();
        assert!(!format!("{key:?}").contains("SECRET"));
    }
}
