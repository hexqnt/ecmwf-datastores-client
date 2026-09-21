use std::{
    fs,
    num::NonZeroUsize,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, NaiveTime};
use ecmwf_datastores_client::{CollectionId, Selection, planning::PlanningRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

/// Planning types shared with library consumers.
pub use ecmwf_datastores_client::planning::{
    DEFAULT_MAX_ITEMS, DEFAULT_MAX_REQUESTS, DatasetProfile, GeographicArea as BoundingBox,
    TimeRange,
};

/// Default number of jobs allowed to wait or run on the server concurrently.
pub const DEFAULT_MAX_ACTIVE_JOBS: usize = 2;
/// Default number of result assets downloaded concurrently.
pub const DEFAULT_MAX_CONCURRENT_DOWNLOADS: usize = 1;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpec {
    bbox: Option<RawBoundingBox>,

    time: Option<RawTimeRange>,

    #[serde(default)]
    split: RawSplit,

    output: Option<PathBuf>,

    dataset: String,

    #[serde(default)]
    request: toml::Table,

    #[serde(default)]
    execution: ExecutionConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSplit {
    #[serde(default = "default_max_items")]
    max_items: NonZeroUsize,
    #[serde(default = "default_max_requests")]
    max_requests: NonZeroUsize,
}

impl Default for RawSplit {
    fn default() -> Self {
        Self {
            max_items: default_max_items(),
            max_requests: default_max_requests(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTimeRange {
    end: String,

    start: String,

    #[serde(default)]
    times: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBoundingBox {
    lat: [f64; 2],

    lon: [f64; 2],
}

#[derive(Debug, Serialize)]
pub(crate) struct ImportedSpec<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<&'a Path>,

    pub dataset: &'a CollectionId,

    pub request: &'a toml::Table,
}

/// Parsed retrieval configuration with validated structural invariants.
#[derive(Debug, Clone)]
pub struct RetrievalSpec {
    bbox: Option<BoundingBox>,

    time: Option<TimeRange>,

    output: Option<PathBuf>,

    dataset: CollectionId,

    request: Selection,

    execution: ExecutionConfig,

    max_items: NonZeroUsize,
    max_requests: NonZeroUsize,
}

impl RetrievalSpec {
    /// Returns the structured bounding box, when specified.
    pub const fn bbox(&self) -> Option<BoundingBox> {
        self.bbox
    }

    /// Returns the structured time range, when specified.
    pub fn time(&self) -> Option<&TimeRange> {
        self.time.as_ref()
    }

    /// Parses a retrieval configuration from TOML text.
    pub fn parse(source: &str) -> Result<Self> {
        let raw: RawSpec = toml::from_str(source).context("invalid TOML")?;
        raw.try_into()
    }

    /// Returns the configured base output path, when specified.
    pub fn output(&self) -> Option<&Path> {
        self.output.as_deref()
    }

    /// Returns the dataset-specific planning profile.
    pub fn profile(&self) -> DatasetProfile {
        DatasetProfile::for_collection(&self.dataset)
    }

    /// Returns the target CDS collection.
    pub fn dataset(&self) -> &CollectionId {
        &self.dataset
    }

    /// Returns the dataset-specific request parameters.
    pub const fn request(&self) -> &Selection {
        &self.request
    }

    /// Reads and parses a retrieval configuration from a TOML file.
    pub fn from_path(path: &Path) -> Result<Self> {
        let source = fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration {}", path.display()))?;
        Self::parse(&source)
            .with_context(|| format!("failed to parse configuration {}", path.display()))
    }

    /// Returns the bounded-concurrency settings used during retrieval.
    pub const fn execution(&self) -> ExecutionConfig {
        self.execution
    }

    /// Returns the maximum estimated number of items in one request.
    pub const fn max_items(&self) -> NonZeroUsize {
        self.max_items
    }
    /// Returns the maximum number of requests in one plan.
    pub const fn max_requests(&self) -> NonZeroUsize {
        self.max_requests
    }

    /// Converts CLI configuration into the path-independent library request.
    pub fn planning_request(&self) -> PlanningRequest {
        let mut request = PlanningRequest::new(self.dataset.clone(), self.request.clone());
        if let Some(time) = self.time.clone() {
            request = request.with_time_range(time);
        }
        if let Some(area) = self.bbox {
            request = request.with_area(area);
        }
        request
    }
}

impl TryFrom<RawSpec> for RetrievalSpec {
    type Error = anyhow::Error;

    fn try_from(raw: RawSpec) -> Result<Self> {
        let dataset = CollectionId::try_from(raw.dataset)?;
        let profile = DatasetProfile::for_collection(&dataset);
        if raw.time.is_some() && profile == DatasetProfile::Other {
            bail!(
                "structured time ranges are not supported for dataset `{dataset}`; use exact fields in [request]"
            );
        }
        if raw.bbox.is_some() && profile == DatasetProfile::Other {
            bail!(
                "structured bbox is not supported for dataset `{dataset}`; use exact fields in [request]"
            );
        }
        let bbox = raw
            .bbox
            .map(|bbox| BoundingBox::new(bbox.lat, bbox.lon))
            .transpose()?;
        let time = raw
            .time
            .map(|time| -> Result<TimeRange> {
                let start = parse_date(&time.start, "time.start")?;
                let end = parse_date(&time.end, "time.end")?;
                let times = time
                    .times
                    .into_iter()
                    .map(|value| {
                        NaiveTime::parse_from_str(&value, "%H:%M")
                            .with_context(|| format!("invalid time `{value}`; expected HH:MM"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(TimeRange::new(start, end, times)?)
            })
            .transpose()?;
        if matches!(
            profile,
            DatasetProfile::Era5Hourly | DatasetProfile::Era5LandHourly
        ) && time.as_ref().is_some_and(|time| time.times().is_empty())
        {
            bail!("time.times must contain at least one HH:MM value for hourly ERA5 datasets");
        }
        if profile == DatasetProfile::Era5Daily
            && time.as_ref().is_some_and(|time| !time.times().is_empty())
        {
            bail!("time.times is not supported for daily ERA5 datasets");
        }
        let request = table_to_json(raw.request)?;
        reject_empty_arrays(&request, "request")?;
        let request = Selection::try_from(request)?;
        Ok(Self {
            dataset,
            output: raw.output,
            bbox,
            time,
            request,
            max_items: raw.split.max_items,
            max_requests: raw.split.max_requests,
            execution: raw.execution,
        })
    }
}

/// Bounded-concurrency settings for the retrieval pipeline.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConfig {
    #[serde(default = "default_max_active_jobs")]
    max_active_jobs: NonZeroUsize,
    #[serde(default = "default_max_concurrent_downloads")]
    max_concurrent_downloads: NonZeroUsize,
}

impl ExecutionConfig {
    /// Returns the maximum number of jobs waiting or running on the server.
    pub const fn max_active_jobs(self) -> NonZeroUsize {
        self.max_active_jobs
    }
    /// Returns the maximum number of simultaneous asset downloads.
    pub const fn max_concurrent_downloads(self) -> NonZeroUsize {
        self.max_concurrent_downloads
    }
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            max_active_jobs: default_max_active_jobs(),
            max_concurrent_downloads: default_max_concurrent_downloads(),
        }
    }
}

fn parse_date(value: &str, field: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .with_context(|| format!("invalid {field} `{value}`; expected YYYY-MM-DD"))
}

fn toml_to_json(value: toml::Value) -> Result<Value> {
    Ok(match value {
        toml::Value::String(value) => Value::String(value),
        toml::Value::Integer(value) => Value::Number(value.into()),
        toml::Value::Float(value) => Value::Number(
            Number::from_f64(value)
                .context("request contains a non-finite floating-point value")?,
        ),
        toml::Value::Boolean(value) => Value::Bool(value),
        toml::Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(toml_to_json)
                .collect::<Result<_>>()?,
        ),
        toml::Value::Table(table) => table_to_json(table)?,
        toml::Value::Datetime(value) => {
            bail!("TOML date/time `{value}` in [request] must be quoted")
        }
    })
}

fn table_to_json(table: toml::Table) -> Result<Value> {
    let values = table
        .into_iter()
        .map(|(key, value)| Ok((key, toml_to_json(value)?)))
        .collect::<Result<Map<_, _>>>()?;
    Ok(Value::Object(values))
}

fn reject_empty_arrays(value: &Value, path: &str) -> Result<()> {
    match value {
        Value::Array(values) if values.is_empty() => bail!("{path} must not be an empty array"),
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                reject_empty_arrays(value, &format!("{path}[{index}]"))?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                reject_empty_arrays(value, &format!("{path}.{key}"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn default_max_items() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_MAX_ITEMS).expect("default item limit is non-zero")
}

fn default_max_requests() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_MAX_REQUESTS).expect("default request limit is non-zero")
}

fn default_max_active_jobs() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_MAX_ACTIVE_JOBS).expect("default active job limit is non-zero")
}

fn default_max_concurrent_downloads() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_MAX_CONCURRENT_DOWNLOADS)
        .expect("default concurrent download limit is non-zero")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_structured_era5_request() {
        let spec = RetrievalSpec::parse(
            r#"
dataset = "reanalysis-era5-pressure-levels"
output = "temperature.nc"

[bbox]
lat = [50.0, 60.0]
lon = [20.0, 40.0]

[time]
start = "2024-01-01"
end = "2024-01-31"
times = ["00:00", "12:00"]

[request]
variable = ["temperature"]
pressure_level = ["500", "1000"]
data_format = "netcdf"
"#,
        )
        .unwrap();

        assert_eq!(spec.profile(), DatasetProfile::Era5Hourly);
        for (actual, expected) in spec
            .bbox()
            .unwrap()
            .cds_order()
            .into_iter()
            .zip([60.0, 20.0, 50.0, 40.0])
        {
            approx::assert_abs_diff_eq!(actual, expected);
        }
        assert_eq!(
            spec.time().unwrap().times(),
            [
                NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
                NaiveTime::from_hms_opt(12, 0, 0).unwrap()
            ]
        );
    }

    #[test]
    fn rejects_structured_time_for_unknown_dataset() {
        let error = RetrievalSpec::parse(
            r#"
dataset = "some-future-dataset"
[time]
start = "2024-01-01"
end = "2024-01-02"
"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("structured time ranges"));
    }

    #[test]
    fn parses_daily_era5_range_without_times() {
        let spec = RetrievalSpec::parse(
            r#"
dataset = "derived-era5-single-levels-daily-statistics"
[time]
start = "2021-01-01"
end = "2026-09-30"
[request]
variable = ["2m_temperature"]
"#,
        )
        .unwrap();

        assert_eq!(spec.profile(), DatasetProfile::Era5Daily);
        assert_eq!(spec.time().unwrap().times(), []);
    }

    #[test]
    fn rejects_times_for_daily_era5_range() {
        let error = RetrievalSpec::parse(
            r#"
dataset = "derived-era5-single-levels-daily-statistics"
[time]
start = "2025-01-01"
end = "2025-01-31"
times = ["00:00"]
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("not supported for daily ERA5"));
    }

    #[test]
    fn rejects_empty_request_axes() {
        let error = RetrievalSpec::parse(
            r#"
dataset = "reanalysis-era5-land"
[request]
variable = []
"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("request.variable"));
    }

    #[test]
    fn uses_default_execution_limits_and_accepts_overrides() {
        let defaults = RetrievalSpec::parse("dataset = 'future-dataset'").unwrap();
        assert_eq!(
            defaults.execution().max_active_jobs().get(),
            DEFAULT_MAX_ACTIVE_JOBS
        );
        assert_eq!(
            defaults.execution().max_concurrent_downloads().get(),
            DEFAULT_MAX_CONCURRENT_DOWNLOADS
        );

        let configured = RetrievalSpec::parse(
            r#"
dataset = "future-dataset"
[execution]
max_active_jobs = 4
max_concurrent_downloads = 3
"#,
        )
        .unwrap();
        assert_eq!(configured.execution().max_active_jobs().get(), 4);
        assert_eq!(configured.execution().max_concurrent_downloads().get(), 3);

        let partial = RetrievalSpec::parse(
            r#"
dataset = "future-dataset"
[execution]
max_active_jobs = 4
"#,
        )
        .unwrap();
        assert_eq!(partial.execution().max_active_jobs().get(), 4);
        assert_eq!(
            partial.execution().max_concurrent_downloads().get(),
            DEFAULT_MAX_CONCURRENT_DOWNLOADS
        );
    }

    #[test]
    fn rejects_zero_execution_limits() {
        for field in ["max_active_jobs", "max_concurrent_downloads"] {
            let source = format!("dataset = 'future-dataset'\n[execution]\n{field} = 0\n");
            assert!(
                RetrievalSpec::parse(&source).is_err(),
                "accepted {field} = 0"
            );
        }
    }
}
