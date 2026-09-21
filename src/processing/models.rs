use std::{collections::HashMap, num::NonZeroU32};

use chrono::{DateTime, Utc};
use log::Level;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    catalogue::Link,
    error::Result,
    http::{HttpClient, PageLink, PagePayload, Paged, TypedResponse},
    id::{CollectionId, JobId},
    selection::Selection,
    serde_helpers, utils,
};

/// Sort order for jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JobSort {
    /// Oldest jobs first.
    CreatedAscending,
    /// Newest jobs first.
    CreatedDescending,
}

impl JobSort {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::CreatedAscending => "created",
            Self::CreatedDescending => "-created",
        }
    }
}

/// Status values a job can transition through.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum JobStatus {
    /// Processing failed after the job was accepted.
    Failed,

    /// The job is currently being processed.
    Running,

    /// The job was deleted from the service.
    Deleted,

    /// A status introduced by a newer server version.
    Unknown(String),

    /// The service accepted the job but has not started it.
    Accepted,

    /// The service rejected the job before processing.
    Rejected,

    /// The job was dismissed or cancelled.
    Dismissed,

    /// The job completed and results are available.
    Successful,
}

impl JobStatus {
    /// Returns the wire-format status string.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Accepted => "accepted",
            Self::Running => "running",
            Self::Successful => "successful",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::Dismissed => "dismissed",
            Self::Deleted => "deleted",
            Self::Unknown(value) => value,
        }
    }
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for JobStatus {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct JobStatusVisitor;

        impl serde::de::Visitor<'_> for JobStatusVisitor {
            type Value = JobStatus;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a job status string")
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
                Ok(known_job_status(value).unwrap_or_else(|| JobStatus::Unknown(value.to_owned())))
            }

            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                value: &'_ str,
            ) -> std::result::Result<Self::Value, E> {
                self.visit_str(value)
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
                Ok(known_job_status(&value).unwrap_or(JobStatus::Unknown(value)))
            }
        }

        deserializer.deserialize_string(JobStatusVisitor)
    }
}

/// Sort order for processes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProcessSort {
    /// Sort by process identifier in ascending order.
    IdAscending,
    /// Sort by process identifier in descending order.
    IdDescending,
}

impl ProcessSort {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::IdAscending => "id",
            Self::IdDescending => "-id",
        }
    }
}

/// Paginated wrapper around job details.
#[derive(Debug, Clone)]
pub struct JobsPage {
    inner: Paged<JobDetails>,
}

impl JobsPage {
    pub(super) fn new(response: TypedResponse<JobsPayload>, http: HttpClient) -> Self {
        Self {
            inner: Paged::new(response.body.jobs, response.body.links, response.url, http),
        }
    }

    /// Returns a borrowed slice of jobs contained in this page.
    pub fn jobs(&self) -> &[JobDetails] {
        self.inner.items()
    }

    /// Consumes the page and returns the underlying job vector.
    pub fn into_jobs(self) -> Vec<JobDetails> {
        self.inner.into_items()
    }

    fn from_paged(inner: Paged<JobDetails>) -> Self {
        Self { inner }
    }

    /// Retrieves the next page if available.
    pub async fn next(&self) -> Result<Option<Self>> {
        self.follow("next").await
    }

    /// Retrieves the previous page if available.
    pub async fn prev(&self) -> Result<Option<Self>> {
        self.follow("prev").await
    }

    async fn follow(&self, rel: &str) -> Result<Option<Self>> {
        self.inner
            .follow::<JobsPayload>(rel)
            .await
            .map(|page| page.map(Self::from_paged))
    }

    /// Collects all remaining jobs across pages.
    pub async fn collect_all(self) -> Result<Vec<JobDetails>> {
        self.inner.collect_all::<JobsPayload>().await
    }
}
/// Filters applied when listing jobs.
#[derive(Debug, Clone, Default)]
pub struct JobsRequest {
    /// Maximum non-zero number of jobs returned on one page.
    pub limit: Option<NonZeroU32>,

    /// Requested result ordering.
    pub sort_by: Option<JobSort>,

    /// Status filters; repeated values become repeated query parameters.
    pub statuses: Vec<JobStatus>,
}

impl JobsRequest {
    /// Sets the maximum number of jobs returned on one page.
    pub fn with_limit(mut self, limit: NonZeroU32) -> Self {
        self.limit = Some(limit);
        self
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct JobsPayload {
    pub(super) jobs: Vec<JobDetails>,

    #[serde(default)]
    pub(super) links: Vec<PageLink>,
}

impl PagePayload<JobDetails> for JobsPayload {
    fn into_page(self) -> (Vec<JobDetails>, Vec<PageLink>) {
        (self.jobs, self.links)
    }
}

/// Detailed description of a submitted job.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct JobDetails {
    /// Hypermedia links associated with the job.
    #[serde(default)]
    pub links: Vec<Link>,

    /// Server-assigned job identifier.
    #[serde(rename = "jobID")]
    pub job_id: JobId,

    /// Current job status.
    pub status: JobStatus,

    /// Optional logs and echoed request metadata.
    #[serde(default)]
    pub metadata: Option<JobMetadata>,

    /// Collection/process identifier used for the submission.
    #[serde(rename = "processID")]
    pub process_id: CollectionId,

    /// Time at which the job was created.
    #[serde(
        default,
        rename = "created",
        deserialize_with = "serde_helpers::deserialize_option_datetime"
    )]
    pub created_at: Option<DateTime<Utc>>,

    /// Time at which processing started.
    #[serde(
        default,
        rename = "started",
        deserialize_with = "serde_helpers::deserialize_option_datetime"
    )]
    pub started_at: Option<DateTime<Utc>>,

    /// Time at which the job was last updated.
    #[serde(
        default,
        rename = "updated",
        deserialize_with = "serde_helpers::deserialize_option_datetime"
    )]
    pub updated_at: Option<DateTime<Utc>>,

    /// Time at which processing finished.
    #[serde(
        default,
        rename = "finished",
        deserialize_with = "serde_helpers::deserialize_option_datetime"
    )]
    pub finished_at: Option<DateTime<Utc>>,
}

impl JobDetails {
    /// Returns the request parameters echoed in job metadata, if present.
    pub fn request(&self) -> Option<&Selection> {
        self.metadata.as_ref()?.request.as_ref()
    }
}

/// Stable fields of a completed job receipt, including failed jobs.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct JobReceipt {
    /// Other receipt fields not modeled by this crate.
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,

    /// Final processing status.
    pub status: JobStatus,

    /// Suggested result filename when available.
    #[serde(default)]
    pub filename: Option<String>,

    /// Dataset-specific request parameters.
    #[serde(default)]
    pub request: Option<Selection>,
    /// Server-assigned request identifier.
    #[serde(rename = "request-id")]
    pub request_id: JobId,

    /// Collection that produced the receipt.
    #[serde(rename = "collection-id")]
    pub collection_id: CollectionId,

    /// Result size when a download was produced.
    #[serde(default, rename = "download-size")]
    pub download_size: Option<u64>,
}

/// Optional metadata returned alongside a job.
#[derive(Debug, Clone, Deserialize, Default)]
#[non_exhaustive]
pub struct JobMetadata {
    /// Structured log entries emitted while processing the job.
    #[serde(default)]
    pub log: Vec<JobLogEntry>,

    /// Request values echoed by the service.
    #[serde(default, deserialize_with = "deserialize_echoed_request")]
    pub request: Option<Selection>,
}

/// Structured log entry reported by the server for a job.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct JobLogEntry {
    /// Parsed log severity.
    pub level: Level,

    /// Log message without the severity prefix.
    pub message: String,

    /// Server timestamp for the entry.
    pub timestamp: DateTime<Utc>,
}

impl<'de> Deserialize<'de> for JobLogEntry {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let (serde_helpers::ParsedDateTime(timestamp), mut message) =
            <(serde_helpers::ParsedDateTime, String)>::deserialize(deserializer)?;
        let (level, content) = utils::split_prefixed_level(&message);
        let content_start = content.as_ptr() as usize - message.as_ptr() as usize;
        let content_end = content_start + content.len();
        message.truncate(content_end);
        message.drain(..content_start);
        Ok(Self {
            level,
            message,
            timestamp,
        })
    }
}

/// Server-side estimate of the processing cost for one request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RequestCostWire")]
pub struct RequestCost {
    /// Identifier of the cost model used by the service.
    id: Option<String>,

    /// Maximum cost accepted by the provider.
    limit: f64,

    /// Estimated request cost in provider-defined units.
    cost: f64,
    /// Percentage thresholds used by the provider to classify costly requests.
    cost_bar_steps: Option<[u32; 2]>,

    /// Provider-supplied explanation for an invalid request.
    invalid_reason: Option<String>,

    /// Whether the provider considers the request valid.
    request_is_valid: bool,
}

impl RequestCost {
    /// Returns the identifier of the provider cost model, when supplied.
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Returns the hard provider limit in the same units as [`Self::cost`].
    pub const fn limit(&self) -> f64 {
        self.limit
    }

    /// Returns whether the provider considers the request valid.
    pub const fn is_valid(&self) -> bool {
        self.request_is_valid
    }

    /// Returns the estimated cost in provider-defined units.
    pub const fn cost(&self) -> f64 {
        self.cost
    }
    /// Returns percentage thresholds used to classify costly requests.
    pub const fn cost_bar_steps(&self) -> Option<[u32; 2]> {
        self.cost_bar_steps
    }

    /// Returns the provider-supplied reason for an invalid request.
    pub fn invalid_reason(&self) -> Option<&str> {
        self.invalid_reason.as_deref()
    }

    /// Returns the preferred upper cost bound advertised by the provider.
    ///
    /// Providers use the first of two cost-bar percentages as the upper bound
    /// of their lowest-cost band. Responses without exactly two thresholds
    /// fall back to the hard limit, matching the CDS web interface.
    pub fn preferred_limit(&self) -> f64 {
        match self.cost_bar_steps {
            Some([low, _high]) => self.limit * f64::from(low) / 100.0,
            _ => self.limit,
        }
    }
}

impl TryFrom<RequestCostWire> for RequestCost {
    type Error = String;

    fn try_from(wire: RequestCostWire) -> std::result::Result<Self, Self::Error> {
        if !wire.cost.is_finite() || wire.cost < 0.0 {
            return Err("request cost must be finite and non-negative".into());
        }
        if !wire.limit.is_finite() || wire.limit <= 0.0 {
            return Err("request cost limit must be finite and positive".into());
        }
        let cost_bar_steps = wire
            .cost_bar_steps
            .map(|steps| {
                let steps: [u32; 2] = steps
                    .try_into()
                    .map_err(|_| "request cost thresholds must contain exactly two percentages")?;
                if steps.iter().any(|step| !(1..=100).contains(step)) || steps[0] > steps[1] {
                    return Err(
                        "request cost thresholds must be ordered percentages from 1 to 100",
                    );
                }
                Ok(steps)
            })
            .transpose()?;
        Ok(Self {
            id: wire.id,
            cost: wire.cost,
            limit: wire.limit,
            cost_bar_steps,
            request_is_valid: wire.request_is_valid,
            invalid_reason: wire.invalid_reason,
        })
    }
}

#[derive(Deserialize)]
struct RequestCostWire {
    #[serde(default)]
    id: Option<String>,

    limit: f64,

    cost: f64,
    #[serde(default)]
    cost_bar_steps: Option<Vec<u32>>,

    #[serde(default)]
    invalid_reason: Option<String>,

    #[serde(default = "default_true")]
    request_is_valid: bool,
}

#[derive(Deserialize)]
struct EchoedRequest {
    #[serde(default)]
    ids: Selection,
}

/// Paginated wrapper around a list of process definitions.
#[derive(Debug, Clone)]
pub struct ProcessList {
    inner: Paged<ProcessDefinition>,
}

impl ProcessList {
    pub(super) fn new(response: TypedResponse<ProcessListPayload>, http: HttpClient) -> Self {
        Self {
            inner: Paged::new(
                response.body.processes,
                response.body.links,
                response.url,
                http,
            ),
        }
    }

    /// Returns a borrowed slice of processes contained in this page.
    pub fn processes(&self) -> &[ProcessDefinition] {
        self.inner.items()
    }

    fn from_paged(inner: Paged<ProcessDefinition>) -> Self {
        Self { inner }
    }

    /// Consumes the page and returns the underlying process vector.
    pub fn into_processes(self) -> Vec<ProcessDefinition> {
        self.inner.into_items()
    }

    /// Retrieves the next page if available.
    pub async fn next(&self) -> Result<Option<Self>> {
        self.follow("next").await
    }

    /// Retrieves the previous page if available.
    pub async fn prev(&self) -> Result<Option<Self>> {
        self.follow("prev").await
    }

    async fn follow(&self, rel: &str) -> Result<Option<Self>> {
        self.inner
            .follow::<ProcessListPayload>(rel)
            .await
            .map(|page| page.map(Self::from_paged))
    }

    /// Collects all remaining processes across pages.
    pub async fn collect_all(self) -> Result<Vec<ProcessDefinition>> {
        self.inner.collect_all::<ProcessListPayload>().await
    }
}

/// Server-side description of a processing workflow.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct ProcessDefinition {
    /// Stable process identifier, normally equal to the collection identifier.
    pub id: CollectionId,

    /// Human-readable process title.
    #[serde(default)]
    pub title: Option<String>,

    /// Hypermedia links associated with the process.
    #[serde(default)]
    pub links: Vec<Link>,

    /// Input schema and parameter metadata.
    #[serde(default)]
    pub inputs: HashMap<String, Value>,

    /// Output schema and metadata.
    #[serde(default)]
    pub outputs: HashMap<String, Value>,

    /// Optional service message about the process.
    #[serde(default)]
    pub message: Option<String>,

    /// Additional process metadata.
    #[serde(default)]
    pub metadata: Value,

    /// Human-readable process description.
    #[serde(default)]
    pub description: Option<String>,
}

/// Parameters used to paginate process listings.
#[derive(Debug, Clone, Default)]
pub struct ProcessListRequest {
    /// Maximum non-zero number of processes returned on one page.
    pub limit: Option<NonZeroU32>,

    /// Requested result ordering.
    pub sort_by: Option<ProcessSort>,
}

impl ProcessListRequest {
    /// Sets the maximum number of processes returned on one page.
    pub fn with_limit(mut self, limit: NonZeroU32) -> Self {
        self.limit = Some(limit);
        self
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct ProcessListPayload {
    #[serde(default)]
    pub(super) links: Vec<PageLink>,

    pub(super) processes: Vec<ProcessDefinition>,
}

impl PagePayload<ProcessDefinition> for ProcessListPayload {
    fn into_page(self) -> (Vec<ProcessDefinition>, Vec<PageLink>) {
        (self.processes, self.links)
    }
}

const fn default_true() -> bool {
    true
}

fn known_job_status(value: &str) -> Option<JobStatus> {
    Some(match value {
        "accepted" => JobStatus::Accepted,
        "running" => JobStatus::Running,
        "successful" => JobStatus::Successful,
        "failed" => JobStatus::Failed,
        "rejected" => JobStatus::Rejected,
        "dismissed" => JobStatus::Dismissed,
        "deleted" => JobStatus::Deleted,
        _ => return None,
    })
}

fn deserialize_echoed_request<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Selection>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<EchoedRequest>::deserialize(deserializer)?.map(|request| request.ids))
}
