use std::path::{Path, PathBuf};

use anyhow::Result;
use ecmwf_datastores_client::{
    Client, CollectionId, RequestCost, Selection,
    planning::{CostingOutcome, Planner},
};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::config::RetrievalSpec;

/// Serializable description of one planned request.
#[derive(Debug, Serialize)]
pub struct PlanEntry<'a> {
    /// One-based request number.
    pub number: usize,

    /// Assigned output file.
    pub output: &'a Path,

    /// Exact selection sent to CDS.
    pub request: &'a Selection,

    /// Server-side cost estimate, when online planning succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_cost: Option<&'a RequestCost>,

    /// Estimated Cartesian item count.
    pub estimated_items: Option<usize>,
}

/// Serializable description of a complete retrieval plan.
#[derive(Debug, Serialize)]
pub struct PlanSummary<'a> {
    /// Final user-visible file produced after all requests complete.
    pub output: PathBuf,

    /// CDS collection identifier.
    pub dataset: &'a CollectionId,

    /// Requests in submission order.
    pub requests: Vec<PlanEntry<'a>>,

    /// Sum of estimated item counts, or unknown if any request cannot be estimated.
    pub estimated_items: Option<usize>,
}

impl<'a> PlanSummary<'a> {
    /// Creates a borrowed serializable summary.
    pub fn new(spec: &'a RetrievalSpec, requests: &'a [PlannedRequest]) -> Self {
        Self {
            dataset: spec.dataset(),
            output: final_output(spec, requests),
            requests: requests
                .iter()
                .enumerate()
                .map(|(index, request)| PlanEntry {
                    number: index + 1,
                    estimated_items: request.estimated_items,
                    provider_cost: request.provider_cost.as_ref(),
                    output: &request.output,
                    request: &request.selection,
                })
                .collect(),
            estimated_items: requests.iter().try_fold(0_usize, |total, request| {
                total.checked_add(request.estimated_items?)
            }),
        }
    }
}

/// One bounded CDS request and its deterministic output path.
#[derive(Debug, Clone)]
pub struct PlannedRequest {
    output: PathBuf,

    selection: Selection,

    provider_cost: Option<RequestCost>,

    estimated_items: Option<usize>,
}

impl PlannedRequest {
    /// Returns the file target assigned to this request.
    pub fn output(&self) -> &Path {
        &self.output
    }

    /// Returns the exact selection sent to CDS.
    pub const fn selection(&self) -> &Selection {
        &self.selection
    }

    /// Returns the server-side cost estimate used to build the plan, when available.
    pub const fn provider_cost(&self) -> Option<&RequestCost> {
        self.provider_cost.as_ref()
    }

    /// Returns the estimated Cartesian item count.
    pub const fn estimated_items(&self) -> Option<usize> {
        self.estimated_items
    }
}

fn part_path(base: &Path, index: usize, total: usize) -> PathBuf {
    if total == 1 {
        return base.to_path_buf();
    }
    let mut filename = std::ffi::OsString::from(".");
    filename.push(
        base.file_stem()
            .unwrap_or_else(|| std::ffi::OsStr::new("download")),
    );
    filename.push(format!(".part-{:04}-of-{total:04}", index + 1));
    if let Some(extension) = base.extension() {
        filename.push(".");
        filename.push(extension);
    }
    base.with_file_name(filename)
}

/// Expands and partitions a validated retrieval specification locally.
pub fn build_plan(spec: &RetrievalSpec) -> Result<Vec<PlannedRequest>> {
    let plan = configured_planner(spec).plan_locally(&spec.planning_request())?;
    Ok(finish_plan(spec, plan.into_requests()))
}

fn finish_plan(
    spec: &RetrievalSpec,
    requests: Vec<ecmwf_datastores_client::planning::PlannedRequest>,
) -> Vec<PlannedRequest> {
    let base_output = requested_output(
        spec,
        requests.first().map(|request| request.selection().as_map()),
    );
    let total = requests.len();
    requests
        .into_iter()
        .enumerate()
        .map(|(index, request)| {
            let (selection, estimated_items, provider_cost) = request.into_parts();
            PlannedRequest {
                selection,
                estimated_items,
                provider_cost,
                output: part_path(&base_output, index, total),
            }
        })
        .collect()
}

/// Returns the single user-visible output represented by a plan.
pub fn final_output(spec: &RetrievalSpec, requests: &[PlannedRequest]) -> PathBuf {
    requested_output(
        spec,
        requests.first().map(|request| request.selection().as_map()),
    )
}

fn default_output(spec: &RetrievalSpec, request: Option<&Map<String, Value>>) -> PathBuf {
    let extension = request
        .and_then(|request| request.get("download_format"))
        .and_then(Value::as_str)
        .filter(|format| *format != "unarchived")
        .map_or_else(
            || {
                request
                    .and_then(|request| {
                        request.get("data_format").or_else(|| request.get("format"))
                    })
                    .and_then(Value::as_str)
                    .map_or("bin", |format| match format {
                        "netcdf" | "netcdf_legacy" => "nc",
                        "grib" => "grib",
                        other
                            if !other.is_empty()
                                && other
                                    .bytes()
                                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') =>
                        {
                            other
                        }
                        _ => "bin",
                    })
            },
            |_| "zip",
        );
    PathBuf::from(format!("{}.{}", spec.dataset(), extension))
}

fn requested_output(spec: &RetrievalSpec, request: Option<&Map<String, Value>>) -> PathBuf {
    spec.output()
        .map_or_else(|| default_output(spec, request), Path::to_path_buf)
}

fn configured_planner(spec: &RetrievalSpec) -> Planner {
    Planner::new()
        .with_max_items(spec.max_items())
        .with_max_requests(spec.max_requests())
}

/// Builds a plan using provider costs with an automatic local fallback.
pub async fn build_adaptive_plan(
    spec: &RetrievalSpec,
    client: &Client,
) -> Result<(Vec<PlannedRequest>, CostingOutcome)> {
    let planner = configured_planner(spec);
    let outcome = planner.plan(client, &spec.planning_request()).await?;
    let (plan, costing) = outcome.into_parts();
    Ok((finish_plan(spec, plan.into_requests()), costing))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_output_stems_and_extensions() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt as _};

        let base = Path::new(OsStr::from_bytes(b"output/result-\xff.nc-\xfe"));
        let expected = Path::new(OsStr::from_bytes(
            b"output/.result-\xff.part-0001-of-0002.nc-\xfe",
        ));
        assert_eq!(part_path(base, 0, 2), expected);
    }

    #[test]
    fn assigns_deterministic_paths_to_split_plan() {
        let spec = RetrievalSpec::parse(
            r#"
dataset = "reanalysis-era5-pressure-levels"
output = "result.nc"
[time]
start = "2024-01-30"
end = "2024-02-02"
times = ["00:00", "12:00"]
[split]
max_items = 4
[request]
variable = ["temperature"]
pressure_level = ["500"]
"#,
        )
        .unwrap();

        let plan = build_plan(&spec).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].output(), Path::new(".result.part-0001-of-0002.nc"));
        assert_eq!(plan[1].output(), Path::new(".result.part-0002-of-0002.nc"));
    }

    #[test]
    fn unsafe_format_does_not_affect_default_output_path() {
        let spec = RetrievalSpec::parse(
            r#"
dataset = "future-dataset"
[request]
data_format = "../../outside"
"#,
        )
        .unwrap();
        let plan = build_plan(&spec).unwrap();
        assert_eq!(plan[0].output(), Path::new("future-dataset.bin"));
    }

    #[test]
    fn preserves_unknown_raw_requests() {
        let spec = RetrievalSpec::parse(
            r#"
dataset = "future-dataset"
[request]
coordinates = [[1, 2], [3, 4]]
"#,
        )
        .unwrap();
        let plan = build_plan(&spec).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].estimated_items(), None);
        assert_eq!(
            plan[0].selection().as_map()["coordinates"],
            json!([[1, 2], [3, 4]])
        );
    }
}
