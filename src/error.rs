use std::{io, path::PathBuf, time::Duration};

use reqwest::StatusCode;
use thiserror::Error;

use crate::{id::JobId, processing::JobStatus};

/// Convenient alias for results returned by this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by the data stores client.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A local filesystem or stream operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// A URL returned by the caller or service was invalid.
    #[error("URL parse error: {0}")]
    Url(#[from] url::ParseError),

    /// The service returned a non-success response unrelated to authentication.
    #[error("API error {status}: {message}")]
    Api {
        /// Parsed response body, or a JSON string for a non-JSON body.
        body: serde_json::Value,

        /// HTTP response status.
        status: StatusCode,

        /// Human-readable message extracted from the response.
        message: String,
    },

    /// The HTTP transport failed.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// JSON serialization or deserialization failed.
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),

    /// A required or uniquely expected hypermedia link was absent or ambiguous.
    #[error("API link `{rel}` not found or ambiguous")]
    Link {
        /// Requested link relation.
        rel: String,
    },

    /// Retryable requests exhausted the configured policy.
    #[error("retry attempts exhausted after {attempts} tries ({elapsed:?})")]
    Retry {
        /// Total elapsed time across attempts and delays.
        elapsed: Duration,

        /// Number of attempts made.
        attempts: usize,

        /// Last observed error, when one was available.
        #[source]
        last_error: Option<Box<Self>>,
    },

    /// A result asset could not be downloaded or committed atomically.
    #[error("download failed: {0}")]
    Download(String),

    /// A job reached a terminal state without usable results.
    #[error("job {job_id} failed with status {status}: {message}")]
    JobFailed {
        /// Failed job identifier.
        job_id: JobId,

        /// Terminal job status.
        status: JobStatus,

        /// Failure detail reported by the service.
        message: String,
    },
    /// An operation failed after the service accepted a job.
    #[error("operation for job {job_id} failed: {source}")]
    JobOperation {
        /// Job that can be inspected or resumed later.
        job_id: JobId,

        /// Underlying operation error.
        #[source]
        source: Box<Self>,
    },

    /// The configured deadline elapsed while waiting for a job.
    #[error("timed out waiting for job {job_id} after {elapsed:?}")]
    WaitTimeout {
        /// Job that can be resumed later.
        job_id: JobId,

        /// Configured waiting duration.
        elapsed: Duration,
    },

    /// A credentials file could not be parsed.
    #[error("failed to parse configuration file {path}: {source}")]
    ConfigParse {
        /// Configuration file path.
        path: PathBuf,

        /// Underlying parse error.
        #[source]
        source: io::Error,
    },
    /// The requested credentials file does not exist.
    #[error("configuration file not found at {path}")]
    ConfigNotFound {
        /// Missing configuration file path.
        path: PathBuf,
    },

    /// The service rejected the supplied API key.
    #[error("authentication failed ({status}): {message}")]
    Authentication {
        /// Parsed response body, or a JSON string for a non-JSON body.
        body: serde_json::Value,

        /// HTTP response status.
        status: StatusCode,

        /// Human-readable authentication failure message.
        message: String,
    },

    /// The API key was empty or invalid as an HTTP header value.
    #[error("invalid API key: {0}")]
    InvalidApiKey(String),
    /// The service response violated a client-side invariant.
    #[error("invalid API response: {0}")]
    InvalidResponse(String),
    /// A dataset selection was not a JSON object.
    #[error("invalid selection: {0}")]
    InvalidSelection(String),
    /// An identifier was empty or unsafe as a URL path segment.
    #[error("invalid identifier: {0}")]
    InvalidIdentifier(String),
    /// A client setting cannot be used safely.
    #[error("invalid client configuration: {0}")]
    InvalidConfiguration(String),

    /// Submission may have reached the service, but no response confirmed its job ID.
    #[error("submission outcome is unknown: {0}")]
    SubmissionUnknown(#[source] Box<Self>),

    /// The operating system did not provide a home directory.
    #[error("could not determine the home directory")]
    HomeDirectoryNotFound,

    /// A required environment variable was not set or was empty.
    #[error("environment variable `{0}` is missing")]
    MissingEnvironmentVariable(String),
}

impl Error {
    pub(crate) fn link<S: Into<String>>(rel: S) -> Self {
        Self::Link { rel: rel.into() }
    }

    /// Returns the recoverable job identifier carried by this error, if any.
    pub fn job_id(&self) -> Option<&JobId> {
        match self {
            Self::JobFailed { job_id, .. }
            | Self::WaitTimeout { job_id, .. }
            | Self::JobOperation { job_id, .. } => Some(job_id),
            _ => None,
        }
    }

    pub(crate) fn with_job_context(self, job_id: JobId) -> Self {
        if self.job_id().is_some() {
            self
        } else {
            Self::JobOperation {
                job_id,
                source: Box::new(self),
            }
        }
    }
}

impl From<Error> for io::Error {
    fn from(value: Error) -> Self {
        match value {
            Error::Io(err) => err,
            other => Self::other(other),
        }
    }
}
