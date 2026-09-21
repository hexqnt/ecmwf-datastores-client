use std::{
    io::SeekFrom,
    path::{Path, PathBuf},
};

use futures_core::Stream;
use futures_util::StreamExt;
use log::{debug, info, trace};
use reqwest::{
    Method, StatusCode,
    header::{
        CONTENT_ENCODING, CONTENT_RANGE, ETAG, HeaderMap, HeaderValue, IF_RANGE, LAST_MODIFIED,
        RANGE,
    },
};
use serde::Deserialize;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use url::Url;

use crate::{
    error::{Error, Result},
    http::{DownloadAttemptBudget, HttpClient, JsonResponse, RequestTemplate, RetryPolicy},
};

enum ResponseAction {
    Transfer,

    Complete,
}

/// Controls how a completed download is committed when its target already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExistingTarget {
    /// Fail at commit time and preserve the existing file.
    Error,

    /// Atomically replace the existing file with the completed download.
    Replace,
}

/// Downloadable result asset and its metadata.
#[derive(Clone)]
pub struct Asset {
    url: Url,

    http: HttpClient,

    file_size: Option<u64>,

    content_type: Option<String>,
}

impl Asset {
    /// Returns the remote asset URL.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Returns the expected asset size in bytes, when reported by the service.
    pub fn file_size(&self) -> Option<u64> {
        self.file_size
    }

    /// Returns the reported media type.
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// Derives a local filename from the asset URL.
    pub fn suggested_filename(&self) -> PathBuf {
        self.url
            .path_segments()
            .and_then(std::iter::Iterator::last)
            .filter(|segment| !segment.is_empty())
            .map_or_else(|| PathBuf::from("download.bin"), PathBuf::from)
    }

    /// Downloads the complete asset into memory.
    ///
    /// The returned bytes are checked against the expected asset size when the
    /// service reports one. Transient failures while reading the response body
    /// retry the complete GET within the configured attempt budget. Use
    /// [`Self::byte_stream`] to process large assets without buffering them in
    /// full.
    pub async fn bytes(&self) -> Result<bytes::Bytes> {
        let mut budget = self.http.download_attempt_budget();
        loop {
            let response = self.open_response_with_budget(None, &mut budget).await?;
            let bytes = match response.bytes().await {
                Ok(bytes) => bytes,
                Err(error) => {
                    let error = error.without_url();
                    if budget.can_retry() && RetryPolicy::should_retry_error(&error) {
                        self.http.wait_to_resume(&mut budget).await;
                        continue;
                    }
                    return Err(Error::Http(error));
                }
            };
            let downloaded = u64::try_from(bytes.len())
                .map_err(|_| Error::Download("asset size does not fit in u64".into()))?;
            if let Some(expected) = self.file_size
                && downloaded != expected
            {
                return Err(Error::Download(format!(
                    "downloaded {downloaded} bytes but expected {expected}"
                )));
            }
            return Ok(bytes);
        }
    }

    /// Streams asset bytes without sending the API key to the asset URL.
    ///
    /// Dropping the stream stops the transfer. Use [`Self::download_to`] when
    /// size verification and replacement of the target file are needed.
    pub async fn byte_stream(&self) -> Result<impl Stream<Item = Result<bytes::Bytes>> + Send> {
        Ok(self
            .open_response(None)
            .await?
            .bytes_stream()
            .map(|chunk| chunk.map_err(|err| Error::Http(err.without_url()))))
    }

    /// Writes to a temporary file and moves it to the target after verification.
    ///
    /// Interrupted transfers are resumed with HTTP range requests when a
    /// response validator is available. Otherwise the transfer restarts from
    /// byte zero. A full response also restarts the temporary file when a
    /// range is ignored or the asset has changed.
    ///
    /// Dropping the returned future before commit stops the transfer and removes
    /// the temporary file. Once commit starts in the blocking pool, it may
    /// finish after cancellation. `existing_target` is enforced at commit time.
    pub async fn download_to(
        &self,
        target: impl AsRef<Path>,
        existing_target: ExistingTarget,
    ) -> Result<PathBuf> {
        self.download_to_with_progress(target, existing_target, |_| {})
            .await
    }
    /// Writes to a temporary file and reports absolute transfer progress.
    ///
    /// The observer receives an initial snapshot and further snapshots when
    /// the transfer position or known total may change, including when a retry
    /// restarts the transfer from byte zero. The existing target policy and all
    /// other guarantees of [`Self::download_to`] are preserved.
    pub async fn download_to_with_progress(
        &self,
        target: impl AsRef<Path>,
        existing_target: ExistingTarget,
        mut observe: impl FnMut(DownloadProgress) + Send,
    ) -> Result<PathBuf> {
        let mut path = target.as_ref().to_path_buf();
        if tokio::fs::metadata(&path)
            .await
            .is_ok_and(|metadata| metadata.is_dir())
        {
            path.push(self.suggested_filename());
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent).await?;
        let mut budget = self.http.download_attempt_budget();
        let (temp, file) = create_temp_download(parent.to_path_buf()).await?;
        let mut writer = tokio::fs::File::from_std(file);
        let mut state = DownloadState::new(self.file_size);
        observe(state.progress());
        loop {
            state.restart_without_validator(&mut writer).await?;
            observe(state.progress());
            let response = self
                .open_response_with_budget(state.resume_request(), &mut budget)
                .await?;
            if matches!(
                state.prepare_response(&response, &mut writer).await?,
                ResponseAction::Complete
            ) {
                observe(state.progress());
                return persist_download(writer, temp, path, existing_target).await;
            }
            observe(state.progress());
            let transfer = transfer_response(response, &mut writer, &mut state, &mut observe).await;
            if transfer.is_ok() {
                state.validate_transferred_range()?;
            }
            match transfer {
                Ok(()) if state.is_complete() => {
                    return persist_download(writer, temp, path, existing_target).await;
                }
                Ok(()) if budget.can_retry() && state.can_retry_incomplete() => {}
                Ok(()) => return Err(state.incomplete_error()),
                Err(Error::Http(err))
                    if budget.can_retry() && RetryPolicy::should_retry_error(&err) => {}
                Err(err) => return Err(err),
            }
            self.http.wait_to_resume(&mut budget).await;
        }
    }

    async fn open_response(&self, resume: Option<ResumeRequest<'_>>) -> Result<reqwest::Response> {
        let mut budget = self.http.download_attempt_budget();
        self.open_response_with_budget(resume, &mut budget).await
    }
    async fn open_response_with_budget(
        &self,
        resume: Option<ResumeRequest<'_>>,
        budget: &mut DownloadAttemptBudget,
    ) -> Result<reqwest::Response> {
        let mut template = RequestTemplate::new(Method::GET, self.url.clone())
            .for_asset()
            .with_log_messages(false);
        let is_resume = resume.is_some();
        let response = if let Some(resume) = resume {
            let range = HeaderValue::from_str(&format!("bytes={}-", resume.offset))
                .map_err(|err| Error::Download(format!("invalid byte range: {err}")))?;
            template = template.with_header(RANGE, range);
            template = template.with_header(IF_RANGE, resume.validator.clone());
            self.http
                .execute_raw_allowing_with_budget(
                    &template,
                    Some(StatusCode::RANGE_NOT_SATISFIABLE),
                    budget,
                )
                .await
        } else {
            self.http
                .execute_raw_allowing_with_budget(&template, None, budget)
                .await
        }?;
        if !is_resume && response.status() != StatusCode::OK {
            return Err(Error::Download(format!(
                "unexpected HTTP status {} for a complete asset request",
                response.status()
            )));
        }
        reject_encoded_asset(&response)?;
        Ok(response)
    }
}

impl std::fmt::Debug for Asset {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Asset")
            .field("file_size", &self.file_size)
            .field("content_type", &self.content_type)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct AssetEntry {
    pub(super) value: AssetValue,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct AssetValue {
    href: String,

    #[serde(
        default,
        rename = "file:size",
        deserialize_with = "deserialize_file_size"
    )]
    pub(super) file_size: Option<u64>,

    #[serde(rename = "type")]
    pub(super) content_type: Option<String>,
}

/// Results of a completed job.
#[derive(Debug, Clone)]
pub struct JobResults {
    asset: Asset,
}

impl JobResults {
    pub(super) fn new(
        response: &JsonResponse,
        http: HttpClient,
        value: AssetValue,
    ) -> Result<Self> {
        let url = response.url().join(&value.href)?;
        Ok(Self {
            asset: Asset {
                url,
                file_size: value.file_size,
                content_type: value.content_type,
                http,
            },
        })
    }

    /// Returns the single downloadable result asset.
    pub fn asset(&self) -> &Asset {
        &self.asset
    }

    /// Returns the result media type, when reported by the service.
    pub fn content_type(&self) -> Option<&str> {
        self.asset.content_type()
    }

    /// Derives a local filename from the result URL.
    pub fn suggested_filename(&self) -> PathBuf {
        self.asset.suggested_filename()
    }

    /// Downloads the result atomically to `target`.
    ///
    /// Dropping the returned future before commit removes the temporary file.
    /// Once commit starts, it may finish after cancellation. The
    /// `existing_target` policy is enforced at commit time.
    pub async fn download_to(
        &self,
        target: impl AsRef<Path>,
        existing_target: ExistingTarget,
    ) -> Result<PathBuf> {
        self.asset.download_to(target, existing_target).await
    }
    /// Downloads the result atomically while reporting absolute byte progress.
    pub async fn download_to_with_progress(
        &self,
        target: impl AsRef<Path>,
        existing_target: ExistingTarget,
        observe: impl FnMut(DownloadProgress) + Send,
    ) -> Result<PathBuf> {
        self.asset
            .download_to_with_progress(target, existing_target, observe)
            .await
    }
}

#[derive(Clone, Copy)]
struct ContentRange {
    end: u64,

    start: u64,

    complete_length: Option<u64>,
}

impl ContentRange {
    fn parse(value: &HeaderValue) -> Option<Self> {
        let value = value.to_str().ok()?.strip_prefix("bytes ")?;
        let (range, complete_length) = value.split_once('/')?;
        let (start, end) = range.split_once('-')?;
        let start = start.parse().ok()?;
        let end = end.parse().ok()?;
        let complete_length = if complete_length == "*" {
            None
        } else {
            Some(complete_length.parse().ok()?)
        };
        (start <= end).then_some(Self {
            end,
            start,
            complete_length,
        })
    }

    fn unsatisfied_length(value: &HeaderValue) -> Option<u64> {
        value.to_str().ok()?.strip_prefix("bytes */")?.parse().ok()
    }
}

#[derive(Clone, Copy)]
struct ResumeRequest<'a> {
    offset: u64,

    validator: &'a HeaderValue,
}

#[derive(Debug, Deserialize)]
pub(super) struct ResultsPayload {
    pub(super) asset: AssetEntry,
}

struct DownloadState {
    validator: Option<HeaderValue>,

    downloaded: u64,

    response_end: Option<u64>,
    response_expected: Option<u64>,

    metadata_expected: Option<u64>,
}

impl DownloadState {
    fn new(expected: Option<u64>) -> Self {
        Self {
            metadata_expected: expected,
            response_expected: None,
            downloaded: 0,
            validator: None,
            response_end: None,
        }
    }

    fn expected(&self) -> Option<u64> {
        self.metadata_expected.or(self.response_expected)
    }

    fn progress(&self) -> DownloadProgress {
        DownloadProgress {
            downloaded: self.downloaded,
            total: self.expected(),
        }
    }

    fn is_complete(&self) -> bool {
        self.expected().is_none_or(|size| size == self.downloaded)
    }

    fn resume_request(&self) -> Option<ResumeRequest<'_>> {
        self.validator
            .as_ref()
            .filter(|_| self.downloaded > 0)
            .map(|validator| ResumeRequest {
                offset: self.downloaded,
                validator,
            })
    }

    fn position_after(&self, chunk_len: usize) -> Result<u64> {
        let downloaded = self
            .downloaded
            .checked_add(chunk_len as u64)
            .ok_or_else(|| Error::Download("asset size overflows u64".into()))?;
        if let Some(end) = self.response_end
            && downloaded.saturating_sub(1) > end
        {
            return Err(Error::Download(format!(
                "partial response exceeds its Content-Range ending at byte {end}"
            )));
        }
        if let Some(expected) = self.expected()
            && downloaded > expected
        {
            return Err(Error::Download(format!(
                "downloaded {downloaded} bytes but expected {expected}"
            )));
        }
        Ok(downloaded)
    }

    fn incomplete_error(&self) -> Error {
        Error::Download(format!(
            "downloaded {} bytes but expected {}",
            self.downloaded,
            self.expected().unwrap_or_default()
        ))
    }

    fn can_retry_incomplete(&self) -> bool {
        self.expected().is_some_and(|size| self.downloaded < size)
    }

    fn handle_unsatisfied_range(&self, response: &reqwest::Response) -> Result<ResponseAction> {
        let reported = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(ContentRange::unsatisfied_length);
        if self.expected().is_some_and(|size| size == self.downloaded)
            && reported.is_none_or(|size| size == self.downloaded)
        {
            return Ok(ResponseAction::Complete);
        }
        Err(Error::Download(format!(
            "server rejected a range starting at byte {}{}",
            self.downloaded,
            reported.map_or_else(String::new, |size| format!(
                "; remote object is {size} bytes"
            ))
        )))
    }

    fn validate_partial_response(&mut self, response: &reqwest::Response) -> Result<()> {
        let range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(ContentRange::parse)
            .ok_or_else(|| {
                Error::Download("partial response has an invalid Content-Range header".to_string())
            })?;
        if range.start != self.downloaded {
            return Err(Error::Download(format!(
                "partial response starts at byte {}, expected {}",
                range.start, self.downloaded
            )));
        }
        if let Some(expected) = &self.validator
            && resume_validator(response.headers()).as_ref() != Some(expected)
        {
            return Err(Error::Download(
                "asset validator changed or is missing during download".into(),
            ));
        }
        if let Some(complete_length) = range.complete_length {
            if range.end >= complete_length {
                return Err(Error::Download(format!(
                    "partial response ends at byte {} beyond its complete length {complete_length}",
                    range.end
                )));
            }
            if let Some(expected) = self.expected()
                && expected != complete_length
            {
                return Err(Error::Download(format!(
                    "partial response reports {complete_length} bytes but expected {expected}"
                )));
            }
            self.response_expected = Some(complete_length);
        }
        if self.expected().is_none() {
            return Err(Error::Download(
                "partial response does not report the complete asset size".into(),
            ));
        }
        self.response_end = Some(range.end);
        Ok(())
    }
    fn validate_transferred_range(&self) -> Result<()> {
        if let Some(end) = self.response_end
            && end.checked_add(1) != Some(self.downloaded)
        {
            return Err(Error::Download(format!(
                "partial response ended at byte {end} but transferred through byte {}",
                self.downloaded.saturating_sub(1)
            )));
        }
        Ok(())
    }

    async fn prepare_response(
        &mut self,
        response: &reqwest::Response,
        writer: &mut tokio::fs::File,
    ) -> Result<ResponseAction> {
        match response.status() {
            StatusCode::OK => {
                if self.downloaded > 0 {
                    debug!(
                        "asset server returned a full response for a range request; restarting download"
                    );
                    writer.set_len(0).await?;
                    writer.seek(SeekFrom::Start(0)).await?;
                    self.downloaded = 0;
                }
                self.validator = resume_validator(response.headers());
                self.response_expected = response.content_length();
                self.response_end = None;
                Ok(ResponseAction::Transfer)
            }
            StatusCode::PARTIAL_CONTENT => {
                self.validate_partial_response(response)?;
                Ok(ResponseAction::Transfer)
            }
            StatusCode::RANGE_NOT_SATISFIABLE => self.handle_unsatisfied_range(response),
            status => Err(Error::Download(format!(
                "unexpected HTTP status {status} while downloading asset"
            ))),
        }
    }

    async fn restart_without_validator(&mut self, writer: &mut tokio::fs::File) -> Result<()> {
        if self.downloaded > 0 && self.validator.is_none() {
            writer.set_len(0).await?;
            writer.seek(SeekFrom::Start(0)).await?;
            self.downloaded = 0;
            self.response_expected = None;
        }
        Ok(())
    }
}

/// A snapshot of an asset transfer reported while downloading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DownloadProgress {
    total: Option<u64>,

    downloaded: u64,
}

impl DownloadProgress {
    /// Returns the expected complete asset size, when known.
    pub const fn total(self) -> Option<u64> {
        self.total
    }

    /// Returns the number of bytes currently written to the temporary file.
    pub const fn downloaded(self) -> u64 {
        self.downloaded
    }
}

fn resume_validator(headers: &HeaderMap) -> Option<HeaderValue> {
    headers
        .get(ETAG)
        .filter(|value| value.as_bytes().get(..2) != Some(b"W/"))
        .or_else(|| headers.get(LAST_MODIFIED))
        .cloned()
}

fn reject_encoded_asset(response: &reqwest::Response) -> Result<()> {
    let Some(encoding) = response.headers().get(CONTENT_ENCODING) else {
        return Ok(());
    };
    let encoding = encoding.to_str().map_err(|_| {
        Error::Download("asset response has an invalid Content-Encoding header".into())
    })?;
    if encoding.eq_ignore_ascii_case("identity") {
        Ok(())
    } else {
        Err(Error::Download(format!(
            "asset server ignored `Accept-Encoding: identity` and returned `{encoding}`"
        )))
    }
}

fn deserialize_file_size<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptionalFileSizeVisitor;
    struct FileSizeVisitor;

    impl<'de> serde::de::Visitor<'de> for OptionalFileSizeVisitor {
        type Value = Option<u64>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a non-negative integer, an integer string, or null")
        }

        fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(FileSizeVisitor).map(Some)
        }
    }

    impl serde::de::Visitor<'_> for FileSizeVisitor {
        type Value = u64;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a non-negative integer or an integer string")
        }

        fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
            Ok(value)
        }

        fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            value.parse().map_err(E::custom)
        }
    }

    deserializer.deserialize_option(OptionalFileSizeVisitor)
}

async fn persist_download(
    mut writer: tokio::fs::File,
    temp: tempfile::NamedTempFile,
    path: PathBuf,
    existing_target: ExistingTarget,
) -> Result<PathBuf> {
    writer.flush().await?;
    drop(writer);
    let path = run_file_operation(move || {
        match existing_target {
            ExistingTarget::Error => temp.persist_noclobber(&path),
            ExistingTarget::Replace => temp.persist(&path),
        }
        .map_err(|err| err.error)?;
        Ok(path)
    })
    .await?;
    info!("saved results to {}", path.display());
    Ok(path)
}

async fn transfer_response(
    mut response: reqwest::Response,
    writer: &mut tokio::fs::File,
    state: &mut DownloadState,
    observe: &mut (impl FnMut(DownloadProgress) + Send),
) -> Result<()> {
    let start = state.downloaded;
    let mut chunks = 0usize;
    let mut smallest = usize::MAX;
    let mut largest = 0usize;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| Error::Http(err.without_url()))?
    {
        let downloaded = state.position_after(chunk.len())?;
        writer.write_all(&chunk).await?;
        state.downloaded = downloaded;
        observe(state.progress());
        chunks += 1;
        smallest = smallest.min(chunk.len());
        largest = largest.max(chunk.len());
    }
    writer.flush().await?;
    trace!(
        "asset transfer: {chunks} chunks, {} bytes, chunk size {}..{} bytes",
        state.downloaded - start,
        if chunks == 0 { 0 } else { smallest },
        largest
    );
    Ok(())
}

async fn run_file_operation<T: Send + 'static>(
    operation: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T> {
    Ok(tokio::task::spawn_blocking(operation)
        .await
        .map_err(std::io::Error::other)??)
}
async fn create_temp_download(parent: PathBuf) -> Result<(tempfile::NamedTempFile, std::fs::File)> {
    run_file_operation(move || {
        let temp = tempfile::Builder::new()
            .prefix(".ecmwf-download-")
            .tempfile_in(parent)?;
        let file = temp.reopen()?;
        Ok((temp, file))
    })
    .await
}
