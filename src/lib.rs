#![doc = include_str!("../README.md")]
#![warn(missing_docs)]

pub use catalogue::{
    BoundingBox, Collection, CollectionExtent, CollectionSearchFacet, CollectionSearchStats,
    CollectionSort, CollectionsPage, CollectionsRequest, Licence, Link, SpatialExtent,
    TemporalExtent, TemporalInterval,
};
pub use client::{Client, ClientBuilder};
pub use config::{ApiKey, Credentials};
pub use http::RetryPolicy;
pub use id::{CollectionId, JobId, LicenceId};
pub use processing::{
    Asset, DownloadProgress, ExistingTarget, Job, JobDetails, JobLogEntry, JobMetadata, JobReceipt,
    JobResults, JobSort, JobStatus, JobsPage, JobsRequest, ProcessDefinition, ProcessList,
    ProcessListRequest, ProcessSort, RequestCost,
};
pub use selection::Selection;

/// Provider-aware request planning built on top of the low-level client API.
pub mod planning;

/// Credential types and explicit credential-loading helpers.
pub mod config;
/// Errors returned by the client.
pub mod error;

mod catalogue;
mod client;
mod http;
mod id;
mod processing;
mod profile;
mod selection;
mod serde_helpers;
mod utils;
