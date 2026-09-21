use std::{
    collections::HashSet,
    num::NonZeroUsize,
    sync::OnceLock,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use log::{Level, trace, warn};
use reqwest::{
    Client as ReqwestClient, Method, RequestBuilder, Response, StatusCode,
    header::{ACCEPT_ENCODING, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER},
    redirect::Policy,
};
use rustls_platform_verifier::ConfigVerifierExt;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use url::Url;

use crate::{
    config::ApiKey,
    error::{Error, Result},
    utils,
};

const USER_AGENT_VALUE: &str = concat!("ecmwf-datastores-client/", env!("CARGO_PKG_VERSION"));
const MAX_AUTO_PAGES: usize = 10_000;
// API and asset traffic need separate redirect and timeout policies. Reuse the
// expensive platform-verifier initialization while retaining two HTTP clients.
static TLS_CONFIG: OnceLock<std::result::Result<rustls::ClientConfig, String>> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct Paged<T> {
    url: Url,

    http: HttpClient,

    items: Vec<T>,

    links: Vec<PageLink>,
}

impl<T: Send + Sync> Paged<T> {
    pub(crate) fn new(items: Vec<T>, links: Vec<PageLink>, url: Url, http: HttpClient) -> Self {
        Self {
            url,
            http,
            items,
            links,
        }
    }

    pub(crate) fn http(&self) -> &HttpClient {
        &self.http
    }

    pub(crate) fn items(&self) -> &[T] {
        &self.items
    }

    pub(crate) fn into_items(self) -> Vec<T> {
        self.items
    }

    pub(crate) fn link_href_optional(&self, rel: &str) -> Result<Option<Url>> {
        let mut matches = self
            .links
            .iter()
            .filter(|link| link.rel.as_deref() == Some(rel));
        let Some(first) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            return Err(Error::link(rel.to_string()));
        }
        let href = first
            .href
            .as_deref()
            .ok_or_else(|| Error::link(rel.to_string()))?;
        Ok(Some(self.url.join(href).or_else(|_| Url::parse(href))?))
    }

    pub(crate) async fn fetch<P: DeserializeOwned + Send>(
        &self,
        rel: &str,
    ) -> Result<Option<TypedResponse<P>>> {
        let Some(url) = self.link_href_optional(rel)? else {
            return Ok(None);
        };
        let template = RequestTemplate::new(Method::GET, url);
        self.http.execute_typed_response(&template).await.map(Some)
    }

    pub(crate) async fn follow<P>(&self, rel: &str) -> Result<Option<Self>>
    where
        P: PagePayload<T> + Send,
    {
        let Some(response) = self.fetch::<P>(rel).await? else {
            return Ok(None);
        };
        let (items, links) = response.body.into_page();
        Ok(Some(Self::new(
            items,
            links,
            response.url,
            self.http.clone(),
        )))
    }

    pub(crate) async fn collect_all<P>(self) -> Result<Vec<T>>
    where
        P: PagePayload<T> + Send,
    {
        let mut page = self;
        let mut items = Vec::new();
        let mut visited = HashSet::new();
        loop {
            visited.insert(page.url.clone());
            items.append(&mut page.items);
            let Some(next_url) = page.link_href_optional("next")? else {
                return Ok(items);
            };
            if visited.contains(&next_url) {
                return Err(Error::InvalidResponse(
                    "pagination link cycle detected".into(),
                ));
            }
            if visited.len() >= MAX_AUTO_PAGES {
                return Err(Error::InvalidResponse(format!(
                    "pagination exceeded {MAX_AUTO_PAGES} pages"
                )));
            }
            let response = page
                .http
                .execute_typed_response::<P>(&RequestTemplate::new(Method::GET, next_url))
                .await?;
            let (next_items, links) = response.body.into_page();
            page = Self::new(next_items, links, response.url, page.http);
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PageLink {
    rel: Option<String>,

    href: Option<String>,
}

#[derive(Clone)]
pub struct HttpClient {
    retry: RetryPolicy,

    assets: ReqwestClient,

    api: ReqwestClient,
    api_key: Option<ApiKey>,
    api_origin: Url,
}

impl HttpClient {
    pub(crate) fn new(
        api_origin: Url,
        api_key: Option<ApiKey>,
        request_timeout: Duration,
        download_idle_timeout: Duration,
        verify_tls: bool,
        retry: RetryPolicy,
    ) -> Result<Self> {
        let loopback = matches!(
            api_origin.host_str(),
            Some("localhost" | "127.0.0.1" | "::1")
        );
        let tls = verify_tls.then(tls_config).transpose()?;
        let builder = || {
            let builder = ReqwestClient::builder()
                .user_agent(USER_AGENT_VALUE)
                .danger_accept_invalid_certs(!verify_tls);
            let builder = if let Some(tls) = &tls {
                builder.tls_backend_preconfigured(tls.clone())
            } else {
                builder
            };
            if loopback {
                builder.no_proxy()
            } else {
                builder
            }
        };
        Ok(Self {
            api: builder()
                .timeout(request_timeout)
                .redirect(Policy::none())
                .build()?,
            assets: builder()
                .connect_timeout(download_idle_timeout)
                .read_timeout(download_idle_timeout)
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd()
                .default_headers(HeaderMap::from_iter([(
                    ACCEPT_ENCODING,
                    HeaderValue::from_static("identity"),
                )]))
                .build()?,
            api_origin,
            api_key,
            retry,
        })
    }

    fn attempts_for(&self, template: &RequestTemplate) -> usize {
        if template.method == Method::GET || template.method == Method::HEAD {
            self.retry.max_attempts.get()
        } else {
            1
        }
    }

    fn build_request(&self, template: &RequestTemplate) -> Result<RequestBuilder> {
        let client = if template.authenticated {
            self.validate_api_url(&template.url)?;
            &self.api
        } else {
            &self.assets
        };
        let mut builder = client.request(template.method.clone(), template.url.clone());
        if !template.headers.is_empty() {
            builder = builder.headers(template.headers.clone());
        }
        if template.authenticated
            && let Some(key) = &self.api_key
        {
            builder = builder.header("PRIVATE-TOKEN", key.as_header_value());
        }
        if !template.query.is_empty() {
            builder = builder.query(&template.query);
        }
        if let Some(body) = &template.json_body {
            builder = builder
                .header(CONTENT_TYPE, "application/json")
                .body(body.clone());
        }
        Ok(builder)
    }

    fn transport_error<T>(
        err: reqwest::Error,
        attempts: usize,
        attempt: usize,
        start: Instant,
    ) -> Result<T> {
        if attempts > 1 && attempt == attempts {
            Err(Error::Retry {
                attempts,
                elapsed: start.elapsed(),
                last_error: Some(Box::new(Error::Http(err))),
            })
        } else {
            Err(Error::Http(err))
        }
    }

    pub(crate) fn validate_api_url(&self, url: &Url) -> Result<()> {
        if url.origin() != self.api_origin.origin() {
            return Err(Error::InvalidResponse(format!(
                "API link points to another origin: {:?}",
                url.origin()
            )));
        }
        Ok(())
    }

    pub(crate) fn download_attempt_budget(&self) -> DownloadAttemptBudget {
        DownloadAttemptBudget::new(&self.retry)
    }

    fn retry_delay_for_response(
        &self,
        response: &Response,
        attempt: usize,
        attempts: usize,
        fallback: Duration,
    ) -> Option<Duration> {
        if attempt == attempts || !RetryPolicy::should_retry_status(response.status()) {
            return None;
        }
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(parse_retry_after);
        self.retry.retry_delay(retry_after, fallback)
    }

    async fn send_once(&self, template: &RequestTemplate, attempt: usize) -> Result<Response> {
        let builder = self.build_request(template)?;
        trace!(
            "HTTP {} {} attempt {}",
            template.method,
            log_url(&template.url),
            attempt
        );
        builder
            .send()
            .await
            .map_err(|err| Error::Http(err.without_url()))
    }

    async fn wait_to_retry(
        &self,
        budget: &mut DownloadAttemptBudget,
        requested: Option<Duration>,
    ) -> bool {
        if !budget.can_retry() {
            return false;
        }
        let Some(delay) = self.retry.retry_delay(requested, budget.delay) else {
            return false;
        };
        tokio::time::sleep(delay).await;
        budget.delay = self.retry.next_delay(budget.delay);
        true
    }
    pub(crate) async fn wait_to_resume(&self, budget: &mut DownloadAttemptBudget) {
        let retry_scheduled = self.wait_to_retry(budget, None).await;
        debug_assert!(retry_scheduled);
    }

    async fn handle_response<T>(
        response: Response,
        log_messages: bool,
        parse: fn(StatusCode, Url, &[u8], bool) -> Result<T>,
    ) -> Result<T> {
        let status = response.status();
        let url = response.url().clone();
        let body = response
            .bytes()
            .await
            .map_err(|err| Error::Http(err.without_url()))?;
        parse(status, url, &body, log_messages)
    }

    pub(crate) async fn execute(&self, template: &RequestTemplate) -> Result<JsonResponse> {
        self.execute_with(template, parse_json_response).await
    }
    async fn execute_with<T>(
        &self,
        template: &RequestTemplate,
        parse: fn(StatusCode, Url, &[u8], bool) -> Result<T>,
    ) -> Result<T> {
        let start = Instant::now();
        let attempts = self.attempts_for(template);
        let mut delay = self.retry.base_delay;
        for attempt in 1..=attempts {
            let response = match self.send_once(template, attempt).await {
                Ok(response) => response,
                Err(Error::Http(err)) => {
                    if attempt == attempts || !RetryPolicy::should_retry_error(&err) {
                        return Self::transport_error(err, attempts, attempt, start);
                    }
                    warn!(
                        "retrying request {} {} due to error: {} (attempt {})",
                        template.method,
                        log_url(&template.url),
                        err,
                        attempt
                    );
                    tokio::time::sleep(delay).await;
                    delay = self.retry.next_delay(delay);
                    continue;
                }
                Err(err) => return Err(err),
            };
            let status = response.status();
            if let Some(retry_delay) =
                self.retry_delay_for_response(&response, attempt, attempts, delay)
            {
                warn!(
                    "retrying HTTP {} {} due to status {status} (attempt {attempt})",
                    template.method,
                    log_url(&template.url)
                );
                tokio::time::sleep(retry_delay).await;
                delay = self.retry.next_delay(delay);
                continue;
            }
            match Self::handle_response(response, template.log_messages, parse).await {
                Ok(response) => return Ok(response),
                Err(Error::Http(err))
                    if attempt < attempts && RetryPolicy::should_retry_error(&err) =>
                {
                    warn!(
                        "retrying HTTP {} {} after response body error: {} (attempt {})",
                        template.method,
                        log_url(&template.url),
                        err,
                        attempt
                    );
                    tokio::time::sleep(delay).await;
                    delay = self.retry.next_delay(delay);
                }
                Err(Error::Http(err)) => {
                    return Self::transport_error(err, attempts, attempt, start);
                }
                Err(err) => return Err(err),
            }
        }
        unreachable!("retry policies always contain at least one attempt")
    }
    pub(crate) async fn execute_typed<T: DeserializeOwned>(
        &self,
        template: &RequestTemplate,
    ) -> Result<T> {
        self.execute_with(template, parse_typed_response::<T>).await
    }
    pub(crate) async fn execute_typed_response<T: DeserializeOwned>(
        &self,
        template: &RequestTemplate,
    ) -> Result<TypedResponse<T>> {
        self.execute_with(template, parse_typed_response_with_url::<T>)
            .await
    }
    pub(crate) async fn execute_raw_allowing_with_budget(
        &self,
        template: &RequestTemplate,
        allowed_status: Option<StatusCode>,
        budget: &mut DownloadAttemptBudget,
    ) -> Result<Response> {
        loop {
            let attempt = budget.begin_attempt();
            let response = match self.send_once(template, attempt).await {
                Ok(response) => response,
                Err(Error::Http(err)) => {
                    if RetryPolicy::should_retry_error(&err)
                        && self.wait_to_retry(budget, None).await
                    {
                        warn!(
                            "retrying request {} {} due to error: {} (attempt {})",
                            template.method,
                            log_url(&template.url),
                            err,
                            attempt
                        );
                        continue;
                    }
                    return Self::transport_error(
                        err,
                        self.retry.max_attempts.get(),
                        attempt,
                        budget.started,
                    );
                }
                Err(err) => return Err(err),
            };
            let status = response.status();
            if RetryPolicy::should_retry_status(status) {
                let retry_after = response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(parse_retry_after);
                if self.wait_to_retry(budget, retry_after).await {
                    warn!(
                        "retrying HTTP {} {} due to status {status} (attempt {attempt})",
                        template.method,
                        log_url(&template.url)
                    );
                    continue;
                }
            }
            if status.is_success() || allowed_status == Some(status) {
                return Ok(response);
            }
            let body = response
                .bytes()
                .await
                .map_err(|err| Error::Http(err.without_url()))?;
            return Err(api_error(status, parse_error_body(&body)));
        }
    }
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpClient")
            .field("api_key_configured", &self.api_key.is_some())
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApiMessage {
    level: Level,

    content: String,
}

impl ApiMessage {
    fn new(level: Level, content: impl Into<String>) -> Self {
        Self {
            level,
            content: content.into(),
        }
    }
}

/// Retry configuration for HTTP requests.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Initial delay between retry attempts.
    base_delay: Duration,

    /// Maximum permitted delay between retry attempts.
    max_delay: Duration,
    /// Maximum total number of attempts, including the first request.
    max_attempts: NonZeroUsize,
}

impl RetryPolicy {
    /// Creates a new policy with attempt and delay bounds.
    ///
    /// A response is returned without retrying when its `Retry-After` value
    /// exceeds `max_delay`, because retrying earlier would violate the server's
    /// instruction.
    pub fn new(
        max_attempts: NonZeroUsize,
        base_delay: Duration,
        max_delay: Duration,
    ) -> Result<Self> {
        if base_delay > max_delay {
            return Err(Error::InvalidConfiguration(
                "retry base delay must not exceed the maximum delay".into(),
            ));
        }
        Ok(Self {
            base_delay,
            max_delay,
            max_attempts,
        })
    }

    /// Returns the initial delay between retry attempts.
    pub const fn base_delay(&self) -> Duration {
        self.base_delay
    }

    fn next_delay(&self, delay: Duration) -> Duration {
        delay.saturating_mul(2).min(self.max_delay)
    }

    /// Returns the maximum delay between retry attempts.
    pub const fn max_delay(&self) -> Duration {
        self.max_delay
    }
    /// Returns the maximum total number of attempts, including the first request.
    ///
    /// A resumable file download shares this budget across the initial request,
    /// response retries, and subsequent range requests.
    pub const fn max_attempts(&self) -> NonZeroUsize {
        self.max_attempts
    }

    fn retry_delay(&self, requested: Option<Duration>, fallback: Duration) -> Option<Duration> {
        match requested {
            Some(delay) if delay > self.max_delay => None,
            Some(delay) => Some(delay),
            None => Some(fallback.min(self.max_delay)),
        }
    }

    pub(crate) fn should_retry_error(err: &reqwest::Error) -> bool {
        err.is_timeout() || err.is_connect() || err.is_body() || err.is_decode()
    }
    fn should_retry_status(status: StatusCode) -> bool {
        status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(
            NonZeroUsize::new(10).expect("default attempt count is non-zero"),
            Duration::from_secs(5),
            Duration::from_secs(120),
        )
        .expect("the default retry policy is valid")
    }
}

#[derive(Debug, Clone)]
pub struct JsonResponse {
    url: Url,

    body: Value,
}

impl JsonResponse {
    pub(crate) fn url(&self) -> &Url {
        &self.url
    }

    pub(crate) fn body(&self) -> &Value {
        &self.body
    }

    pub(crate) fn parse<T: DeserializeOwned>(&self) -> Result<T> {
        Ok(T::deserialize(&self.body)?)
    }

    pub(crate) fn into_body(self) -> Value {
        self.body
    }

    pub(crate) fn log_messages(&self) {
        for message in extract_messages(&self.body) {
            log::log!(message.level, "{}", message.content);
        }
    }

    pub(crate) fn link_href_optional(&self, rel: &str) -> Result<Option<Url>> {
        let Some(links) = self.body.get("links").and_then(Value::as_array) else {
            return Ok(None);
        };
        let mut matches = links
            .iter()
            .filter(|link| link.get("rel").and_then(Value::as_str) == Some(rel));
        let Some(first) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            return Err(Error::link(rel.to_string()));
        }
        let href = first
            .get("href")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::link(rel.to_string()))?;
        let resolved = self.url.join(href).or_else(|_| Url::parse(href))?;
        Ok(Some(resolved))
    }
}

pub struct TypedResponse<T> {
    pub(crate) url: Url,

    pub(crate) body: T,
}

#[derive(Clone)]
pub struct RequestTemplate {
    url: Url,

    query: Vec<(String, String)>,

    method: Method,

    headers: HeaderMap,

    json_body: Option<bytes::Bytes>,

    log_messages: bool,

    authenticated: bool,
}

impl RequestTemplate {
    pub(crate) fn new(method: Method, url: Url) -> Self {
        Self {
            method,
            url,
            query: Vec::new(),
            json_body: None,
            headers: HeaderMap::new(),
            log_messages: true,
            authenticated: true,
        }
    }

    pub(crate) fn for_asset(mut self) -> Self {
        self.authenticated = false;
        self
    }

    pub(crate) fn with_json<T: serde::Serialize + ?Sized>(mut self, body: &T) -> Result<Self> {
        self.json_body = Some(bytes::Bytes::from(serde_json::to_vec(body)?));
        Ok(self)
    }
    pub(crate) fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.headers.insert(name, value);
        self
    }
    pub(crate) fn with_query_pair(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.query.push((key.into(), value.into()));
        self
    }
    pub(crate) fn with_log_messages(mut self, enabled: bool) -> Self {
        self.log_messages = enabled;
        self
    }
}

impl std::fmt::Debug for RequestTemplate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestTemplate")
            .field("method", &self.method)
            .field("path", &self.url.path())
            .field("query_count", &self.query.len())
            .field("has_json_body", &self.json_body.is_some())
            .field("header_count", &self.headers.len())
            .field("log_messages", &self.log_messages)
            .field("authenticated", &self.authenticated)
            .finish()
    }
}

#[derive(Deserialize)]
struct DatasetMessageFields {
    messages: Option<Value>,
}

pub struct DownloadAttemptBudget {
    delay: Duration,

    started: Instant,

    attempted: usize,

    remaining: usize,
}

impl DownloadAttemptBudget {
    fn new(policy: &RetryPolicy) -> Self {
        Self {
            attempted: 0,
            remaining: policy.max_attempts.get(),
            delay: policy.base_delay,
            started: Instant::now(),
        }
    }

    pub(super) const fn can_retry(&self) -> bool {
        self.remaining > 0
    }

    fn begin_attempt(&mut self) -> usize {
        debug_assert!(self.remaining > 0);
        self.remaining -= 1;
        self.attempted += 1;
        self.attempted
    }
}

#[derive(Deserialize)]
struct ResponseMessageFields {
    message: Option<Value>,

    messages: Option<Value>,

    metadata: Option<MetadataMessageFields>,
}

impl ResponseMessageFields {
    fn messages(&self) -> Vec<ApiMessage> {
        extract_message_values(
            self.message.as_ref(),
            self.messages.as_ref(),
            self.metadata
                .as_ref()
                .and_then(|metadata| metadata.dataset_metadata.as_ref())
                .and_then(|metadata| metadata.messages.as_ref()),
        )
    }
}

#[derive(Deserialize)]
struct MetadataMessageFields {
    #[serde(rename = "datasetMetadata")]
    dataset_metadata: Option<DatasetMessageFields>,
}

pub trait PagePayload<T>: DeserializeOwned {
    fn into_page(self) -> (Vec<T>, Vec<PageLink>);
}

fn log_url(url: &Url) -> &str {
    url.path()
}

fn tls_config() -> Result<rustls::ClientConfig> {
    TLS_CONFIG
        .get_or_init(|| {
            let mut config =
                rustls::ClientConfig::with_platform_verifier().map_err(|err| err.to_string())?;
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            Ok(config)
        })
        .as_ref()
        .cloned()
        .map_err(|err| {
            Error::InvalidConfiguration(format!(
                "failed to initialize the platform TLS verifier: {err}"
            ))
        })
}

fn api_error(status: StatusCode, body: Value) -> Error {
    let message = extract_error_message(&body).unwrap_or_else(|| {
        status
            .canonical_reason()
            .unwrap_or("unknown error")
            .to_owned()
    });
    if status == StatusCode::UNAUTHORIZED {
        Error::Authentication {
            status,
            message,
            body,
        }
    } else {
        Error::Api {
            status,
            message,
            body,
        }
    }
}

fn api_messages_enabled() -> bool {
    [
        Level::Error,
        Level::Warn,
        Level::Info,
        Level::Debug,
        Level::Trace,
    ]
    .into_iter()
    .any(|level| log::log_enabled!(level))
}

fn extract_messages(value: &Value) -> Vec<ApiMessage> {
    extract_message_values(
        value.get("message"),
        value.get("messages"),
        value.pointer("/metadata/datasetMetadata/messages"),
    )
}

fn extract_error_message(value: &Value) -> Option<String> {
    let keys = ["title", "detail", "message", "error"];
    for key in keys {
        if let Some(text) = value.get(key).and_then(Value::as_str)
            && !text.is_empty()
        {
            return Some(text.to_string());
        }
    }
    None
}

fn extract_message_values(
    message: Option<&Value>,
    structured_messages: Option<&Value>,
    dataset_messages: Option<&Value>,
) -> Vec<ApiMessage> {
    let mut messages = Vec::new();

    if let Some(text) = message.and_then(Value::as_str) {
        let (level, content) = utils::split_prefixed_level(text);
        messages.push(ApiMessage::new(level, content.to_string()));
    }

    if let Some(array) = structured_messages.and_then(Value::as_array) {
        messages.extend(array.iter().filter_map(parse_structured_message));
    }

    if let Some(array) = dataset_messages.and_then(Value::as_array) {
        messages.extend(array.iter().filter_map(parse_structured_message));
    }

    messages
}

fn parse_typed_body<T: DeserializeOwned>(
    status: StatusCode,
    body: &[u8],
    log_messages: bool,
) -> Result<T> {
    let body = successful_json_body(status, body)?;
    if log_messages
        && api_messages_enabled()
        && let Ok(fields) = serde_json::from_slice::<ResponseMessageFields>(body)
    {
        for message in fields.messages() {
            log::log!(message.level, "{}", message.content);
        }
    }
    Ok(serde_json::from_slice(body)?)
}

fn parse_error_body(body: &[u8]) -> Value {
    serde_json::from_slice(body)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned()))
}

fn parse_retry_after(value: &HeaderValue) -> Option<Duration> {
    let value = value.to_str().ok()?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let deadline = DateTime::parse_from_rfc2822(value).ok()?;
    Some(
        (deadline.with_timezone(&Utc) - Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO),
    )
}

fn parse_json_response(
    status: StatusCode,
    url: Url,
    body: &[u8],
    log_messages: bool,
) -> Result<JsonResponse> {
    let response = JsonResponse {
        url,
        body: parse_response_body(status, body)?,
    };
    if log_messages {
        response.log_messages();
    }
    Ok(response)
}

fn parse_response_body(status: StatusCode, body: &[u8]) -> Result<Value> {
    Ok(serde_json::from_slice(successful_json_body(status, body)?)?)
}

fn parse_typed_response<T: DeserializeOwned>(
    status: StatusCode,
    _url: Url,
    body: &[u8],
    log_messages: bool,
) -> Result<T> {
    parse_typed_body(status, body, log_messages)
}

fn parse_structured_message(value: &Value) -> Option<ApiMessage> {
    let content = value.get("content")?.as_str()?;
    let severity = value
        .get("severity")
        .and_then(Value::as_str)
        .unwrap_or("info");
    let level = utils::level_from_severity(severity);
    value.get("date").and_then(Value::as_str).map_or_else(
        || Some(ApiMessage::new(level, content)),
        |date| Some(ApiMessage::new(level, format!("[{date}] {content}"))),
    )
}

fn parse_typed_response_with_url<T: DeserializeOwned>(
    status: StatusCode,
    url: Url,
    body: &[u8],
    log_messages: bool,
) -> Result<TypedResponse<T>> {
    Ok(TypedResponse {
        url,
        body: parse_typed_body(status, body, log_messages)?,
    })
}

fn successful_json_body(status: StatusCode, body: &[u8]) -> Result<&[u8]> {
    if !status.is_success() {
        return Err(api_error(status, parse_error_body(body)));
    }

    if body.iter().all(u8::is_ascii_whitespace) {
        Ok(b"null")
    } else {
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn retries_and_errors_after_max_attempts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = [0u8; 1024];
                let _ = stream.read(&mut buffer).await.unwrap();
                stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            }
        });

        let client = HttpClient::new(
            Url::parse(&format!("http://{addr}/")).unwrap(),
            None,
            Duration::from_secs(5),
            Duration::from_secs(5),
            true,
            RetryPolicy::new(
                NonZeroUsize::new(3).unwrap(),
                Duration::from_millis(10),
                Duration::from_millis(50),
            )
            .unwrap(),
        )
        .unwrap();

        let url = Url::parse(&format!("http://{addr}/data")).unwrap();
        let template = RequestTemplate::new(Method::GET, url);
        match client.execute(&template).await {
            Err(Error::Api { status, .. }) => assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE),
            other => panic!("expected retry error, got {other:?}"),
        }
        server.await.unwrap();
    }

    #[test]
    fn handles_empty_success_body() {
        let body = parse_response_body(StatusCode::NO_CONTENT, b"").unwrap();
        assert!(body.is_null());
    }

    #[test]
    fn distinguishes_authentication_from_other_api_errors() {
        let denied = parse_response_body(StatusCode::UNAUTHORIZED, b"{\"detail\":\"bad token\"}")
            .unwrap_err();
        assert!(matches!(denied, Error::Authentication { .. }));
        let forbidden =
            parse_response_body(StatusCode::FORBIDDEN, b"{\"detail\":\"licence required\"}")
                .unwrap_err();
        assert!(matches!(forbidden, Error::Api { .. }));
    }

    #[test]
    fn preserves_non_json_error_body() {
        let error =
            parse_response_body(StatusCode::BAD_GATEWAY, b"upstream unavailable").unwrap_err();
        assert!(matches!(
            error,
            Error::Api { body: Value::String(body), status, .. }
                if status == StatusCode::BAD_GATEWAY && body == "upstream unavailable"
        ));
    }

    #[test]
    fn request_debug_hides_url_query() {
        let template = RequestTemplate::new(
            Method::GET,
            Url::parse("https://example.test/data?signature=top-secret").unwrap(),
        );
        assert!(!format!("{template:?}").contains("top-secret"));
    }

    #[test]
    fn extracts_error_from_sanitized_live_fixture() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/cds-contract.json")).unwrap();
        assert_eq!(
            extract_error_message(&fixture["error"]).as_deref(),
            Some("Resource fixture-does-not-exist not found")
        );
    }

    #[test]
    fn typed_message_projection_preserves_api_messages() {
        let body = br#"{
            "message": "[warning] root",
            "messages": [{"severity": "error", "content": "structured"}],
            "metadata": {"datasetMetadata": {"messages": [
                {"severity": "info", "content": "nested", "date": "today"}
            ]}},
            "largeUnrelatedField": [1, 2, 3]
        }"#;
        let raw: Value = serde_json::from_slice(body).unwrap();
        let fields: ResponseMessageFields = serde_json::from_slice(body).unwrap();
        assert_eq!(fields.messages(), extract_messages(&raw));
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        assert_eq!(
            parse_retry_after(&HeaderValue::from_static("120")),
            Some(Duration::from_secs(120))
        );
        let future = (Utc::now() + chrono::Duration::days(1))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        assert!(
            parse_retry_after(&HeaderValue::from_str(&future).unwrap())
                .is_some_and(|delay| delay > Duration::ZERO)
        );
        let past = (Utc::now() - chrono::Duration::days(1))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        assert_eq!(
            parse_retry_after(&HeaderValue::from_str(&past).unwrap()),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn retry_policy_rejects_invalid_delay_bounds() {
        assert!(matches!(
            RetryPolicy::new(
                NonZeroUsize::new(2).unwrap(),
                Duration::from_secs(2),
                Duration::from_secs(1),
            ),
            Err(Error::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn default_retry_policy_caps_server_delays() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_attempts().get(), 10);
        assert_eq!(
            policy.retry_delay(Some(Duration::from_secs(3_600)), Duration::ZERO),
            None
        );
    }
}
