use std::{path::PathBuf, time::Duration};

use chrono::{DateTime, Utc};
use log::debug;
use reqwest::Method;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::{
    catalogue::Link,
    error::{Error, Result},
    http::{HttpClient, JsonResponse, RequestTemplate},
    id::JobId,
};

use super::{
    ProcessingConfig,
    download::{ExistingTarget, JobResults, ResultsPayload},
    models::{JobDetails, JobLogEntry, JobReceipt, JobStatus},
};

#[derive(Clone, Copy)]
enum CompletionGoal {
    Results,

    Receipt,
}

#[derive(Clone, Copy)]
enum JobResponseKind {
    Poll,

    Minimal,

    Details,
}

#[derive(Debug, Clone, Default)]
pub(super) struct LogCursor {
    time: Option<DateTime<Utc>>,

    count: usize,
}

impl LogCursor {
    pub(super) fn accept(&mut self, time: DateTime<Utc>, ordinal: usize) -> bool {
        match self.time {
            Some(previous) if time < previous => false,
            Some(previous) if time == previous && ordinal <= self.count => false,
            _ => {
                self.time = Some(time);
                self.count = ordinal;
                true
            }
        }
    }
}

/// Client-side handle used to track and download a remote processing job.
#[derive(Clone)]
pub struct Job {
    id: JobId,

    url: Url,

    http: HttpClient,

    config: ProcessingConfig,

    log_cursor: LogCursor,
}

impl Job {
    /// Returns the identifier used to restore this job later.
    pub fn id(&self) -> &JobId {
        &self.id
    }

    /// Builds a new remote job handle using the monitor URL returned by the API.
    pub(crate) fn new(
        mut job_url: Url,
        http: HttpClient,
        config: ProcessingConfig,
    ) -> Result<Self> {
        http.validate_api_url(&job_url)?;
        let job_id = JobId::parse(
            job_url
                .path_segments()
                .and_then(|segments| segments.rev().find(|segment| !segment.is_empty()))
                .unwrap_or_default(),
        )?;
        let trailing_slashes = job_url
            .path()
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'/')
            .count();
        if trailing_slashes > 0 {
            let mut segments = job_url.path_segments_mut().map_err(|()| {
                Error::InvalidResponse("job URL cannot contain path segments".into())
            })?;
            for _ in 0..trailing_slashes {
                segments.pop_if_empty();
            }
        }
        Ok(Self {
            id: job_id,
            url: job_url,
            http,
            config,
            log_cursor: LogCursor::default(),
        })
    }

    fn child_url(&self, segment: &str) -> Result<Url> {
        let mut url = self.url.clone();
        url.set_query(None);
        url.set_fragment(None);
        url.path_segments_mut()
            .map_err(|()| Error::InvalidResponse("job URL cannot contain path segments".into()))?
            .push(segment);
        Ok(url)
    }

    fn request_template(&self, kind: JobResponseKind) -> RequestTemplate {
        let mut template = RequestTemplate::new(Method::GET, self.url.clone());
        if matches!(kind, JobResponseKind::Details) {
            template = template.with_query_pair("request", "true");
        }
        if matches!(kind, JobResponseKind::Poll) {
            template = template.with_query_pair("log", "true");
            if let Some(ts) = self.log_cursor.time {
                template = template.with_query_pair("logStartTime", ts.to_rfc3339());
            }
        }
        template
    }

    fn emit_log_entries<'a>(&mut self, entries: impl IntoIterator<Item = &'a JobLogEntry>) {
        let mut current_time = None;
        let mut ordinal = 0;
        for entry in entries {
            if current_time == Some(entry.timestamp) {
                ordinal += 1;
            } else {
                current_time = Some(entry.timestamp);
                ordinal = 1;
            }
            if self.log_cursor.accept(entry.timestamp, ordinal) {
                log_job_entry(entry);
            }
        }
    }

    fn results_url(&self, response: &JsonResponse) -> Result<Url> {
        if let Some(url) = response.link_href_optional("results")? {
            return Ok(url);
        }
        self.child_url("results")
    }
    fn results_url_from_links(&self, links: &[Link]) -> Result<Url> {
        let mut matches = links
            .iter()
            .filter(|link| link.rel.as_deref() == Some("results"));
        let Some(first) = matches.next() else {
            return self.child_url("results");
        };
        if matches.next().is_some() {
            return Err(Error::link("results"));
        }
        Ok(self
            .url
            .join(&first.href)
            .or_else(|_| Url::parse(&first.href))?)
    }

    async fn poll(&mut self) -> Result<JobDetails> {
        let details = self.fetch_details(JobResponseKind::Poll).await?;
        if let Some(metadata) = &details.metadata {
            if metadata.log.is_sorted_by_key(|entry| entry.timestamp) {
                self.emit_log_entries(metadata.log.iter());
            } else {
                let mut entries: Vec<_> = metadata.log.iter().collect();
                entries.sort_by_key(|entry| entry.timestamp);
                self.emit_log_entries(entries);
            }
        }
        Ok(details)
    }

    /// Deletes this job from the server.
    pub async fn delete(&self) -> Result<()> {
        let template =
            RequestTemplate::new(Method::DELETE, self.url.clone()).with_log_messages(false);
        self.http.execute(&template).await?;
        Ok(())
    }

    /// Fetches the current job status without waiting for completion.
    pub async fn status(&self) -> Result<JobStatus> {
        Ok(self
            .http
            .execute_typed::<JobStatusPayload>(&self.request_template(JobResponseKind::Minimal))
            .await?
            .status)
    }

    /// Fetches the current typed job response without waiting.
    pub async fn details(&self) -> Result<JobDetails> {
        self.http
            .execute_typed(&self.request_template(JobResponseKind::Details))
            .await
    }

    /// Fetches the current job response as JSON without parsing it into [`JobDetails`].
    pub async fn raw_json(&self) -> Result<Value> {
        Ok(self
            .http
            .execute(&self.request_template(JobResponseKind::Details))
            .await?
            .into_body())
    }

    async fn completion(&self, details: &JobDetails, goal: CompletionGoal) -> Result<bool> {
        match &details.status {
            JobStatus::Successful => Ok(true),
            JobStatus::Failed if matches!(goal, CompletionGoal::Receipt) => Ok(true),
            JobStatus::Accepted | JobStatus::Running => Ok(false),
            JobStatus::Failed | JobStatus::Rejected => Err(Error::JobFailed {
                job_id: self.id.clone(),
                status: details.status.clone(),
                message: self.failure_message(details).await,
            }),
            JobStatus::Dismissed | JobStatus::Deleted => Err(Error::JobFailed {
                job_id: self.id.clone(),
                status: details.status.clone(),
                message: format!("job {} by server", details.status),
            }),
            JobStatus::Unknown(value) => Err(Error::InvalidResponse(format!(
                "unknown job status: {value}"
            ))),
        }
    }

    /// Waits for completion and fetches the receipt, including for failed jobs.
    ///
    /// Dropping the returned future stops local polling but leaves the remote
    /// job running.
    pub async fn receipt(&mut self) -> Result<Value> {
        self.wait_details(CompletionGoal::Receipt).await?;
        let template = RequestTemplate::new(Method::GET, self.child_url("receipt")?);
        Ok(self.http.execute(&template).await?.into_body())
    }
    /// Waits for a receipt and parses its stable fields.
    pub async fn receipt_details(&mut self) -> Result<JobReceipt> {
        self.wait_details(CompletionGoal::Receipt).await?;
        let template = RequestTemplate::new(Method::GET, self.child_url("receipt")?);
        self.http.execute_typed(&template).await
    }

    async fn fetch_details(&self, kind: JobResponseKind) -> Result<JobDetails> {
        self.http.execute_typed(&self.request_template(kind)).await
    }

    /// Waits for job completion and returns its details.
    ///
    /// Dropping the returned future stops local polling but does not cancel or
    /// delete the remote job.
    pub async fn wait(&mut self) -> Result<JobDetails> {
        self.wait_details(CompletionGoal::Results).await
    }
    async fn wait_details(&mut self, goal: CompletionGoal) -> Result<JobDetails> {
        let timeout = self.config.wait_timeout;
        let job_id = self.id.clone();
        tokio::time::timeout(timeout, self.wait_until_complete(goal))
            .await
            .map_err(|_| Error::WaitTimeout {
                job_id,
                elapsed: timeout,
            })?
    }
    /// Waits for successful completion and fetches the results.
    ///
    /// Dropping the returned future stops local polling but leaves the remote
    /// job running.
    pub async fn wait_for_results(&mut self) -> Result<JobResults> {
        let details = self.wait_details(CompletionGoal::Results).await?;
        let results_url = self.results_url_from_links(&details.links)?;
        self.results_from_url(results_url).await
    }
    /// Waits for completion and downloads the job results to the given path.
    ///
    /// Dropping the returned future stops local polling or transfer, leaves the
    /// remote job intact, and does not replace the target with a partial file.
    pub async fn wait_and_download(
        &mut self,
        target: Option<PathBuf>,
        existing_target: ExistingTarget,
    ) -> Result<PathBuf> {
        let results = self.wait_for_results().await?;
        let path = target.unwrap_or_else(|| results.suggested_filename());
        results.download_to(path, existing_target).await
    }
    async fn wait_until_complete(&mut self, goal: CompletionGoal) -> Result<JobDetails> {
        let mut delay = Duration::from_secs(1).min(self.config.poll_max_delay);
        loop {
            let details = self.poll().await?;
            if self.completion(&details, goal).await? {
                return Ok(details);
            }
            debug!(
                "job {} still in {:?}, waiting {:?}",
                self.id(),
                details.status,
                delay
            );
            tokio::time::sleep(delay).await;
            delay = delay.mul_f64(1.5).min(self.config.poll_max_delay);
        }
    }

    async fn failure_message(&self, details: &JobDetails) -> String {
        let Ok(url) = self.results_url_from_links(&details.links) else {
            return "processing failed".into();
        };
        let template = RequestTemplate::new(Method::GET, url);
        match self.http.execute(&template).await {
            Err(Error::Api { message, .. }) => message,
            Ok(response) => response.body().to_string(),
            Err(_) => "processing failed".into(),
        }
    }

    /// Fetches the current Results API response without polling.
    pub async fn results_once(&self) -> Result<JobResults> {
        let response = self
            .http
            .execute(&self.request_template(JobResponseKind::Minimal))
            .await?;
        self.results_from_response(&response).await
    }
    /// Checks readiness in one request; failed jobs return [`Error::JobFailed`].
    pub async fn results_ready(&self) -> Result<bool> {
        let details = self.fetch_details(JobResponseKind::Minimal).await?;
        self.completion(&details, CompletionGoal::Results).await
    }
    async fn results_from_url(&self, results_url: Url) -> Result<JobResults> {
        let template = RequestTemplate::new(Method::GET, results_url);
        let response = self.http.execute(&template).await?;
        let payload: ResultsPayload = response.parse()?;
        JobResults::new(&response, self.http.clone(), payload.asset.value)
    }
    async fn results_from_response(&self, job_response: &JsonResponse) -> Result<JobResults> {
        let results_url = self.results_url(job_response)?;
        self.results_from_url(results_url).await
    }
}

impl std::fmt::Debug for Job {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Job")
            .field("id", &self.id)
            .field("log_cursor", &self.log_cursor)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct JobStatusPayload {
    status: JobStatus,
}

fn log_job_entry(entry: &JobLogEntry) {
    log::log!(
        entry.level,
        "[{}] {}",
        entry.timestamp.to_rfc3339(),
        entry.message
    );
}
