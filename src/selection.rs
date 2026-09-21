use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// A JSON request object without validation against a specific dataset schema.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Selection(Map<String, Value>);

impl Selection {
    /// Creates an empty dataset selection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a dataset-specific request parameter.
    pub fn insert(&mut self, key: impl Into<String>, value: Value) -> Option<Value> {
        self.0.insert(key.into(), value)
    }

    /// Returns the underlying request parameter map.
    pub fn as_map(&self) -> &Map<String, Value> {
        &self.0
    }

    /// Consumes the selection and returns its request parameter map.
    pub fn into_map(self) -> Map<String, Value> {
        self.0
    }
}

impl AsRef<Map<String, Value>> for Selection {
    fn as_ref(&self) -> &Map<String, Value> {
        self.as_map()
    }
}

impl From<Map<String, Value>> for Selection {
    fn from(value: Map<String, Value>) -> Self {
        Self(value)
    }
}

impl TryFrom<Value> for Selection {
    type Error = Error;

    fn try_from(value: Value) -> Result<Self> {
        match value {
            Value::Object(map) => Ok(Self(map)),
            _ => Err(Error::InvalidSelection("expected a JSON object".into())),
        }
    }
}

impl From<Selection> for Map<String, Value> {
    fn from(value: Selection) -> Self {
        value.into_map()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_future_dataset_fields() {
        let selection =
            Selection::try_from(serde_json::json!({ "new_field": ["new_value"] })).unwrap();
        assert_eq!(
            selection.as_map()["new_field"],
            serde_json::json!(["new_value"])
        );
        assert!(Selection::try_from(serde_json::json!([1, 2])).is_err());
    }
}
