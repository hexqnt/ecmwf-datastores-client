use std::{path::PathBuf, time::Duration};

use serde_json::{Map, Value};
use url::Url;

use crate::{
    catalogue::{CatalogueApi, Collection, CollectionsPage, CollectionsRequest, Licence},
    config::{ApiKey, Credentials},
    error::{Error, Result},
    http::{HttpClient, RetryPolicy},
    id::{CollectionId, JobId, LicenceId},
    processing::{
        DEFAULT_WAIT_TIMEOUT, ExistingTarget, Job, JobDetails, JobReceipt, JobResults, JobsPage,
        JobsRequest, ProcessDefinition, ProcessList, ProcessListRequest, ProcessingApi,
        ProcessingConfig, RequestCost,
    },
    profile::ProfileApi,
    selection::Selection,
    utils,
};

/// High-level client for ECMWF Data Stores.
#[derive(Clone)]
pub struct Client {
    profile: ProfileApi,

    base_url: Url,

    catalogue: CatalogueApi,

    processing: ProcessingApi,
}

impl Client {
    /// Builds a client from an explicit endpoint and optional API key.
    pub fn new(endpoint: Url, api_key: Option<ApiKey>) -> Result<Self> {
        let mut builder = Self::builder(endpoint);
        if let Some(api_key) = api_key {
            builder = builder.api_key(api_key);
        }
        builder.build()
    }

    /// Restores a job handle from a saved identifier without a network request.
    pub fn job(&self, job_id: &JobId) -> Result<Job> {
        self.processing.remote_for(job_id)
    }

    /// Returns a new [`ClientBuilder`] for an explicit endpoint.
    pub fn builder(endpoint: Url) -> ClientBuilder {
        ClientBuilder::new(endpoint)
    }

    /// Discovers credentials and builds a client.
    ///
    /// See [`Credentials::discover`] for source precedence.
    #[cfg(feature = "discovery")]
    pub fn discover() -> Result<Self> {
        Self::from_credentials(Credentials::discover()?)
    }

    /// Returns the configured base URL.
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    /// Builds a client from an explicit credentials source.
    pub fn from_credentials(credentials: Credentials) -> Result<Self> {
        ClientBuilder::from_credentials(credentials).build()
    }

    /// Lists jobs with optional filters.
    pub async fn jobs(&self, request: &JobsRequest) -> Result<JobsPage> {
        self.processing.get_jobs(request).await
    }

    /// Fetches a process definition.
    pub async fn process(&self, collection_id: &CollectionId) -> Result<ProcessDefinition> {
        self.processing.get_process(collection_id).await
    }

    /// Lists licences matching the given scope.
    pub async fn licences(&self, scope: Option<&str>) -> Result<Vec<Licence>> {
        self.catalogue.get_licences(scope).await
    }

    /// Submits a request and downloads the resulting asset.
    ///
    /// Dropping the returned future stops local waiting or transfer but does
    /// not delete a job already accepted by the service. The `existing_target`
    /// policy is enforced only after the complete asset has been downloaded.
    pub async fn retrieve(
        &self,
        collection_id: &CollectionId,
        request: &Selection,
        target: Option<PathBuf>,
        existing_target: ExistingTarget,
    ) -> Result<PathBuf> {
        let mut remote = self.processing.submit(collection_id, request).await?;
        let job_id = remote.id().clone();
        remote
            .wait_and_download(target, existing_target)
            .await
            .map_err(|error| error.with_job_context(job_id))
    }

    /// Lists available processes.
    pub async fn processes(&self, request: &ProcessListRequest) -> Result<ProcessList> {
        self.processing.list_processes(request).await
    }

    /// Fetches job details by id.
    pub async fn fetch_job(&self, job_id: &JobId) -> Result<JobDetails> {
        self.processing.job_details(job_id).await
    }

    /// Submits a processing request and returns a remote handle.
    pub async fn submit(&self, collection_id: &CollectionId, request: &Selection) -> Result<Job> {
        self.processing.submit(collection_id, request).await
    }
    /// Submits a request and waits for completion.
    ///
    /// Dropping the returned future stops local polling but does not delete a
    /// job already accepted by the service.
    pub async fn submit_and_wait(
        &self,
        collection_id: &CollectionId,
        request: &Selection,
    ) -> Result<JobResults> {
        let mut remote = self.processing.submit(collection_id, request).await?;
        let job_id = remote.id().clone();
        remote
            .wait_for_results()
            .await
            .map_err(|error| error.with_job_context(job_id))
    }

    /// Waits for a completed job and retrieves its receipt, including failed jobs.
    ///
    /// Dropping the returned future stops local polling but leaves the remote
    /// job intact.
    pub async fn receipt(&self, job_id: &JobId) -> Result<Value> {
        let mut job = self.job(job_id)?;
        job.receipt().await
    }
    /// Waits for a completed job and returns its typed receipt.
    pub async fn receipt_details(&self, job_id: &JobId) -> Result<JobReceipt> {
        let mut job = self.job(job_id)?;
        job.receipt_details().await
    }

    /// Lists catalogue collections.
    pub async fn collections(&self, request: &CollectionsRequest) -> Result<CollectionsPage> {
        self.catalogue.list_collections(request).await
    }

    /// Deletes one or more jobs.
    pub async fn delete_jobs(&self, job_ids: &[JobId]) -> Result<()> {
        self.processing.delete_jobs(job_ids).await
    }

    /// Retrieves all jobs following pagination.
    pub async fn all_jobs(&self, request: &JobsRequest) -> Result<Vec<JobDetails>> {
        self.processing.all_jobs(request).await
    }
    /// Retrieves all processes following pagination.
    pub async fn all_processes(
        &self,
        request: &ProcessListRequest,
    ) -> Result<Vec<ProcessDefinition>> {
        self.processing.all_processes(request).await
    }
    /// Collects all catalogue collections following pagination.
    pub async fn all_collections(&self, request: &CollectionsRequest) -> Result<Vec<Collection>> {
        self.catalogue.all_collections(request).await
    }

    /// Requests a cost estimate for a submission.
    pub async fn estimate_cost(
        &self,
        collection_id: &CollectionId,
        request: &Selection,
    ) -> Result<RequestCost> {
        self.processing.estimate_cost(collection_id, request).await
    }

    /// Accepts a licence revision.
    pub async fn accept_licence(&self, licence_id: &LicenceId, revision: u32) -> Result<Value> {
        self.profile.accept_licence(licence_id, revision).await
    }

    /// Stars (bookmarks) a collection.
    pub async fn star_collection(&self, collection_id: &CollectionId) -> Result<Vec<CollectionId>> {
        self.profile.star_collection(collection_id).await
    }

    /// Retrieves a single catalogue collection.
    pub async fn collection(&self, id: &CollectionId) -> Result<Collection> {
        self.catalogue.get_collection(id).await
    }
    /// Fetches the request form for a collection.
    pub async fn collection_form(&self, id: &CollectionId) -> Result<Vec<Map<String, Value>>> {
        self.catalogue.get_form(id).await
    }
    /// Fetches the published parameter constraints for a collection.
    pub async fn collection_constraints(
        &self,
        id: &CollectionId,
    ) -> Result<Vec<Map<String, Value>>> {
        self.catalogue.get_constraints(id).await
    }

    /// Fetches the current Results API response without waiting for completion.
    pub async fn job_results_once(&self, job_id: &JobId) -> Result<JobResults> {
        self.processing.job_results_once(job_id).await
    }

    /// Lists licences accepted by the current user.
    pub async fn accepted_licences(&self, scope: Option<&str>) -> Result<Vec<Licence>> {
        self.profile.accepted_licences(scope).await
    }

    /// Unstars a collection.
    pub async fn unstar_collection(&self, collection_id: &CollectionId) -> Result<()> {
        self.profile.unstar_collection(collection_id).await
    }

    /// Returns server-side allowed values for the supplied request parameters.
    /// A successful response does not confirm that the complete request can be submitted.
    pub async fn apply_constraints(
        &self,
        collection_id: &CollectionId,
        request: &Selection,
    ) -> Result<Value> {
        self.processing
            .apply_constraints(collection_id, request)
            .await
    }

    /// Fetches and logs current service broadcast messages.
    pub async fn broadcast_messages(&self) -> Result<()> {
        self.catalogue.broadcast_messages().await
    }

    /// Verifies the API key with the server.
    pub async fn check_authentication(&self) -> Result<Value> {
        self.profile.check_authentication().await
    }

    /// Downloads job results to a target path or suggested filename.
    ///
    /// Dropping the returned future stops local waiting or transfer, leaves the
    /// remote job intact. The `existing_target` policy is enforced only after
    /// the complete asset has been downloaded.
    pub async fn wait_and_download_results(
        &self,
        job_id: &JobId,
        target: Option<PathBuf>,
        existing_target: ExistingTarget,
    ) -> Result<PathBuf> {
        let mut job = self.job(job_id)?;
        job.wait_and_download(target, existing_target).await
    }
}
/// Configures how a [`Client`] is built.
#[derive(Clone)]
pub struct ClientBuilder {
    api_key: Option<ApiKey>,

    base_url: Url,

    verify_tls: bool,

    wait_timeout: Duration,

    retry_policy: RetryPolicy,

    poll_max_delay: Duration,

    request_timeout: Duration,

    download_idle_timeout: Duration,
}

impl ClientBuilder {
    /// Creates a builder for an explicit API endpoint.
    pub fn new(endpoint: Url) -> Self {
        Self {
            base_url: endpoint,
            api_key: None,
            verify_tls: true,
            request_timeout: Duration::from_secs(60),
            download_idle_timeout: Duration::from_secs(60),
            retry_policy: RetryPolicy::default(),
            poll_max_delay: Duration::from_secs(120),
            wait_timeout: DEFAULT_WAIT_TIMEOUT,
        }
    }

    /// Builds a fully configured [`Client`] without reading external configuration.
    pub fn build(self) -> Result<Client> {
        for (name, duration) in [
            ("request_timeout", self.request_timeout),
            ("download_idle_timeout", self.download_idle_timeout),
            ("poll_max_delay", self.poll_max_delay),
            ("wait_timeout", self.wait_timeout),
        ] {
            if duration.is_zero() {
                return Err(Error::InvalidConfiguration(format!(
                    "{name} must be greater than zero"
                )));
            }
        }
        let mut base_url = self.base_url;
        utils::ensure_trailing_slash(&mut base_url);

        let http = HttpClient::new(
            base_url.clone(),
            self.api_key,
            self.request_timeout,
            self.download_idle_timeout,
            self.verify_tls,
            self.retry_policy,
        )?;

        let catalogue_url = base_url.join("catalogue/v1")?;
        let processing_url = base_url.join("retrieve/v1")?;
        let profile_url = base_url.join("profiles/v1")?;

        let catalogue = CatalogueApi::new(catalogue_url, http.clone());
        let processing = ProcessingApi::new(
            processing_url,
            http.clone(),
            ProcessingConfig {
                poll_max_delay: self.poll_max_delay,
                wait_timeout: self.wait_timeout,
            },
        );
        let profile = ProfileApi::new(profile_url, http);

        let client = Client {
            profile,
            base_url,
            catalogue,
            processing,
        };

        Ok(client)
    }

    /// Sets the API key to authenticate requests.
    pub fn api_key(mut self, key: ApiKey) -> Self {
        self.api_key = Some(key);
        self
    }

    /// Enables or disables TLS certificate verification.
    ///
    /// Disabling verification weakens transport security and is intended only
    /// for controlled test environments.
    pub fn verify_tls(mut self, verify: bool) -> Self {
        self.verify_tls = verify;
        self
    }

    /// Sets the maximum total time spent waiting for a job to finish.
    pub fn wait_timeout(mut self, duration: Duration) -> Self {
        self.wait_timeout = duration;
        self
    }

    /// Configures retry behaviour.
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }

    /// Sets the maximum backoff duration when polling.
    pub fn poll_max_delay(mut self, duration: Duration) -> Self {
        self.poll_max_delay = duration;
        self
    }

    /// Sets the deadline for an API request, including its response body.
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// Creates a builder from credentials loaded by an explicit source.
    pub fn from_credentials(credentials: Credentials) -> Self {
        let (endpoint, api_key) = credentials.into_parts();
        let mut builder = Self::new(endpoint);
        builder.api_key = api_key;
        builder
    }

    /// Sets the connection and between-chunks idle timeout for asset downloads.
    pub fn download_idle_timeout(mut self, timeout: Duration) -> Self {
        self.download_idle_timeout = timeout;
        self
    }
}

impl std::fmt::Debug for ClientBuilder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClientBuilder")
            .field("origin", &self.base_url.origin())
            .field("api_key_configured", &self.api_key.is_some())
            .field("verify_tls", &self.verify_tls)
            .field("request_timeout", &self.request_timeout)
            .field("download_idle_timeout", &self.download_idle_timeout)
            .field("retry_policy", &self.retry_policy)
            .field("poll_max_delay", &self.poll_max_delay)
            .field("wait_timeout", &self.wait_timeout)
            .finish()
    }
}
