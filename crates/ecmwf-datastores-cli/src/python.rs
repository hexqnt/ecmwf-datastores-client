use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use ecmwf_datastores_client::CollectionId;
use serde_json::{Map, Number, Value};

use crate::config::ImportedSpec;

struct LiteralParser<'a> {
    depth: usize,

    source: &'a str,

    offset: usize,
}

impl<'a> LiteralParser<'a> {
    const fn new(source: &'a str) -> Self {
        Self {
            source,
            offset: 0,
            depth: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.source[self.offset..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.offset += character.len_utf8();
        Some(character)
    }

    fn expect(&mut self, expected: char) -> Result<()> {
        self.skip_trivia();
        if self.consume(expected) {
            Ok(())
        } else {
            bail!("expected `{expected}` at byte {}", self.offset)
        }
    }

    fn consume(&mut self, expected: char) -> bool {
        if self.peek() != Some(expected) {
            return false;
        }
        self.bump();
        true
    }
    fn consume_keyword(&mut self, keyword: &str) -> bool {
        let tail = &self.source[self.offset..];
        if !tail.starts_with(keyword)
            || tail[keyword.len()..]
                .chars()
                .next()
                .is_some_and(is_identifier_char)
        {
            return false;
        }
        self.offset += keyword.len();
        true
    }

    fn skip_trivia(&mut self) {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.bump();
            }
            if self.peek() != Some('#') {
                break;
            }
            while self.peek().is_some_and(|character| character != '\n') {
                self.bump();
            }
        }
    }

    fn parse_dict(&mut self) -> Result<Value> {
        self.expect('{')?;
        let mut map = Map::new();
        loop {
            self.skip_trivia();
            if self.consume('}') {
                return Ok(Value::Object(map));
            }
            let key = self
                .parse_string()
                .context("dictionary keys must be strings")?;
            self.skip_trivia();
            self.expect(':')?;
            let value = self.parse_value()?;
            match map.entry(key) {
                serde_json::map::Entry::Vacant(entry) => {
                    entry.insert(value);
                }
                serde_json::map::Entry::Occupied(entry) => {
                    bail!("duplicate dictionary key `{}`", entry.key());
                }
            }
            self.skip_trivia();
            if self.consume('}') {
                return Ok(Value::Object(map));
            }
            self.expect(',')?;
        }
    }
    fn parse_value(&mut self) -> Result<Value> {
        const MAX_DEPTH: usize = 64;
        if self.depth == MAX_DEPTH {
            bail!("Python literal exceeds the maximum nesting depth of {MAX_DEPTH}");
        }
        self.depth += 1;
        let result = self.parse_value_inner();
        self.depth -= 1;
        result
    }
    fn parse_string(&mut self) -> Result<String> {
        self.skip_trivia();
        let quote = self.peek().context("expected a quoted string")?;
        if !matches!(quote, '\'' | '"') {
            bail!("expected a quoted string at byte {}", self.offset);
        }
        self.bump();
        let mut value = String::new();
        loop {
            let character = self
                .bump()
                .context("unterminated string in Python literal")?;
            if character == quote {
                return Ok(value);
            }
            if character != '\\' {
                value.push(character);
                continue;
            }
            let escaped = self.bump().context("unterminated escape sequence")?;
            value.push(match escaped {
                '\\' => '\\',
                '\'' => '\'',
                '"' => '"',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => bail!("unsupported Python escape `\\{other}`"),
            });
        }
    }
    fn parse_number(&mut self) -> Result<Value> {
        let start = self.offset;
        while self.peek().is_some_and(|character| {
            character.is_ascii_digit() || matches!(character, '-' | '+' | '.' | 'e' | 'E')
        }) {
            self.bump();
        }
        let token = &self.source[start..self.offset];
        if !token.contains(['.', 'e', 'E']) {
            return token
                .parse::<i64>()
                .map(|value| Value::Number(value.into()))
                .map_err(|error| {
                    if matches!(
                        error.kind(),
                        std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow
                    ) {
                        anyhow::anyhow!("Python integer `{token}` is outside the i64 range")
                    } else {
                        anyhow::anyhow!("invalid Python integer `{token}`")
                    }
                });
        }
        let value = token
            .parse::<f64>()
            .with_context(|| format!("invalid Python number `{token}`"))?;
        Number::from_f64(value)
            .map(Value::Number)
            .context("Python number must be finite")
    }
    fn parse_sequence(&mut self, end: char) -> Result<Vec<Value>> {
        let start = if end == ']' { '[' } else { '(' };
        self.expect(start)?;
        let mut values = Vec::new();
        loop {
            self.skip_trivia();
            if self.consume(end) {
                return Ok(values);
            }
            values.push(self.parse_value()?);
            self.skip_trivia();
            if self.consume(end) {
                return Ok(values);
            }
            self.expect(',')?;
        }
    }
    fn parse_value_inner(&mut self) -> Result<Value> {
        self.skip_trivia();
        match self.peek() {
            Some('\'' | '"') => self.parse_string().map(Value::String),
            Some('{') => self.parse_dict(),
            Some('[') => self.parse_sequence(']').map(Value::Array),
            Some('(') => self.parse_sequence(')').map(Value::Array),
            Some('-' | '0'..='9') => self.parse_number(),
            Some(_) if self.consume_keyword("True") => Ok(Value::Bool(true)),
            Some(_) if self.consume_keyword("False") => Ok(Value::Bool(false)),
            Some(_) if self.consume_keyword("None") => Ok(Value::Null),
            Some(character) => bail!(
                "unsupported Python literal starting with `{character}` at byte {}",
                self.offset
            ),
            None => bail!("expected a Python literal at end of input"),
        }
    }

    fn finish_assignment(&mut self) -> Result<()> {
        while matches!(self.peek(), Some(' ' | '\t' | '\r')) {
            self.bump();
        }
        if self.peek() == Some('#') {
            while self.peek().is_some_and(|character| character != '\n') {
                self.bump();
            }
        }
        if self.peek().is_some_and(|character| character != '\n') {
            bail!(
                "unsupported expression after Python literal at byte {}",
                self.offset
            );
        }
        Ok(())
    }
}

fn json_to_toml(value: Value) -> Result<toml::Value> {
    Ok(match value {
        Value::Null => bail!("Python None cannot be represented in a TOML request"),
        Value::Bool(value) => toml::Value::Boolean(value),
        Value::Number(value) => number_to_toml(&value)?,
        Value::String(value) => toml::Value::String(value),
        Value::Array(values) => toml::Value::Array(
            values
                .into_iter()
                .map(json_to_toml)
                .collect::<Result<_>>()?,
        ),
        Value::Object(map) => toml::Value::Table(json_map_to_toml(map)?),
    })
}

fn json_map_to_toml(map: Map<String, Value>) -> Result<toml::Table> {
    map.into_iter()
        .map(|(key, value)| Ok((key, json_to_toml(value)?)))
        .collect()
}

fn number_to_toml(number: &Number) -> Result<toml::Value> {
    if let Some(value) = number.as_i64() {
        return Ok(toml::Value::Integer(value));
    }
    if let Some(value) = number.as_u64() {
        return i64::try_from(value)
            .map(toml::Value::Integer)
            .context("integer is too large for TOML");
    }
    number
        .as_f64()
        .map(toml::Value::Float)
        .context("invalid floating-point number")
}

/// Converts the constrained Python literal format emitted by “Show API request
/// code” into a retrieval TOML file without executing Python.
pub fn import_api_code(source: &str) -> Result<String> {
    let dataset = parse_assignment(source, "dataset")?
        .as_str()
        .context("`dataset` must be a string")?
        .parse::<CollectionId>()?;
    let Value::Object(request) = parse_assignment(source, "request")? else {
        bail!("`request` must be a dictionary");
    };
    let output = optional_assignment(source, "target")?
        .map(|value| {
            value
                .as_str()
                .map(PathBuf::from)
                .context("`target` must be a string")
        })
        .transpose()?;
    let request = json_map_to_toml(request)?;
    toml::to_string_pretty(&ImportedSpec {
        dataset: &dataset,
        output: output.as_deref(),
        request: &request,
    })
    .context("API request cannot be represented as TOML")
}

fn parse_assignment(source: &str, name: &str) -> Result<Value> {
    optional_assignment(source, name)?
        .with_context(|| format!("Python assignment `{name} = ...` was not found"))
}

fn is_identifier_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn optional_assignment(source: &str, name: &str) -> Result<Option<Value>> {
    let Some(offset) = assignment_value_offset(source, name)? else {
        return Ok(None);
    };
    let mut parser = LiteralParser::new(&source[offset..]);
    let value = parser.parse_value()?;
    parser.finish_assignment()?;
    Ok(Some(value))
}

fn assignment_value_offset(source: &str, name: &str) -> Result<Option<usize>> {
    let mut line_offset = 0;
    let mut found = None;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#')
            && let Some(tail) = trimmed.strip_prefix(name)
            && !tail.chars().next().is_some_and(is_identifier_char)
        {
            let whitespace = tail.len() - tail.trim_start().len();
            if tail[whitespace..].starts_with('=') {
                let indentation = line.len() - trimmed.len();
                if found.is_some() {
                    bail!("Python assignment `{name}` occurs more than once");
                }
                found = Some(line_offset + indentation + name.len() + whitespace + 1);
            }
        }
        line_offset += line.len();
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_current_cds_api_snippet() {
        let output = import_api_code(
            r#"
import cdsapi

dataset = "reanalysis-era5-pressure-levels"
request = {
    "product_type": ["reanalysis"],
    "variable": ["temperature"],
    "year": ["2024"],
    "month": ["01"],
    "day": ["01"],
    "time": ["00:00"],
    "pressure_level": ["500"],
    "area": [60, 20, 50, 40], # north, west, south, east
}
target = "temperature.nc"
client = cdsapi.Client()
client.retrieve(dataset, request, target)
"#,
        )
        .unwrap();

        let spec = crate::config::RetrievalSpec::parse(&output).unwrap();
        assert_eq!(spec.dataset().as_str(), "reanalysis-era5-pressure-levels");
        assert_eq!(spec.output(), Some(std::path::Path::new("temperature.nc")));
        assert_eq!(
            spec.request().as_map()["pressure_level"],
            serde_json::json!(["500"])
        );
    }

    #[test]
    fn rejects_non_literal_python() {
        let error = import_api_code(
            r#"
dataset = "reanalysis-era5-land"
request = make_request()
"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("unsupported Python literal"));
    }

    #[test]
    fn rejects_unsafe_dataset_identifier_during_import() {
        let error = import_api_code(
            "dataset = '../other-dataset'\nrequest = {'variable': ['temperature']}\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid identifier"));
    }

    #[test]
    fn ignores_commented_assignments() {
        let output = import_api_code(
            r#"
# dataset = "wrong-dataset"
dataset = "reanalysis-era5-land"
# request = {"variable": ["wrong"]}
request = {"variable": ["2m_temperature"]}
"#,
        )
        .unwrap();

        let spec = crate::config::RetrievalSpec::parse(&output).unwrap();
        assert_eq!(spec.dataset().as_str(), "reanalysis-era5-land");
        assert_eq!(
            spec.request().as_map()["variable"],
            serde_json::json!(["2m_temperature"])
        );
    }

    #[test]
    fn rejects_expression_after_literal() {
        let error = import_api_code(
            "dataset = 'reanalysis-era5-land'\nrequest = {'variable': ['temperature']} | {'year': ['2024']}\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("unsupported expression"));
    }

    #[test]
    fn rejects_integer_outside_i64_range() {
        let error = import_api_code(
            "dataset = 'reanalysis-era5-land'\nrequest = {'number': 9223372036854775808}\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("outside the i64 range"));
    }

    #[test]
    fn rejects_repeated_assignment() {
        let error = import_api_code(
            "dataset = 'first'\ndataset = 'second'\nrequest = {'variable': ['temperature']}\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("occurs more than once"));
    }

    #[test]
    fn rejects_excessive_nesting() {
        let nested = format!(
            "dataset = 'reanalysis-era5-land'\nrequest = {{'value': {}0{}}}\n",
            "[".repeat(65),
            "]".repeat(65)
        );
        let error = import_api_code(&nested).unwrap_err();
        assert!(error.to_string().contains("maximum nesting depth"));
    }
}
