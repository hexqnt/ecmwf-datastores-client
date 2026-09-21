use std::{
    num::{NonZeroU32, NonZeroUsize},
    sync::{Arc, Mutex},
    time::Duration,
};

use ecmwf_datastores_client::{
    ApiKey, Asset, Client, CollectionId, CollectionsRequest, Credentials, ExistingTarget, Job,
    JobId, JobResults, JobSort, JobStatus, JobsRequest, ProcessListRequest, ProcessSort,
    RetryPolicy, Selection, error::Error,
};
use futures_util::StreamExt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

const JOB_PATH: &str = "/api/retrieve/v1/jobs/job-1";
const RESULTS_PATH: &str = "/api/retrieve/v1/jobs/job-1/results";
const ASSET_PATH: &str = "/asset";
const SUCCESSFUL_JOB: &str = r#"{"processID":"dataset-1","jobID":"job-1","status":"successful"}"#;
const ASSET_RESULTS: &str = r#"{"asset":{"value":{"href":"/asset"}}}"#;

fn job_id(value: &str) -> JobId {
    JobId::parse(value).unwrap()
}

fn retry_policy(max_attempts: usize) -> RetryPolicy {
    RetryPolicy::new(
        NonZeroUsize::new(max_attempts).unwrap(),
        Duration::from_millis(1),
        Duration::from_millis(2),
    )
    .unwrap()
}

fn collection_id(value: &str) -> CollectionId {
    CollectionId::parse(value).unwrap()
}

fn asset_responses(
    results: &'static str,
    bodies: impl IntoIterator<Item = &'static str>,
) -> Vec<(&'static str, &'static str, &'static str)> {
    let mut responses = vec![
        ("GET", JOB_PATH, SUCCESSFUL_JOB),
        ("GET", RESULTS_PATH, results),
    ];
    responses.extend(bodies.into_iter().map(|body| ("GET", ASSET_PATH, body)));
    responses
}

#[test]
fn rejects_zero_wait_timeout() {
    let url = Url::parse("http://localhost/api").unwrap();
    assert!(matches!(
        Client::builder(url).wait_timeout(Duration::ZERO).build(),
        Err(Error::InvalidConfiguration(_))
    ));
}
#[test]
fn unknown_status_is_preserved() {
    let status: JobStatus = serde_json::from_str("\"paused\"").unwrap();
    assert_eq!(status, JobStatus::Unknown("paused".into()));
}

#[test]
fn debug_output_redacts_credentials() {
    let url = Url::parse("http://user:password@localhost/api/").unwrap();
    let key = ApiKey::parse("top-secret").unwrap();
    let builder = Client::builder(url.clone()).api_key(key.clone());
    let credentials = Credentials::new(url, Some(key));
    let client = builder.clone().build().unwrap();
    let job = client.job(&job_id("job-1")).unwrap();
    for output in [
        format!("{builder:?}"),
        format!("{credentials:?}"),
        format!("{job:?}"),
    ] {
        assert!(!output.contains("top-secret"));
        assert!(!output.contains("password"));
    }
}
#[test]
fn public_handles_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Client>();
    assert_send_sync::<Job>();
    assert_send_sync::<Asset>();
}

#[allow(clippy::too_many_lines)] // The table keeps the wire-level test responses in one place.
async fn serve(
    responses: Vec<(&'static str, &'static str, &'static str)>,
) -> (Url, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/api/", listener.local_addr().unwrap())).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let handle = tokio::spawn(async move {
        for (method, path, body) in responses {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                request.extend_from_slice(&chunk[..n]);
            }
            let header = String::from_utf8(request).unwrap();
            assert!(header.starts_with(&format!("{method} {path} ")), "{header}");
            captured.lock().unwrap().push(header);
            if body == "DROP" {
                continue;
            }
            if body == "HANG" {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nDATA",
                    )
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_secs(60)).await;
                continue;
            }
            let (status, content, extra_headers) = match body {
                "ERROR" => (
                    "500 Internal Server Error",
                    "{\"title\":\"unavailable\"}",
                    "",
                ),
                "PENDING" => ("409 Conflict", "{\"title\":\"not ready\"}", ""),
                "RATE_LIMIT" => (
                    "429 Too Many Requests",
                    "{\"title\":\"slow down\"}",
                    "Retry-After: 0\r\n",
                ),
                "RATE_LIMIT_LONG" => (
                    "429 Too Many Requests",
                    "{\"title\":\"slow down\"}",
                    "Retry-After: 3600\r\n",
                ),
                "REDIRECT" => ("302 Found", "", "Location: /api/retrieve/v1/jobs/job-1\r\n"),
                "TRUNCATED" => ("200 OK", "DATA", ""),
                "TRUNCATED_ETAG" => (
                    "200 OK",
                    "0123",
                    "Accept-Ranges: bytes\r\nETag: \"asset-v1\"\r\n",
                ),
                "TRUNCATED_NO_VALIDATOR" => ("200 OK", "0123", ""),
                "TRUNCATED_AT_END" => ("200 OK", "0123456789", "ETag: \"asset-v1\"\r\n"),
                "PARTIAL" => (
                    "206 Partial Content",
                    "456789",
                    "Content-Range: bytes 4-9/10\r\nETag: \"asset-v1\"\r\n",
                ),
                "BAD_PARTIAL" => (
                    "206 Partial Content",
                    "456789",
                    "Content-Range: bytes 5-10/11\r\n",
                ),
                "PARTIAL_CHANGED" => (
                    "206 Partial Content",
                    "456789",
                    "Content-Range: bytes 4-9/10\r\nETag: \"asset-v2\"\r\n",
                ),
                "PARTIAL_NO_VALIDATOR" => (
                    "206 Partial Content",
                    "456789",
                    "Content-Range: bytes 4-9/10\r\n",
                ),
                "SHORT_RANGE" => (
                    "206 Partial Content",
                    "45678",
                    "Content-Range: bytes 4-9/10\r\nETag: \"asset-v1\"\r\n",
                ),
                "OVERRUN_TRUNCATED_RANGE" => (
                    "206 Partial Content",
                    "456789",
                    "Content-Range: bytes 4-5/10\r\nETag: \"asset-v1\"\r\n",
                ),
                "UNSATISFIED_10" => (
                    "416 Range Not Satisfiable",
                    "",
                    "Content-Range: bytes */10\r\n",
                ),
                "FULL_10" => ("200 OK", "0123456789", ""),
                "FULL_CHANGED_10" => ("200 OK", "ABCDEFGHIJ", ""),
                "FULL_CHANGED_12" => ("200 OK", "ABCDEFGHIJKL", ""),
                "ENCODED" => ("200 OK", "not-really-gzip", "Content-Encoding: gzip\r\n"),
                "ENCODED_ZSTD" => ("200 OK", "not-really-zstd", "Content-Encoding: zstd\r\n"),
                _ => ("200 OK", body, ""),
            };
            let content_length = match body {
                "TRUNCATED" => 100,
                "TRUNCATED_ETAG" | "TRUNCATED_NO_VALIDATOR" => 10,
                "TRUNCATED_AT_END" => 11,
                "OVERRUN_TRUNCATED_RANGE" => 7,
                _ => content.len(),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {content_length}\r\nConnection: close\r\nContent-Type: application/json\r\n{extra_headers}\r\n{content}"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (url, seen, handle)
}

async fn serve_resumable_asset(
    first_response: &'static str,
    second_response: &'static str,
) -> (Url, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    serve(asset_responses(
        r#"{"asset":{"value":{"href":"/asset","file:size":10}}}"#,
        [first_response, second_response],
    ))
    .await
}

async fn resumable_results(url: Url) -> JobResults {
    let client = Client::builder(url)
        .retry_policy(retry_policy(2))
        .build()
        .unwrap();
    client.job_results_once(&job_id("job-1")).await.unwrap()
}

#[tokio::test]
async fn invalid_content_range_is_rejected() {
    let (url, _, server) = serve_resumable_asset("TRUNCATED_ETAG", "BAD_PARTIAL").await;
    let results = resumable_results(url).await;
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("data.bin");

    let error = results
        .download_to(&target, ExistingTarget::Error)
        .await
        .unwrap_err();

    assert!(matches!(error, Error::Download(message) if message.contains("starts at byte 5")));
    assert!(!target.exists());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    server.await.unwrap();
}

#[tokio::test]
async fn does_not_retry_before_retry_after() {
    let (url, seen, server) = serve(vec![(
        "GET",
        "/api/retrieve/v1/jobs/job-1",
        "RATE_LIMIT_LONG",
    )])
    .await;
    let client = Client::builder(url)
        .retry_policy(retry_policy(2))
        .build()
        .unwrap();

    assert!(matches!(
        client
            .fetch_job(&job_id("job-1"))
            .await,
        Err(Error::Api { status, .. }) if status == reqwest::StatusCode::TOO_MANY_REQUESTS
    ));
    server.await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn asset_can_be_downloaded_into_memory() {
    let (url, _, server) = serve(asset_responses(
        r#"{"asset":{"value":{"href":"/asset","file:size":4}}}"#,
        ["DATA"],
    ))
    .await;
    let client = Client::builder(url).build().unwrap();
    let results = client.job_results_once(&job_id("job-1")).await.unwrap();

    let bytes = results.asset().bytes().await.unwrap();

    assert_eq!(bytes, b"DATA"[..]);
    server.await.unwrap();
}

#[tokio::test]
async fn status_accepts_minimal_job_response() {
    let (url, _, server) = serve(vec![(
        "GET",
        "/api/retrieve/v1/jobs/job-1",
        "{\"status\":\"running\"}",
    )])
    .await;
    let client = Client::builder(url).build().unwrap();
    let job = client.job(&job_id("job-1")).unwrap();
    assert_eq!(job.status().await.unwrap(), JobStatus::Running);
    server.await.unwrap();
}

#[tokio::test]
async fn partial_body_must_match_content_range() {
    let (url, _, server) = serve_resumable_asset("TRUNCATED_ETAG", "SHORT_RANGE").await;
    let results = resumable_results(url).await;
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("data.bin");

    let error = results
        .download_to(&target, ExistingTarget::Error)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, Error::Download(message) if message.contains("partial response ended")),
        "{error:?}"
    );
    assert!(!target.exists());
    server.await.unwrap();
}

#[tokio::test]
async fn unsolicited_partial_assets_are_rejected() {
    let (url, _, server) = serve(asset_responses(
        ASSET_RESULTS,
        ["PARTIAL", "PARTIAL", "PARTIAL"],
    ))
    .await;
    let client = Client::new(url, None).unwrap();
    let results = client.job_results_once(&job_id("job-1")).await.unwrap();

    assert!(matches!(
        results.asset().bytes().await,
        Err(Error::Download(_))
    ));
    assert!(matches!(
        results.asset().byte_stream().await,
        Err(Error::Download(_))
    ));
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("data.bin");
    assert!(matches!(
        results.download_to(&target, ExistingTarget::Error).await,
        Err(Error::Download(_))
    ));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    server.await.unwrap();
}

#[tokio::test]
async fn automatic_pagination_rejects_link_cycles() {
    let repeated = "{\"collections\":[{\"id\":\"dataset-1\"}],\"links\":[{\"rel\":\"next\",\"href\":\"?page=1\"}]}";
    let (url, seen, server) = serve(vec![
        ("GET", "/api/catalogue/v1/datasets", repeated),
        ("GET", "/api/catalogue/v1/datasets?page=1", repeated),
    ])
    .await;
    let client = Client::builder(url).build().unwrap();
    let page = client
        .collections(&CollectionsRequest::default())
        .await
        .unwrap();
    assert!(
        matches!(page.collect_all().await, Err(Error::InvalidResponse(message)) if message.contains("cycle"))
    );
    server.await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn in_memory_download_checks_expected_size() {
    let (url, _, server) = serve(asset_responses(
        r#"{"asset":{"value":{"href":"/asset","file:size":5}}}"#,
        ["DATA"],
    ))
    .await;
    let client = Client::builder(url).build().unwrap();
    let results = client.job_results_once(&job_id("job-1")).await.unwrap();

    let error = results.asset().bytes().await.unwrap_err();

    assert!(
        matches!(error, Error::Download(message) if message.contains("downloaded 4 bytes but expected 5"))
    );
    server.await.unwrap();
}

#[tokio::test]
async fn in_memory_download_retries_interrupted_body() {
    let (url, seen, server) = serve(asset_responses(
        r#"{"asset":{"value":{"href":"/asset","file:size":4}}}"#,
        ["TRUNCATED", "DATA"],
    ))
    .await;
    let client = Client::builder(url)
        .retry_policy(retry_policy(2))
        .build()
        .unwrap();
    let results = client.job_results_once(&job_id("job-1")).await.unwrap();

    assert_eq!(results.asset().bytes().await.unwrap(), b"DATA"[..]);
    server.await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn full_restart_uses_the_new_response_length() {
    let (url, _, server) = serve(asset_responses(
        ASSET_RESULTS,
        ["TRUNCATED_ETAG", "FULL_CHANGED_12"],
    ))
    .await;
    let results = resumable_results(url).await;
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("data.bin");

    let mut updates = Vec::new();
    results
        .download_to_with_progress(&target, ExistingTarget::Error, |progress| {
            updates.push(progress.downloaded());
        })
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(&target).await.unwrap(), b"ABCDEFGHIJKL");
    assert!(
        updates
            .windows(2)
            .any(|positions| positions[0] > 0 && positions[1] == 0)
    );
    assert_eq!(updates.last(), Some(&12));
    server.await.unwrap();
}

#[tokio::test]
async fn truncated_asset_preserves_existing_target() {
    let (url, _, server) = serve(asset_responses(ASSET_RESULTS, ["TRUNCATED"])).await;
    let client = Client::builder(url)
        .retry_policy(retry_policy(1))
        .build()
        .unwrap();
    let results = client.job_results_once(&job_id("job-1")).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("existing.bin");
    tokio::fs::write(&target, b"OLD").await.unwrap();
    assert!(
        results
            .download_to(&target, ExistingTarget::Replace)
            .await
            .is_err()
    );
    assert_eq!(tokio::fs::read(&target).await.unwrap(), b"OLD");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    server.await.unwrap();
}

#[tokio::test]
async fn missing_validator_restarts_temporary_file() {
    let (url, seen, server) =
        serve_resumable_asset("TRUNCATED_NO_VALIDATOR", "FULL_CHANGED_10").await;
    let results = resumable_results(url).await;
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("data.bin");

    results
        .download_to(&target, ExistingTarget::Error)
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(&target).await.unwrap(), b"ABCDEFGHIJ");
    let resumed = seen.lock().unwrap()[3].to_ascii_lowercase();
    assert!(!resumed.contains("range:"));
    assert!(!resumed.contains("if-range:"));
    server.await.unwrap();
}

#[tokio::test]
async fn raw_json_does_not_require_known_job_fields() {
    let (url, _, server) = serve(vec![(
        "GET",
        "/api/retrieve/v1/jobs/job-1?request=true",
        "{\"futureField\":\"value\"}",
    )])
    .await;
    let client = Client::builder(url).build().unwrap();
    let job = client.job(&job_id("job-1")).unwrap();
    assert_eq!(job.raw_json().await.unwrap()["futureField"], "value");
    server.await.unwrap();
}

#[tokio::test]
async fn cancelling_download_removes_temporary_file() {
    let (url, seen, server) = serve(asset_responses(ASSET_RESULTS, ["HANG"])).await;
    let client = Client::builder(url).build().unwrap();
    let results = client.job_results_once(&job_id("job-1")).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("existing.bin");
    tokio::fs::write(&target, b"OLD").await.unwrap();
    let download_target = target.clone();
    let download = tokio::spawn(async move {
        results
            .download_to(download_target, ExistingTarget::Replace)
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while seen.lock().unwrap().len() < 3 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    download.abort();
    assert!(download.await.unwrap_err().is_cancelled());
    server.abort();
    assert_eq!(tokio::fs::read(&target).await.unwrap(), b"OLD");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn first_poll_delay_respects_configured_maximum() {
    let (url, _, server) = serve(vec![
        (
            "GET",
            "/api/retrieve/v1/jobs/job-1?log=true",
            r#"{"jobID":"job-1","processID":"dataset-1","status":"running"}"#,
        ),
        (
            "GET",
            "/api/retrieve/v1/jobs/job-1?log=true",
            r#"{"jobID":"job-1","processID":"dataset-1","status":"successful"}"#,
        ),
    ])
    .await;
    let client = Client::builder(url)
        .poll_max_delay(Duration::from_millis(1))
        .wait_timeout(Duration::from_millis(500))
        .build()
        .unwrap();
    let mut job = client.job(&job_id("job-1")).unwrap();

    assert_eq!(job.wait().await.unwrap().status, JobStatus::Successful);
    server.await.unwrap();
}

#[tokio::test]
async fn results_api_can_be_called_without_waiting() {
    let (url, _, server) = serve(vec![
        (
            "GET",
            "/api/retrieve/v1/jobs/job-1",
            "{\"links\":[{\"rel\":\"results\",\"href\":\"/api/retrieve/v1/jobs/job-1/results\"}]}",
        ),
        ("GET", "/api/retrieve/v1/jobs/job-1/results", "PENDING"),
    ])
    .await;
    let client = Client::builder(url).build().unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        client.job_results_once(&job_id("job-1")),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(matches!(error, Error::Api { status, .. } if status == reqwest::StatusCode::CONFLICT));
    server.await.unwrap();
}

#[tokio::test]
async fn results_ready_handles_pending_and_terminal_jobs() {
    let (url, _, server) = serve(vec![
        (
            "GET",
            "/api/retrieve/v1/jobs/job-0",
            "{\"processID\":\"dataset-1\",\"jobID\":\"job-0\",\"status\":\"accepted\"}",
        ),
        (
            "GET",
            "/api/retrieve/v1/jobs/job-1",
            "{\"processID\":\"dataset-1\",\"jobID\":\"job-1\",\"status\":\"failed\"}",
        ),
        ("GET", "/api/retrieve/v1/jobs/job-1/results", "PENDING"),
        (
            "GET",
            "/api/retrieve/v1/jobs/job-2",
            "{\"processID\":\"dataset-1\",\"jobID\":\"job-2\",\"status\":\"dismissed\"}",
        ),
        (
            "GET",
            "/api/retrieve/v1/jobs/job-3",
            "{\"processID\":\"dataset-1\",\"jobID\":\"job-3\",\"status\":\"rejected\"}",
        ),
        ("GET", "/api/retrieve/v1/jobs/job-3/results", "PENDING"),
        (
            "GET",
            "/api/retrieve/v1/jobs/job-4",
            "{\"processID\":\"dataset-1\",\"jobID\":\"job-4\",\"status\":\"deleted\"}",
        ),
    ])
    .await;
    let client = Client::builder(url).build().unwrap();
    let pending = client.job(&job_id("job-0")).unwrap();
    assert!(!pending.results_ready().await.unwrap());
    let failed = client.job(&job_id("job-1")).unwrap();
    assert!(
        matches!(failed.results_ready().await, Err(Error::JobFailed { status: JobStatus::Failed, message, .. }) if message == "not ready")
    );
    let dismissed = client.job(&job_id("job-2")).unwrap();
    assert!(matches!(
        dismissed.results_ready().await,
        Err(Error::JobFailed {
            status: JobStatus::Dismissed,
            ..
        })
    ));
    let rejected = client.job(&job_id("job-3")).unwrap();
    assert!(matches!(
        rejected.results_ready().await,
        Err(Error::JobFailed {
            status: JobStatus::Rejected,
            ..
        })
    ));
    let deleted = client.job(&job_id("job-4")).unwrap();
    assert!(matches!(
        deleted.results_ready().await,
        Err(Error::JobFailed {
            status: JobStatus::Deleted,
            ..
        })
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn failed_download_preserves_existing_file() {
    let (url, _, server) = serve(vec![
        (
            "GET",
            "/api/retrieve/v1/jobs/job-1?log=true",
            "{\"processID\":\"dataset-1\",\"jobID\":\"job-1\",\"status\":\"successful\"}",
        ),
        (
            "GET",
            "/api/retrieve/v1/jobs/job-1/results",
            "{\"asset\":{\"value\":{\"href\":\"/asset\",\"file:size\":5}}}",
        ),
        ("GET", "/asset", "DATA"),
        ("GET", "/asset", "DATA"),
    ])
    .await;
    let client = Client::builder(url)
        .retry_policy(retry_policy(2))
        .build()
        .unwrap();
    let mut job = client.job(&job_id("job-1")).unwrap();
    let results = job.wait_for_results().await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("existing.bin");
    tokio::fs::write(&target, b"OLD").await.unwrap();
    assert!(matches!(
        results.download_to(&target, ExistingTarget::Replace).await,
        Err(Error::Download(_))
    ));
    assert_eq!(tokio::fs::read(&target).await.unwrap(), b"OLD");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    server.await.unwrap();
}

#[tokio::test]
async fn failed_job_uses_advertised_results_link_for_error() {
    let (url, _, server) = serve(vec![
        (
            "GET",
            "/api/retrieve/v1/jobs/job-1?log=true",
            "{\"processID\":\"dataset-1\",\"jobID\":\"job-1\",\"status\":\"failed\",\"links\":[{\"rel\":\"results\",\"href\":\"/custom-results\"}]}",
        ),
        ("GET", "/custom-results", "PENDING"),
    ]).await;
    let client = Client::builder(url).build().unwrap();
    let mut job = client.job(&job_id("job-1")).unwrap();
    let error = job.wait().await.unwrap_err();
    assert!(matches!(error, Error::JobFailed { message, .. } if message == "not ready"));
    server.await.unwrap();
}

#[tokio::test]
async fn waiting_returns_resumable_job_id_at_deadline() {
    let (url, _, server) = serve(vec![(
        "GET",
        "/api/retrieve/v1/jobs/job-1?log=true",
        "HANG",
    )])
    .await;
    let client = Client::builder(url)
        .wait_timeout(Duration::from_millis(20))
        .build()
        .unwrap();
    let mut job = client.job(&job_id("job-1")).unwrap();
    assert!(
        matches!(job.wait().await, Err(Error::WaitTimeout { job_id, .. }) if job_id.as_str() == "job-1")
    );
    server.abort();
}
#[tokio::test]
async fn unexpected_asset_content_encoding_is_rejected() {
    let (url, seen, server) = serve(asset_responses(ASSET_RESULTS, ["ENCODED"])).await;
    let client = Client::builder(url).build().unwrap();
    let results = client.job_results_once(&job_id("job-1")).await.unwrap();

    let error = results.asset().bytes().await.unwrap_err();

    assert!(matches!(error, Error::Download(message) if message.contains("Accept-Encoding")));
    assert!(
        seen.lock().unwrap()[2]
            .to_ascii_lowercase()
            .contains("accept-encoding: identity")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn job_details_preserve_request_and_updated_time() {
    let running = "{\"processID\":\"dataset-1\",\"jobID\":\"job-1\",\"status\":\"running\",\"updated\":\"2026-09-16T12:00:00Z\",\"metadata\":{\"request\":{\"ids\":{\"year\":[\"2023\"]}}}}";
    let (url, _, server) = serve(vec![
        ("GET", "/api/retrieve/v1/jobs/job-1?request=true", running),
        ("GET", "/api/retrieve/v1/jobs/job-1?request=true", running),
        (
            "GET",
            "/api/retrieve/v1/jobs/job-1",
            "{\"processID\":\"dataset-1\",\"jobID\":\"job-1\",\"status\":\"successful\"}",
        ),
    ])
    .await;
    let client = Client::builder(url).build().unwrap();
    let job = client.job(&job_id("job-1")).unwrap();
    let details = job.details().await.unwrap();
    assert_eq!(
        details.updated_at.unwrap().to_rfc3339(),
        "2026-09-16T12:00:00+00:00"
    );
    assert_eq!(
        details.request().unwrap().as_map()["year"],
        serde_json::json!(["2023"])
    );
    assert_eq!(job.raw_json().await.unwrap()["status"], "running");
    assert!(job.results_ready().await.unwrap());
    server.await.unwrap();
}

#[tokio::test]
async fn retries_get_when_reading_json_body_fails() {
    let (url, seen, server) = serve(vec![
        ("GET", "/api/retrieve/v1/jobs/job-1", "TRUNCATED"),
        (
            "GET",
            "/api/retrieve/v1/jobs/job-1",
            "{\"processID\":\"dataset-1\",\"jobID\":\"job-1\",\"status\":\"successful\"}",
        ),
    ])
    .await;
    let client = Client::builder(url)
        .retry_policy(retry_policy(2))
        .build()
        .unwrap();

    assert_eq!(
        client.fetch_job(&job_id("job-1")).await.unwrap().status,
        JobStatus::Successful
    );
    server.await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn retries_rate_limits_but_does_not_follow_api_redirects() {
    let (url, seen, server) = serve(vec![
        ("GET", "/api/retrieve/v1/jobs/job-1", "RATE_LIMIT"),
        (
            "GET",
            "/api/retrieve/v1/jobs/job-1",
            "{\"processID\":\"dataset-1\",\"jobID\":\"job-1\",\"status\":\"successful\"}",
        ),
        ("GET", "/api/retrieve/v1/jobs/job-1", "REDIRECT"),
    ])
    .await;
    let client = Client::builder(url)
        .retry_policy(retry_policy(2))
        .build()
        .unwrap();
    let job_id = job_id("job-1");
    assert_eq!(
        client.fetch_job(&job_id).await.unwrap().status,
        JobStatus::Successful
    );
    assert!(matches!(
        client.fetch_job(&job_id).await,
        Err(Error::Api { status, .. }) if status == reqwest::StatusCode::FOUND
    ));
    server.await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn submission_is_not_retried_after_server_error() {
    let (url, seen, server) = serve(vec![(
        "POST",
        "/api/retrieve/v1/processes/dataset-1/execution",
        "ERROR",
    )])
    .await;
    let client = Client::builder(url)
        .retry_policy(retry_policy(3))
        .build()
        .unwrap();
    let selection = Selection::new();
    let error = client
        .submit(&collection_id("dataset-1"), &selection)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::SubmissionUnknown(_)));
    server.await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn submission_recovers_job_id_without_monitor_link() {
    let (url, seen, server) = serve(vec![(
        "POST",
        "/api/retrieve/v1/processes/dataset-1/execution",
        "{\"jobID\":\"job-1\"}",
    )])
    .await;
    let client = Client::builder(url).build().unwrap();
    let job = client
        .submit(&collection_id("dataset-1"), &Selection::new())
        .await
        .unwrap();
    assert_eq!(job.id().as_str(), "job-1");
    server.await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn submission_accepts_monitor_url_with_trailing_slash() {
    let (url, seen, server) = serve(vec![
        (
            "POST",
            "/api/retrieve/v1/processes/dataset-1/execution",
            r#"{"links":[{"rel":"monitor","href":"/api/retrieve/v1/jobs/job-1/"}]}"#,
        ),
        (
            "GET",
            "/api/retrieve/v1/jobs/job-1",
            r#"{"status":"running"}"#,
        ),
    ])
    .await;
    let client = Client::builder(url).build().unwrap();
    let job = client
        .submit(&collection_id("dataset-1"), &Selection::new())
        .await
        .unwrap();

    assert_eq!(job.id().as_str(), "job-1");
    assert_eq!(job.status().await.unwrap(), JobStatus::Running);
    server.await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn byte_stream_returns_asset_data_without_api_token() {
    let (url, seen, server) = serve(asset_responses(ASSET_RESULTS, ["DATA"])).await;
    let client = Client::builder(url)
        .api_key(ApiKey::parse("secret").unwrap())
        .build()
        .unwrap();
    let results = client.job_results_once(&job_id("job-1")).await.unwrap();
    let mut stream = results.asset().byte_stream().await.unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap().as_ref(), b"DATA");
    assert!(stream.next().await.is_none());
    server.await.unwrap();
    assert!(
        !seen.lock().unwrap()[2]
            .to_ascii_lowercase()
            .contains("private-token")
    );
}

#[tokio::test]
async fn rejects_cross_origin_pagination_links() {
    let (url, seen, server) = serve(vec![(
        "GET",
        "/api/catalogue/v1/datasets",
        "{\"collections\":[{\"id\":\"dataset-1\"}],\"links\":[{\"rel\":\"next\",\"href\":\"https://example.invalid/api/catalogue/v1/datasets\"}]}",
    )])
    .await;
    let client = Client::builder(url)
        .api_key(ApiKey::parse("secret").unwrap())
        .build()
        .unwrap();
    let page = client
        .collections(&CollectionsRequest::default())
        .await
        .unwrap();
    assert!(matches!(page.next().await, Err(Error::InvalidResponse(_))));
    server.await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn rejects_partial_response_when_validator_changes_or_is_missing() {
    for (response, expected_message) in [
        ("PARTIAL_NO_VALIDATOR", "validator changed or is missing"),
        ("PARTIAL_CHANGED", "validator changed"),
    ] {
        let (url, _, server) = serve_resumable_asset("TRUNCATED_ETAG", response).await;
        let results = resumable_results(url).await;
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("data.bin");

        let error = results
            .download_to(&target, ExistingTarget::Error)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, Error::Download(message) if message.contains(expected_message)),
            "unexpected error for {response}: {error:?}"
        );
        assert!(!target.exists(), "target created for {response}");
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            0,
            "temporary file retained for {response}"
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn zstd_enabled_by_a_consumer_does_not_decode_assets() {
    let (url, _, server) = serve(asset_responses(ASSET_RESULTS, ["ENCODED_ZSTD"])).await;
    let client = Client::builder(url).build().unwrap();
    let results = client.job_results_once(&job_id("job-1")).await.unwrap();

    let result = results.asset().bytes().await;
    assert!(
        matches!(&result, Err(Error::Download(message)) if message.contains("zstd")),
        "unexpected asset result: {result:?}"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn lost_submission_response_reports_uncertain_outcome() {
    let (url, seen, server) = serve(vec![(
        "POST",
        "/api/retrieve/v1/processes/dataset-1/execution",
        "DROP",
    )])
    .await;
    let client = Client::builder(url).build().unwrap();
    let error = client
        .submit(&collection_id("dataset-1"), &Selection::new())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::SubmissionUnknown(_)));
    server.await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn collection_pages_support_next_prev_and_collect_all() {
    let first = "{\"collections\":[{\"id\":\"dataset-1\"}],\"links\":[{\"rel\":\"next\",\"href\":\"?page=2\"}]}";
    let second = "{\"collections\":[{\"id\":\"dataset-2\"}],\"numberMatched\":2,\"numberReturned\":1,\"links\":[{\"rel\":\"prev\",\"href\":\"?page=1\"}]}";
    let (url, _, server) = serve(vec![
        ("GET", "/api/catalogue/v1/datasets", first),
        ("GET", "/api/catalogue/v1/datasets?page=2", second),
        ("GET", "/api/catalogue/v1/datasets?page=1", first),
        ("GET", "/api/catalogue/v1/datasets?page=2", second),
    ])
    .await;
    let client = Client::builder(url).build().unwrap();
    let page = client
        .collections(&CollectionsRequest::default())
        .await
        .unwrap();
    let next = page.next().await.unwrap().unwrap();
    assert_eq!(next.collections()[0].id.as_str(), "dataset-2");
    assert_eq!(next.number_matched(), Some(2));
    assert_eq!(next.number_returned(), Some(1));
    assert_eq!(
        next.prev().await.unwrap().unwrap().collections()[0]
            .id
            .as_str(),
        "dataset-1"
    );
    let all = page.collect_all().await.unwrap();
    assert_eq!(
        all.iter()
            .map(|collection| collection.id.as_str())
            .collect::<Vec<_>>(),
        ["dataset-1", "dataset-2"]
    );
    server.await.unwrap();
}

#[tokio::test]
async fn processing_pages_encode_sort_and_follow_typed_links() {
    let jobs_first = "{\"jobs\":[{\"jobID\":\"job-1\",\"processID\":\"dataset-1\",\"status\":\"running\"}],\"links\":[{\"rel\":\"next\",\"href\":\"?page=2\"}]}";
    let jobs_second =
        "{\"jobs\":[{\"jobID\":\"job-2\",\"processID\":\"dataset-1\",\"status\":\"successful\"}]}";
    let processes_first = "{\"processes\":[{\"id\":\"dataset-1\"}],\"links\":[{\"rel\":\"next\",\"href\":\"?page=2\"}]}";
    let processes_second = "{\"processes\":[{\"id\":\"dataset-2\"}]}";
    let (url, _, server) = serve(vec![
        (
            "GET",
            "/api/retrieve/v1/jobs?limit=25&sortby=-created",
            jobs_first,
        ),
        ("GET", "/api/retrieve/v1/jobs?page=2", jobs_second),
        (
            "GET",
            "/api/retrieve/v1/processes?limit=10&sortby=id",
            processes_first,
        ),
        ("GET", "/api/retrieve/v1/processes?page=2", processes_second),
    ])
    .await;
    let client = Client::builder(url).build().unwrap();
    let jobs = client
        .jobs(&JobsRequest {
            limit: Some(NonZeroU32::new(25).unwrap()),
            sort_by: Some(JobSort::CreatedDescending),
            ..JobsRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(
        jobs.next().await.unwrap().unwrap().jobs()[0]
            .job_id
            .as_str(),
        "job-2"
    );
    let processes = client
        .processes(&ProcessListRequest {
            limit: Some(NonZeroU32::new(10).unwrap()),
            sort_by: Some(ProcessSort::IdAscending),
        })
        .await
        .unwrap();
    assert_eq!(
        processes.next().await.unwrap().unwrap().processes()[0]
            .id
            .as_str(),
        "dataset-2"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn receipt_is_available_for_successful_and_failed_jobs() {
    let (url, seen, server) = serve(vec![
        (
            "GET",
            "/api/retrieve/v1/jobs/job-1?log=true",
            "{\"processID\":\"dataset-1\",\"jobID\":\"job-1\",\"status\":\"successful\"}",
        ),
        ("GET", "/api/retrieve/v1/jobs/job-1/receipt", "{\"cost\":3}"),
        (
            "GET",
            "/api/retrieve/v1/jobs/job-2?log=true",
            "{\"processID\":\"dataset-1\",\"jobID\":\"job-2\",\"status\":\"failed\"}",
        ),
        ("GET", "/api/retrieve/v1/jobs/job-2/receipt", "{\"cost\":0}"),
    ])
    .await;
    let client = Client::builder(url).build().unwrap();
    assert_eq!(client.receipt(&job_id("job-1")).await.unwrap()["cost"], 3);
    let mut failed = client.job(&job_id("job-2")).unwrap();
    assert_eq!(failed.receipt().await.unwrap()["cost"], 0);
    server.await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn completed_download_respects_existing_target_policy() {
    let (url, _, server) = serve(asset_responses(
        r#"{"asset":{"value":{"href":"/asset","file:size":4}}}"#,
        ["DATA", "DATA"],
    ))
    .await;
    let client = Client::builder(url).build().unwrap();
    let results = client.job_results_once(&job_id("job-1")).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("data.bin");
    tokio::fs::write(&target, b"OLD").await.unwrap();

    let error = results
        .download_to(&target, ExistingTarget::Error)
        .await
        .unwrap_err();

    assert!(
        matches!(error, Error::Io(ref source) if source.kind() == std::io::ErrorKind::AlreadyExists)
    );
    assert_eq!(tokio::fs::read(&target).await.unwrap(), b"OLD");

    results
        .download_to(&target, ExistingTarget::Replace)
        .await
        .unwrap();
    assert_eq!(tokio::fs::read(&target).await.unwrap(), b"DATA");
    server.await.unwrap();
}

#[tokio::test]
async fn completed_download_accepts_matching_unsatisfied_range() {
    let (url, seen, server) = serve_resumable_asset("TRUNCATED_AT_END", "UNSATISFIED_10").await;
    let results = resumable_results(url).await;
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("data.bin");

    results
        .download_to(&target, ExistingTarget::Error)
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(&target).await.unwrap(), b"0123456789");
    let resumed = seen.lock().unwrap()[3].to_ascii_lowercase();
    assert!(resumed.contains("range: bytes=10-"));
    assert!(resumed.contains("if-range: \"asset-v1\""));
    server.await.unwrap();
}

#[tokio::test]
async fn interrupted_partial_body_cannot_exceed_content_range() {
    let (url, _, server) = serve_resumable_asset("TRUNCATED_ETAG", "OVERRUN_TRUNCATED_RANGE").await;
    let results = resumable_results(url).await;
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("data.bin");
    tokio::fs::write(&target, b"existing").await.unwrap();

    let error = results
        .download_to_with_progress(&target, ExistingTarget::Replace, |progress| {
            assert!(progress.downloaded() <= 6);
        })
        .await
        .unwrap_err();

    assert!(
        matches!(&error, Error::Download(message) if message.contains("exceeds its Content-Range")),
        "{error:?}"
    );
    assert_eq!(tokio::fs::read(&target).await.unwrap(), b"existing");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    server.await.unwrap();
}

#[tokio::test]
async fn interrupted_download_resumes_with_range_and_validator() {
    let (url, seen, server) = serve_resumable_asset("TRUNCATED_ETAG", "PARTIAL").await;
    let results = resumable_results(url).await;
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("data.bin");

    results
        .download_to(&target, ExistingTarget::Error)
        .await
        .unwrap();

    assert_eq!(tokio::fs::read(&target).await.unwrap(), b"0123456789");
    let resumed = seen.lock().unwrap()[3].to_ascii_lowercase();
    assert!(resumed.contains("range: bytes=4-"));
    assert!(resumed.contains("if-range: \"asset-v1\""));
    assert!(resumed.contains("accept-encoding: identity"));
    server.await.unwrap();
}

#[tokio::test]
async fn catalogue_exposes_form_constraints_and_search_statistics() {
    let (url, _, server) = serve(vec![
        (
            "GET",
            "/api/catalogue/v1/datasets?search_stats=true&limit=1",
            "{\"collections\":[{\"id\":\"dataset-1\"}],\"numberMatched\":3,\"numberReturned\":1,\"search\":{\"kw\":[{\"category\":\"Product type\",\"groups\":{\"Reanalysis\":2}}]}}",
        ),
        (
            "GET",
            "/api/catalogue/v1/collections/dataset-1/form.json",
            "[{\"name\":\"year\",\"required\":true}]",
        ),
        (
            "GET",
            "/api/catalogue/v1/collections/dataset-1/constraints.json",
            "[{\"year\":[\"2023\"]}]",
        ),
    ])
    .await;
    let client = Client::builder(url).build().unwrap();
    let request = CollectionsRequest {
        limit: Some(NonZeroU32::new(1).unwrap()),
        search_stats: true,
        ..Default::default()
    };
    let page = client.collections(&request).await.unwrap();
    assert_eq!(page.number_matched(), Some(3));
    assert_eq!(page.number_returned(), Some(1));
    assert_eq!(
        page.search_stats().unwrap().keyword_facets[0].groups["Reanalysis"],
        2
    );
    let id = collection_id("dataset-1");
    assert_eq!(
        client.collection_form(&id).await.unwrap()[0]["name"],
        "year"
    );
    assert_eq!(
        client.collection_constraints(&id).await.unwrap()[0]["year"][0],
        "2023"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn download_reports_absolute_progress_and_response_size() {
    let (url, _, server) = serve(asset_responses(ASSET_RESULTS, ["DATA"])).await;
    let client = Client::builder(url).build().unwrap();
    let results = client.job_results_once(&job_id("job-1")).await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("data.bin");
    let mut updates = Vec::new();

    results
        .download_to_with_progress(&target, ExistingTarget::Error, |progress| {
            updates.push(progress);
        })
        .await
        .unwrap();

    assert_eq!(updates.first().unwrap().downloaded(), 0);
    assert_eq!(updates.first().unwrap().total(), None);
    assert_eq!(updates.last().unwrap().downloaded(), 4);
    assert_eq!(updates.last().unwrap().total(), Some(4));
    server.await.unwrap();
}

#[tokio::test]
async fn download_attempt_limit_is_shared_by_response_and_body_retries() {
    let (url, seen, server) = serve(asset_responses(ASSET_RESULTS, ["ERROR", "TRUNCATED"])).await;
    let client = Client::builder(url)
        .retry_policy(retry_policy(2))
        .build()
        .unwrap();
    let results = client.job_results_once(&job_id("job-1")).await.unwrap();
    let directory = tempfile::tempdir().unwrap();

    assert!(
        results
            .download_to(directory.path().join("data.bin"), ExistingTarget::Error)
            .await
            .is_err()
    );
    server.await.unwrap();
    assert_eq!(seen.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn resumed_job_uses_correct_results_url_and_keeps_token_off_asset_request() {
    let (url, seen, server) = serve(vec![
        ("GET", "/api/retrieve/v1/jobs/job-1?log=true", "{\"processID\":\"dataset-1\",\"jobID\":\"job-1\",\"status\":\"successful\"}"),
        ("GET", "/api/retrieve/v1/jobs/job-1/results", "{\"asset\":{\"value\":{\"href\":\"/asset?signature=top-secret\",\"file:size\":4,\"type\":\"application/octet-stream\"}}}"),
        ("GET", "/asset?signature=top-secret", "DATA"),
    ]).await;
    let client = Client::builder(url)
        .api_key(ApiKey::parse("secret").unwrap())
        .build()
        .unwrap();
    let mut job = client.job(&job_id("job-1")).unwrap();
    let results = job.wait_for_results().await.unwrap();
    assert_eq!(results.asset().file_size(), Some(4));
    assert!(!format!("{results:?}").contains("top-secret"));
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("data.bin");
    results
        .download_to(&target, ExistingTarget::Error)
        .await
        .unwrap();
    assert_eq!(tokio::fs::read(target).await.unwrap(), b"DATA");
    server.await.unwrap();
    let requests = seen.lock().unwrap();
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("private-token: secret")
    );
    assert!(!requests[2].to_ascii_lowercase().contains("private-token"));
    drop(requests);
}
