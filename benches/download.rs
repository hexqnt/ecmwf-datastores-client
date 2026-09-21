use std::{hint::black_box, sync::Arc, time::Duration};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use ecmwf_datastores_client::{Client, ExistingTarget, JobId, JobResults};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use url::Url;

const RESULTS_PATH: &str = "/api/retrieve/v1/jobs/benchmark-job/results";
const JOB_PATH: &str = "/api/retrieve/v1/jobs/benchmark-job";
const ASSET_PATH: &str = "/asset";

struct DownloadServer {
    results: JobResults,
    task: JoinHandle<()>,
}

impl DownloadServer {
    async fn start(size: usize, server_chunk_size: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("benchmark server binds");
        let endpoint = Url::parse(&format!(
            "http://{}/api/",
            listener
                .local_addr()
                .expect("benchmark address is available")
        ))
        .expect("benchmark URL is valid");
        let body: Arc<[u8]> = vec![0x5a; size].into();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener
                    .accept()
                    .await
                    .expect("benchmark connection accepts");
                let body = Arc::clone(&body);
                tokio::spawn(async move {
                    serve_connection(stream, &body, server_chunk_size).await;
                });
            }
        });
        let client = Client::builder(endpoint)
            .request_timeout(Duration::from_secs(30))
            .download_idle_timeout(Duration::from_secs(30))
            .build()
            .expect("benchmark client builds");
        let results = client
            .job_results_once(&JobId::parse("benchmark-job").expect("benchmark job ID is valid"))
            .await
            .expect("benchmark results are available");
        Self { results, task }
    }
}

impl Drop for DownloadServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_connection(mut stream: TcpStream, body: &[u8], server_chunk_size: usize) {
    let mut request = Vec::with_capacity(1_024);
    let mut buffer = [0_u8; 1_024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream
            .read(&mut buffer)
            .await
            .expect("benchmark request can be read");
        assert!(read > 0, "benchmark request ended before its headers");
        request.extend_from_slice(&buffer[..read]);
    }
    let request = std::str::from_utf8(&request).expect("benchmark request is UTF-8");
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_ascii_whitespace().nth(1))
        .expect("benchmark request contains a path");

    if path == JOB_PATH {
        let response_body =
            r#"{"processID":"benchmark","jobID":"benchmark-job","status":"successful"}"#;
        write_headers(&mut stream, "application/json", response_body.len()).await;
        stream
            .write_all(response_body.as_bytes())
            .await
            .expect("benchmark job can be written");
    } else if path == RESULTS_PATH {
        let response_body = format!(
            r#"{{"asset":{{"value":{{"href":"{ASSET_PATH}","file:size":{}}}}}}}"#,
            body.len()
        );
        write_headers(&mut stream, "application/json", response_body.len()).await;
        stream
            .write_all(response_body.as_bytes())
            .await
            .expect("benchmark results can be written");
    } else {
        assert_eq!(path, ASSET_PATH);
        write_headers(&mut stream, "application/octet-stream", body.len()).await;
        for chunk in body.chunks(server_chunk_size) {
            stream
                .write_all(chunk)
                .await
                .expect("benchmark asset can be written");
        }
    }
}

async fn write_headers(stream: &mut TcpStream, content_type: &str, content_length: usize) {
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {content_length}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(headers.as_bytes())
        .await
        .expect("benchmark headers can be written");
}

fn benchmark_download(criterion: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("benchmark runtime builds");
    let mut group = criterion.benchmark_group("download/loopback_to_file");
    group.sample_size(20);

    for (name, size, chunk_size) in [
        ("1_mib_64_kib_chunks", 1024 * 1024, 64 * 1024),
        ("32_mib_64_kib_chunks", 32 * 1024 * 1024, 64 * 1024),
        ("32_mib_1_mib_chunks", 32 * 1024 * 1024, 1024 * 1024),
    ] {
        let server = runtime.block_on(DownloadServer::start(size, chunk_size));
        group.throughput(Throughput::Bytes(
            u64::try_from(size).expect("benchmark size fits in u64"),
        ));
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &server,
            |bencher, server| {
                bencher.iter_batched(
                    || {
                        let directory =
                            tempfile::tempdir().expect("benchmark directory can be created");
                        let target = directory.path().join("asset.bin");
                        (directory, target)
                    },
                    |(_directory, target)| {
                        let saved = runtime
                            .block_on(server.results.download_to(&target, ExistingTarget::Error))
                            .expect("benchmark download succeeds");
                        black_box(saved);
                    },
                    BatchSize::PerIteration,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(benches, benchmark_download);
criterion_main!(benches);
