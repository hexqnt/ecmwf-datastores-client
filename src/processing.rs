use std::time::Duration;

use reqwest::Method;
use serde::Serialize;
use serde_json::Value;
use url::Url;

use crate::{
    error::{Error, Result},
    http::{HttpClient, RequestTemplate},
    id::{CollectionId, JobId},
    selection::Selection,
    utils,
};

use models::{JobsPayload, ProcessListPayload};

#[cfg(test)]
use download::{AssetEntry, ResultsPayload};
#[cfg(test)]
use job::LogCursor;

pub use download::{Asset, DownloadProgress, ExistingTarget, JobResults};
pub use job::Job;
pub use models::{
    JobDetails, JobLogEntry, JobMetadata, JobReceipt, JobSort, JobStatus, JobsPage, JobsRequest,
    ProcessDefinition, ProcessList, ProcessListRequest, ProcessSort, RequestCost,
};

mod download;
mod job;
mod models;

/// Default deadline for waiting on a processing job.
// `Duration::from_hours` is newer than the crate's supported Rust version.
#[allow(clippy::duration_suboptimal_units)]
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Serialize)]
struct InputsPayload<'a> {
    inputs: &'a Selection,
}

/// High-level client for interacting with the processing API endpoints.
#[derive(Clone)]
pub struct ProcessingApi {
    http: HttpClient,

    config: ProcessingConfig,

    base_url: Url,
}

impl ProcessingApi {
    /// Creates a new processing API client rooted at the given base URL.
    pub fn new(mut base_url: Url, http: HttpClient, config: ProcessingConfig) -> Self {
        utils::ensure_trailing_slash(&mut base_url);
        Self {
            http,
            config,
            base_url,
        }
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.base_url.join(path).map_err(Error::from)
    }

    /// Creates a `Job` handle from an existing job id.
    pub fn remote_for(&self, job_id: &JobId) -> Result<Job> {
        let url = self.endpoint(&format!("jobs/{job_id}"))?;
        Job::new(url, self.http.clone(), self.config.clone())
    }

    /// Submits a processing request for the specified collection.
    pub async fn submit(&self, collection_id: &CollectionId, request: &Selection) -> Result<Job> {
        let url = self.endpoint(&format!("processes/{collection_id}/execution"))?;
        let template = RequestTemplate::new(Method::POST, url)
            .with_json(&InputsPayload { inputs: request })?;
        let response = self
            .http
            .execute(&template)
            .await
            .map_err(|err| match err {
                Error::Http(_) | Error::Json(_) => Error::SubmissionUnknown(Box::new(err)),
                Error::Api { status, .. } if status.is_server_error() => {
                    Error::SubmissionUnknown(Box::new(err))
                }
                other => other,
            })?;
        let linked_job = response
            .link_href_optional("monitor")
            .and_then(|monitor| match monitor {
                Some(url) => Ok(url),
                None => response
                    .link_href_optional("self")?
                    .ok_or_else(|| Error::link("monitor")),
            })
            .and_then(|url| Job::new(url, self.http.clone(), self.config.clone()));
        match linked_job {
            Ok(job) => Ok(job),
            Err(err) => {
                if let Some(id) = response.body().get("jobID").and_then(Value::as_str) {
                    let id = JobId::parse(id)
                        .map_err(|parse_error| Error::SubmissionUnknown(Box::new(parse_error)))?;
                    return self.remote_for(&id);
                }
                Err(Error::SubmissionUnknown(Box::new(err)))
            }
        }
    }

    /// Lists jobs with optional filters applied.
    pub async fn get_jobs(&self, request: &JobsRequest) -> Result<JobsPage> {
        let mut template = RequestTemplate::new(Method::GET, self.endpoint("jobs")?);
        if let Some(limit) = request.limit {
            template = template.with_query_pair("limit", limit.to_string());
        }
        if let Some(sort) = request.sort_by {
            template = template.with_query_pair("sortby", sort.as_str());
        }
        for status in &request.statuses {
            template = template.with_query_pair("status", status.as_str());
        }
        let response = self
            .http
            .execute_typed_response::<JobsPayload>(&template)
            .await?;
        Ok(JobsPage::new(response, self.http.clone()))
    }
    /// Fetches a single process definition by collection identifier.
    pub async fn get_process(&self, collection_id: &CollectionId) -> Result<ProcessDefinition> {
        let url = self.endpoint(&format!("processes/{collection_id}"))?;
        let template = RequestTemplate::new(Method::GET, url);
        self.http.execute_typed(&template).await
    }

    /// Retrieves all jobs, following pagination internally.
    pub async fn all_jobs(&self, request: &JobsRequest) -> Result<Vec<JobDetails>> {
        self.get_jobs(request).await?.collect_all().await
    }
    /// Retrieves all processes, following pagination internally.
    pub async fn all_processes(
        &self,
        request: &ProcessListRequest,
    ) -> Result<Vec<ProcessDefinition>> {
        self.list_processes(request).await?.collect_all().await
    }

    /// Deletes the provided jobs in bulk.
    pub async fn delete_jobs(&self, job_ids: &[JobId]) -> Result<()> {
        if job_ids.is_empty() {
            return Ok(());
        }
        let url = self.endpoint("jobs/delete")?;
        let template =
            RequestTemplate::new(Method::POST, url).with_json(&DeleteJobsPayload { job_ids })?;
        self.http.execute(&template).await?;
        Ok(())
    }

    /// Asks the server to estimate the cost of a request.
    pub async fn estimate_cost(
        &self,
        collection_id: &CollectionId,
        request: &Selection,
    ) -> Result<RequestCost> {
        let url = self.endpoint(&format!("processes/{collection_id}/costing"))?;
        let template = RequestTemplate::new(Method::POST, url)
            .with_query_pair("request_origin", "ui")
            .with_query_pair("mandatory_inputs", "true")
            .with_json(&InputsPayload { inputs: request })?;
        self.http.execute_typed(&template).await
    }

    /// Retrieves detailed information about a job.
    pub async fn job_details(&self, job_id: &JobId) -> Result<JobDetails> {
        let url = self.endpoint(&format!("jobs/{job_id}"))?;
        let template = RequestTemplate::new(Method::GET, url);
        self.http.execute_typed(&template).await
    }
    /// Fetches current results without waiting for job completion.
    pub async fn job_results_once(&self, job_id: &JobId) -> Result<JobResults> {
        let remote = self.remote_for(job_id)?;
        remote.results_once().await
    }

    /// Retrieves a single page of available processes.
    pub async fn list_processes(&self, request: &ProcessListRequest) -> Result<ProcessList> {
        let mut template = RequestTemplate::new(Method::GET, self.endpoint("processes")?);
        if let Some(limit) = request.limit {
            template = template.with_query_pair("limit", limit.to_string());
        }
        if let Some(sort) = request.sort_by {
            template = template.with_query_pair("sortby", sort.as_str());
        }
        let response = self
            .http
            .execute_typed_response::<ProcessListPayload>(&template)
            .await?;
        Ok(ProcessList::new(response, self.http.clone()))
    }

    /// Returns server-side allowed values for the supplied request parameters.
    /// A successful response does not confirm that the complete request can be submitted.
    pub async fn apply_constraints(
        &self,
        collection_id: &CollectionId,
        request: &Selection,
    ) -> Result<Value> {
        let url = self.endpoint(&format!("processes/{collection_id}/constraints"))?;
        let template = RequestTemplate::new(Method::POST, url)
            .with_json(&InputsPayload { inputs: request })?;
        self.http.execute_typed(&template).await
    }
}

/// Configuration options controlling how processing jobs are polled.
#[derive(Debug, Clone)]
pub struct ProcessingConfig {
    /// Maximum time spent waiting for job completion, including HTTP requests.
    pub wait_timeout: Duration,

    /// Upper bound on every delay between polls, including the first delay.
    pub poll_max_delay: Duration,
}

impl Default for ProcessingConfig {
    fn default() -> Self {
        Self {
            poll_max_delay: Duration::from_secs(120),
            wait_timeout: DEFAULT_WAIT_TIMEOUT,
        }
    }
}

#[derive(Serialize)]
struct DeleteJobsPayload<'a> {
    job_ids: &'a [JobId],
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serde_helpers;
    use serde_json::json;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/cds-contract.json")).unwrap()
    }

    #[test]
    fn deserializes_processing_responses_from_sanitized_live_fixture() {
        let fixture = fixture();
        let job: JobDetails = serde_json::from_value(fixture["job"].clone()).unwrap();
        assert_eq!(job.status, JobStatus::Successful);
        assert_eq!(job.request().unwrap().as_map()["year"], json!(["2023"]));

        let results: ResultsPayload = serde_json::from_value(fixture["results"].clone()).unwrap();
        assert_eq!(results.asset.value.file_size, Some(1024));
        assert_eq!(
            results.asset.value.content_type.as_deref(),
            Some("application/x-grib")
        );
        let receipt: JobReceipt = serde_json::from_value(fixture["receipt"].clone()).unwrap();
        assert_eq!(receipt.status, JobStatus::Successful);
        assert_eq!(receipt.download_size, Some(1024));
        assert_eq!(receipt.request.unwrap().as_map()["year"], json!(["2023"]));
    }

    #[test]
    fn log_cursor_preserves_entries_with_equal_timestamps() {
        let time = serde_helpers::parse_datetime("2026-09-17T11:12:08Z").unwrap();
        let mut cursor = LogCursor::default();
        assert!(cursor.accept(time, 1));
        assert!(cursor.accept(time, 2));
        assert!(!cursor.accept(time, 1));
        assert!(!cursor.accept(time, 2));
        assert!(cursor.accept(time, 3));
    }

    #[test]
    fn parses_valid_and_rejects_invalid_reported_file_sizes() {
        for value in [json!(1024), json!("1024")] {
            let payload = json!({ "value": { "href": "/asset", "file:size": value } });
            let asset: AssetEntry = serde_json::from_value(payload).unwrap();
            assert_eq!(asset.value.file_size, Some(1024));
        }
        for value in [json!(-1), json!(1.5), json!("many"), json!({})] {
            let payload = json!({ "value": { "href": "/asset", "file:size": value } });
            assert!(serde_json::from_value::<AssetEntry>(payload).is_err());
        }
    }

    #[test]
    fn parses_and_validates_request_costs() {
        let cost: RequestCost = serde_json::from_value(json!({
            "id": "size",
            "cost": 25.0,
            "limit": 100.0,
            "cost_bar_steps": [50, 70]
        }))
        .unwrap();
        approx::assert_abs_diff_eq!(cost.cost(), 25.0);
        approx::assert_abs_diff_eq!(cost.limit(), 100.0);
        approx::assert_abs_diff_eq!(cost.preferred_limit(), 50.0);
        assert!(cost.is_valid());
        assert_eq!(cost.cost_bar_steps(), Some([50, 70]));

        for value in [
            json!({"cost": -1, "limit": 100}),
            json!({"cost": 1, "limit": 0}),
            json!({"cost": 1, "limit": 100, "cost_bar_steps": [50]}),
            json!({"cost": 1, "limit": 100, "cost_bar_steps": [70, 50]}),
        ] {
            assert!(serde_json::from_value::<RequestCost>(value).is_err());
        }
    }
}
