use std::{
    collections::VecDeque,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use colored::Colorize as _;
#[cfg(test)]
use ecmwf_datastores_cli::plan::build_plan;
use ecmwf_datastores_cli::{
    config::{ExecutionConfig, RetrievalSpec},
    plan::{PlannedRequest, final_output},
};
use ecmwf_datastores_client::{Client, CollectionId, Job, JobId, JobResults, RequestCost};
use serde::Serialize;
use tokio::task::JoinSet;

use super::{
    Connection, adaptive_plan, assembly, ensure_distinct_targets, ensure_targets_available,
    existing_target, print_retrieval_plan,
    progress::{DownloadBar, PipelineStatus, ProgressDisplay, ProgressMode},
    write_atomic_async,
};

type StageResult<T> = std::result::Result<T, ecmwf_datastores_client::error::Error>;

type ProcessingOutcome = StageOutcome<JobResults>;

type DownloadOutcome = StageOutcome<Downloaded>;

type FailureTransition = fn(RequestProgress, u64, String) -> RequestProgress;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum FailurePhase {
    Assembly,

    Downloading,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum RetrievalState {
    Failed {
        phase: FailurePhase,

        error: String,

        retained_parts: Vec<PathBuf>,
    },

    Completed {
        final_output: PathBuf,
    },

    Assembling,

    Downloading,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum RequestProgress {
    Planned,

    Submitted {
        #[serde(flatten)]
        submission: SubmissionProgress,
    },

    Processed {
        #[serde(flatten)]
        processing: ProcessingProgress,
    },

    Completed {
        #[serde(flatten)]
        completion: CompletionProgress,
    },

    #[serde(rename = "error")]
    DownloadFailed {
        error: String,

        #[serde(flatten)]
        processing: ProcessingProgress,

        download_ms: u64,
    },

    #[serde(rename = "error")]
    ProcessingFailed {
        error: String,

        #[serde(flatten)]
        submission: SubmissionProgress,

        processing_ms: u64,
    },

    #[serde(rename = "error")]
    SubmissionFailed {
        error: String,

        submitted_ms: u64,
    },
    SubmissionUnknown {
        error: String,

        submitted_ms: u64,
    },
}

impl RequestProgress {
    fn submitted(self, job_id: JobId, submitted_ms: u64) -> Self {
        let Self::Planned = self else {
            unreachable!("only a planned request can be submitted");
        };
        Self::Submitted {
            submission: SubmissionProgress {
                job_id,
                submitted_ms,
            },
        }
    }

    fn processed(self, processing_ms: u64, bytes: Option<u64>) -> Self {
        let Self::Submitted { submission } = self else {
            unreachable!("only a submitted request can be processed");
        };
        Self::Processed {
            processing: ProcessingProgress {
                bytes,
                submission,
                processing_ms,
            },
        }
    }

    fn completed(
        self,
        download_ms: u64,
        bytes: Option<u64>,
        bytes_per_second: Option<u64>,
    ) -> Self {
        let Self::Processed { mut processing } = self else {
            unreachable!("only a processed request can be completed");
        };
        processing.bytes = bytes;
        Self::Completed {
            completion: CompletionProgress {
                processing,
                download_ms,
                bytes_per_second,
            },
        }
    }

    fn download_failed(self, download_ms: u64, error: String) -> Self {
        let Self::Processed { processing } = self else {
            unreachable!("only a processed request can fail during download");
        };
        Self::DownloadFailed {
            processing,
            download_ms,
            error,
        }
    }

    fn processing_failed(self, processing_ms: u64, error: String) -> Self {
        let Self::Submitted { submission } = self else {
            unreachable!("only a submitted request can fail during processing");
        };
        Self::ProcessingFailed {
            submission,
            processing_ms,
            error,
        }
    }

    fn submission_failed(self, submitted_ms: u64, error: String) -> Self {
        let Self::Planned = self else {
            unreachable!("only a planned request can fail during submission");
        };
        Self::SubmissionFailed {
            submitted_ms,
            error,
        }
    }
    fn submission_unknown(self, submitted_ms: u64, error: String) -> Self {
        let Self::Planned = self else {
            unreachable!("only a planned request can have an unknown submission outcome");
        };
        Self::SubmissionUnknown {
            submitted_ms,
            error,
        }
    }
}

struct Execution<'a> {
    client: &'a Client,

    limits: ExecutionConfig,

    dataset: &'a CollectionId,

    progress: &'a ProgressDisplay,

    overwrite: bool,

    report_path: Option<&'a Path>,

    journal_path: Option<&'a Path>,
}

impl Execution<'_> {
    async fn run(
        &self,
        plan: &[PlannedRequest],
        report: &mut Option<RetrievalReport>,
    ) -> Result<()> {
        let mut next = 0;
        let mut submission = JoinSet::new();
        let mut processing = JoinSet::new();
        let mut ready_downloads: VecDeque<ReadyDownload> = VecDeque::new();
        let mut downloads = JoinSet::new();
        let mut completed = 0;

        loop {
            // Bound all submitted work that has not reached a download slot.
            // This applies backpressure without coupling ready downloads to an
            // in-flight submission.
            if next < plan.len()
                && submission.is_empty()
                && processing.len() + ready_downloads.len() < self.limits.max_active_jobs().get()
            {
                let index = next;
                self.progress.submitting(index + 1);
                submission.spawn(submit_request(
                    index,
                    self.client.clone(),
                    self.dataset.clone(),
                    plan[index].selection().clone(),
                ));
                next += 1;
            }
            while downloads.len() < self.limits.max_concurrent_downloads().get()
                && let Some(ready) = ready_downloads.pop_front()
            {
                let output = plan[ready.index].output().to_path_buf();
                let progress = self.progress.download(ready.index + 1, ready.bytes);
                downloads.spawn(download_result(ready, output, self.overwrite, progress));
            }

            self.progress.update_pipeline(PipelineStatus::new(
                completed,
                (!submission.is_empty()).then_some(next),
                processing.len(),
                ready_downloads.len(),
                downloads.len(),
            ));

            if next == plan.len()
                && submission.is_empty()
                && processing.is_empty()
                && ready_downloads.is_empty()
                && downloads.is_empty()
            {
                return Ok(());
            }

            tokio::select! {
                outcome = submission.join_next(), if !submission.is_empty() => {
                    let outcome = outcome
                        .expect("submission set is not empty")
                        .context("submission task failed")?;
                    let (index, job) = self.handle_submission(outcome, report).await?;
                    processing.spawn(wait_for_results(index, job));
                }
                outcome = processing.join_next(), if !processing.is_empty() => {
                    let result = outcome
                        .expect("processing set is not empty")
                        .context("processing task failed");
                    let result = match result {
                        Ok(outcome) => self.handle_processing(outcome, report).await,
                        Err(error) => Err(error),
                    };
                    match result {
                        Ok(ready) => ready_downloads.push_back(ready),
                        Err(error) => {
                            self.settle_submission(&mut submission, report).await;
                            return Err(error);
                        }
                    }
                }
                outcome = downloads.join_next(), if !downloads.is_empty() => {
                    let result = outcome
                        .expect("download set is not empty")
                        .context("download task failed");
                    let result = match result {
                        Ok(outcome) => self.handle_download(outcome, report).await,
                        Err(error) => Err(error),
                    };
                    if let Err(error) = result {
                        self.settle_submission(&mut submission, report).await;
                        return Err(error);
                    }
                    completed += 1;
                }
            }
        }
    }

    async fn fail(
        &self,
        report: &mut Option<RetrievalReport>,
        index: usize,
        elapsed: Duration,
        error: anyhow::Error,
        transition: FailureTransition,
    ) -> anyhow::Error {
        update_progress(report, index, |progress| {
            transition(progress, elapsed_ms(elapsed), format!("{error:#}"))
        });
        persist_after_error(
            self.report_path,
            self.journal_path,
            report.as_ref(),
            index,
            error,
        )
        .await
    }

    async fn handle_download(
        &self,
        outcome: DownloadOutcome,
        report: &mut Option<RetrievalReport>,
    ) -> Result<()> {
        let downloaded = match outcome.result {
            Ok(downloaded) => downloaded,
            Err(source) => {
                let error = anyhow::Error::new(source)
                    .context(format!("failed to download job {}", outcome.job_id));
                return Err(self
                    .fail(
                        report,
                        outcome.index,
                        outcome.elapsed,
                        error,
                        RequestProgress::download_failed,
                    )
                    .await);
            }
        };
        let bytes = downloaded.bytes.or_else(|| {
            fs::metadata(&downloaded.path)
                .ok()
                .map(|metadata| metadata.len())
        });
        update_report(report, outcome.index, |entry| {
            transition_progress(&mut entry.progress, |progress| {
                progress.completed(
                    elapsed_ms(outcome.elapsed),
                    bytes,
                    transfer_rate(bytes, outcome.elapsed),
                )
            });
            entry.output.clone_from(&downloaded.path);
        });
        append_report_event(self.journal_path, report.as_ref(), outcome.index).await?;
        self.progress.saved(&outcome.job_id, &downloaded.path);
        Ok(())
    }
    async fn handle_submission(
        &self,
        outcome: SubmissionOutcome,
        report: &mut Option<RetrievalReport>,
    ) -> Result<(usize, Job)> {
        let number = outcome.index + 1;
        let job = match outcome.result {
            Ok(job) => job,
            Err(source) => {
                let transition: FailureTransition = if matches!(
                    source,
                    ecmwf_datastores_client::error::Error::SubmissionUnknown(_)
                ) {
                    RequestProgress::submission_unknown
                } else {
                    RequestProgress::submission_failed
                };
                let error = anyhow::Error::new(source)
                    .context(format!("failed to submit request {number}"));
                return Err(self
                    .fail(report, outcome.index, outcome.elapsed, error, transition)
                    .await);
            }
        };
        update_progress(report, outcome.index, |progress| {
            progress.submitted(job.id().clone(), elapsed_ms(outcome.elapsed))
        });
        append_report_event(self.journal_path, report.as_ref(), outcome.index)
            .await
            .with_context(|| {
                format!(
                    "job {} was submitted but its report event could not be saved",
                    job.id()
                )
            })?;
        self.progress.submitted(number, job.id());
        Ok((outcome.index, job))
    }
    async fn handle_processing(
        &self,
        outcome: ProcessingOutcome,
        report: &mut Option<RetrievalReport>,
    ) -> Result<ReadyDownload> {
        let results = match outcome.result {
            Ok(results) => results,
            Err(source) => {
                let error = anyhow::Error::new(source)
                    .context(format!("job {} did not produce a result", outcome.job_id));
                return Err(self
                    .fail(
                        report,
                        outcome.index,
                        outcome.elapsed,
                        error,
                        RequestProgress::processing_failed,
                    )
                    .await);
            }
        };
        let bytes = results.asset().file_size();
        update_progress(report, outcome.index, |progress| {
            progress.processed(elapsed_ms(outcome.elapsed), bytes)
        });
        append_report_event(self.journal_path, report.as_ref(), outcome.index).await?;
        Ok(ReadyDownload {
            index: outcome.index,
            job_id: outcome.job_id,
            results,
            bytes,
        })
    }

    async fn settle_submission(
        &self,
        submission: &mut JoinSet<SubmissionOutcome>,
        report: &mut Option<RetrievalReport>,
    ) {
        let Some(outcome) = submission.join_next().await else {
            return;
        };
        let result = match outcome.context("submission task failed") {
            Ok(outcome) => self.handle_submission(outcome, report).await.map(|_| ()),
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            eprintln!(
                "{} an in-flight submission also failed: {error:#}",
                "Warning:".yellow().bold()
            );
        }
    }
}

struct Downloaded {
    path: PathBuf,

    bytes: Option<u64>,
}

#[derive(Serialize)]
struct ReportEvent<'a> {
    number: usize,

    #[serde(flatten)]
    progress: &'a RequestProgress,
}

struct StageOutcome<T> {
    index: usize,

    job_id: JobId,

    result: StageResult<T>,

    elapsed: Duration,
}

#[derive(Debug, Serialize)]
struct RequestReport {
    number: usize,

    output: PathBuf,

    request: ecmwf_datastores_client::Selection,

    #[serde(flatten)]
    progress: RequestProgress,

    #[serde(skip_serializing_if = "Option::is_none")]
    provider_cost: Option<RequestCost>,

    estimated_items: Option<usize>,
}

struct ReadyDownload {
    index: usize,

    bytes: Option<u64>,

    job_id: JobId,

    results: JobResults,
}

#[derive(Debug, Serialize)]
struct RetrievalReport {
    #[serde(flatten)]
    state: RetrievalState,

    output: PathBuf,

    version: u8,

    dataset: CollectionId,

    requests: Vec<RequestReport>,
}

impl RetrievalReport {
    fn new(spec: &RetrievalSpec, plan: &[PlannedRequest]) -> Self {
        Self {
            version: 3,
            dataset: spec.dataset().clone(),
            output: final_output(spec, plan),
            state: RetrievalState::Downloading,
            requests: plan
                .iter()
                .enumerate()
                .map(|(index, request)| RequestReport {
                    number: index + 1,
                    output: request.output().to_path_buf(),
                    estimated_items: request.estimated_items(),
                    provider_cost: request.provider_cost().cloned(),
                    request: request.selection().clone(),
                    progress: RequestProgress::Planned,
                })
                .collect(),
        }
    }

    fn failed(&mut self, phase: FailurePhase, error: &anyhow::Error, retained_parts: Vec<PathBuf>) {
        self.state = RetrievalState::Failed {
            phase,
            error: format!("{error:#}"),
            retained_parts,
        };
    }

    fn completed(&mut self, final_output: PathBuf) {
        self.state = RetrievalState::Completed { final_output };
    }

    fn assembling(&mut self) {
        self.state = RetrievalState::Assembling;
    }
}

struct SubmissionOutcome {
    index: usize,

    result: StageResult<Job>,

    elapsed: Duration,
}

#[derive(Debug, Serialize)]
struct SubmissionProgress {
    job_id: JobId,

    submitted_ms: u64,
}

#[derive(Debug, Serialize)]
struct ProcessingProgress {
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,

    #[serde(flatten)]
    submission: SubmissionProgress,

    processing_ms: u64,
}

#[derive(Debug, Serialize)]
struct CompletionProgress {
    #[serde(flatten)]
    processing: ProcessingProgress,

    download_ms: u64,

    #[serde(skip_serializing_if = "Option::is_none")]
    bytes_per_second: Option<u64>,
}

fn elapsed_ms(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

fn transfer_rate(bytes: Option<u64>, elapsed: Duration) -> Option<u64> {
    let nanoseconds = elapsed.as_nanos();
    bytes.filter(|_| nanoseconds > 0).map(|bytes| {
        let rate = u128::from(bytes).saturating_mul(1_000_000_000) / nanoseconds;
        u64::try_from(rate).unwrap_or(u64::MAX)
    })
}

fn update_report(
    report: &mut Option<RetrievalReport>,
    index: usize,
    update: impl FnOnce(&mut RequestReport),
) {
    if let Some(report) = report {
        update(&mut report.requests[index]);
    }
}

fn update_progress(
    report: &mut Option<RetrievalReport>,
    index: usize,
    update: impl FnOnce(RequestProgress) -> RequestProgress,
) {
    update_report(report, index, |entry| {
        transition_progress(&mut entry.progress, update);
    });
}

fn event_log_path(path: &Path) -> PathBuf {
    let mut filename = path.as_os_str().to_os_string();
    filename.push(".events.jsonl");
    PathBuf::from(filename)
}

fn existing_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths.iter().filter(|path| path.exists()).cloned().collect()
}

fn transition_progress(
    progress: &mut RequestProgress,
    update: impl FnOnce(RequestProgress) -> RequestProgress,
) {
    let current = std::mem::replace(progress, RequestProgress::Planned);
    *progress = update(current);
}

async fn submit_request(
    index: usize,
    client: Client,
    dataset: CollectionId,
    selection: ecmwf_datastores_client::Selection,
) -> SubmissionOutcome {
    let started = Instant::now();
    let result = client.submit(&dataset, &selection).await;
    SubmissionOutcome {
        index,
        result,
        elapsed: started.elapsed(),
    }
}

async fn download_result(
    ready: ReadyDownload,
    output: PathBuf,
    overwrite: bool,
    mut progress: DownloadBar,
) -> DownloadOutcome {
    let started = Instant::now();
    let bytes = ready.bytes;
    let result = ready
        .results
        .download_to_with_progress(output, existing_target(overwrite), |update| {
            progress.update(update);
        })
        .await
        .map(|path| Downloaded { path, bytes });
    progress.finish();
    StageOutcome {
        index: ready.index,
        job_id: ready.job_id,
        elapsed: started.elapsed(),
        result,
    }
}

pub async fn retrieve_command(
    path: &Path,
    connection: &Connection,
    overwrite: bool,
    report_path: Option<&Path>,
    progress_mode: ProgressMode,
) -> Result<()> {
    let spec = RetrievalSpec::from_path(path)?;
    let client = connection.client()?;
    let plan = adaptive_plan(&spec, &client, progress_mode).await?;
    if plan.len() > 1 {
        assembly::validate_multipart_request(spec.request())?;
    }
    let final_output = final_output(&spec, &plan);
    let journal_path = report_path.map(event_log_path);
    let output_paths = plan
        .iter()
        .map(|request| request.output().to_path_buf())
        .collect::<Vec<_>>();
    let distinct_final_output = (plan.len() > 1).then_some(final_output.as_path());
    ensure_distinct_targets(
        output_paths
            .iter()
            .map(PathBuf::as_path)
            .chain(distinct_final_output)
            .chain(report_path)
            .chain(journal_path.as_deref()),
    )?;
    ensure_targets_available(
        output_paths
            .iter()
            .map(PathBuf::as_path)
            .chain(distinct_final_output),
        overwrite,
    )?;
    if let Some(path) = report_path {
        ensure_targets_available(
            std::iter::once(path).chain(journal_path.as_deref()),
            overwrite,
        )?;
    }
    print_retrieval_plan(&spec, &plan);
    let mut report = report_path.map(|_| RetrievalReport::new(&spec, &plan));
    if let (Some(path), Some(report)) = (report_path, report.as_ref()) {
        write_report_async(path, report, overwrite).await?;
    }
    if let Some(path) = journal_path.as_deref() {
        write_atomic_async(path, String::new(), overwrite).await?;
    }
    let progress = ProgressDisplay::new(plan.len(), progress_mode);

    let execution = Execution {
        client: &client,
        dataset: spec.dataset(),
        overwrite,
        report_path,
        journal_path: journal_path.as_deref(),
        limits: spec.execution(),
        progress: &progress,
    };
    if let Err(error) = execution.run(&plan, &mut report).await {
        progress.fail();
        if let Some(report) = &mut report {
            report.failed(
                FailurePhase::Downloading,
                &error,
                existing_paths(&output_paths),
            );
        }
        persist_report_best_effort(report_path, report.as_ref(), "after failure").await;
        return Err(error);
    }
    if let Err(error) = assemble_downloads(
        &output_paths,
        &final_output,
        overwrite,
        report_path,
        &mut report,
    )
    .await
    {
        progress.fail();
        return Err(error);
    }
    finish_retrieval(
        plan.len(),
        &final_output,
        report_path,
        &mut report,
        progress,
    )
    .await
}

async fn finish_retrieval(
    part_count: usize,
    final_output: &Path,
    report_path: Option<&Path>,
    report: &mut Option<RetrievalReport>,
    progress: ProgressDisplay,
) -> Result<()> {
    if let Some(report) = report.as_mut() {
        report.completed(final_output.to_path_buf());
    }
    if part_count > 1 {
        println!(
            "{} results to {}",
            "Saved".green().bold(),
            final_output.display().to_string().green()
        );
    }
    let (Some(path), Some(report)) = (report_path, report.as_ref()) else {
        progress.finish();
        return Ok(());
    };
    if let Err(error) = write_report_async(path, report, true).await {
        progress.fail();
        return Err(error);
    }
    progress.finish();
    println!(
        "{} report to {}",
        "Wrote".green().bold(),
        path.display().to_string().green()
    );
    Ok(())
}

async fn wait_for_results(index: usize, mut job: Job) -> ProcessingOutcome {
    let job_id = job.id().clone();
    let started = Instant::now();
    let result = job.wait_for_results().await;
    StageOutcome {
        index,
        job_id,
        elapsed: started.elapsed(),
        result,
    }
}

async fn assemble_downloads(
    output_paths: &[PathBuf],
    final_output: &Path,
    overwrite: bool,
    report_path: Option<&Path>,
    report: &mut Option<RetrievalReport>,
) -> Result<()> {
    if output_paths.len() > 1 {
        if let Some(report) = report.as_mut() {
            report.assembling();
        }
        persist_report_best_effort(report_path, report.as_ref(), "before assembly").await;
    }
    if let Err(error) =
        assembly::assemble(output_paths.to_vec(), final_output.to_path_buf(), overwrite).await
    {
        let error = error.context("failed to assemble downloaded parts");
        if let Some(report) = report.as_mut() {
            report.failed(FailurePhase::Assembly, &error, existing_paths(output_paths));
        }
        persist_report_best_effort(report_path, report.as_ref(), "after failure").await;
        return Err(error);
    }
    Ok(())
}

async fn write_report_async(path: &Path, report: &RetrievalReport, overwrite: bool) -> Result<()> {
    let mut contents = serde_json::to_string_pretty(report)?;
    contents.push('\n');
    write_atomic_async(path, contents, overwrite).await
}

async fn append_report_event(
    path: Option<&Path>,
    report: Option<&RetrievalReport>,
    index: usize,
) -> Result<()> {
    if let (Some(path), Some(report)) = (path, report) {
        let entry = &report.requests[index];
        let mut event = serde_json::to_vec(&ReportEvent {
            number: entry.number,
            progress: &entry.progress,
        })?;
        event.push(b'\n');
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .with_context(|| format!("failed to open report journal {}", path.display()))?;
            file.write_all(&event)
                .and_then(|()| file.sync_all())
                .with_context(|| format!("failed to append report journal {}", path.display()))
        })
        .await??;
    }
    Ok(())
}

async fn persist_after_error(
    path: Option<&Path>,
    journal: Option<&Path>,
    report: Option<&RetrievalReport>,
    index: usize,
    error: anyhow::Error,
) -> anyhow::Error {
    if let Err(report_error) = append_report_event(journal, report, index).await {
        eprintln!(
            "{} failed to update report journal: {report_error:#}",
            "Warning:".yellow().bold()
        );
    }
    if let (Some(path), Some(report)) = (path, report)
        && let Err(report_error) = write_report_async(path, report, true).await
    {
        eprintln!(
            "{} failed to update report: {report_error:#}",
            "Warning:".yellow().bold()
        );
    }
    error
}

async fn persist_report_best_effort(
    path: Option<&Path>,
    report: Option<&RetrievalReport>,
    description: &str,
) {
    if let (Some(path), Some(report)) = (path, report)
        && let Err(error) = write_report_async(path, report, true).await
    {
        eprintln!(
            "{} failed to save report {description}: {error:#}",
            "Warning:".yellow().bold()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn journal_records_job_id_before_final_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.json");
        let journal = event_log_path(&path);
        let spec = RetrievalSpec::parse(
            "dataset = 'future-dataset'\n[request]\nvariable = ['temperature']\n",
        )
        .unwrap();
        let plan = build_plan(&spec).unwrap();
        let mut report = Some(RetrievalReport::new(&spec, &plan));
        write_report_async(&path, report.as_ref().unwrap(), false)
            .await
            .unwrap();
        write_atomic_async(&journal, String::new(), false)
            .await
            .unwrap();

        update_progress(&mut report, 0, |progress| {
            progress.submitted(JobId::parse("test-job").unwrap(), 12)
        });
        append_report_event(Some(&journal), report.as_ref(), 0)
            .await
            .unwrap();

        let event: serde_json::Value =
            serde_json::from_str(fs::read_to_string(&journal).unwrap().trim()).unwrap();
        assert_eq!(event["job_id"], "test-job");
        assert_eq!(event["status"], "submitted");
        assert_eq!(event["number"], 1);
        assert!(event.get("request").is_none());
        assert!(event.get("output").is_none());
        let snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(snapshot["requests"][0]["status"], "planned");
    }

    #[test]
    fn completed_progress_preserves_all_stage_data() {
        let progress = RequestProgress::Planned
            .submitted(JobId::parse("test-job").unwrap(), 12)
            .processed(34, Some(1_000))
            .completed(56, Some(1_024), Some(18));

        let value = serde_json::to_value(progress).unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "status": "completed",
                "job_id": "test-job",
                "submitted_ms": 12,
                "processing_ms": 34,
                "download_ms": 56,
                "bytes": 1_024,
                "bytes_per_second": 18,
            })
        );
    }

    #[test]
    fn retrieval_report_exposes_top_level_lifecycle() {
        let spec = RetrievalSpec::parse(
            "dataset = 'future-dataset'\n[request]\nvariable = ['temperature']\n",
        )
        .unwrap();
        let plan = build_plan(&spec).unwrap();
        let mut report = RetrievalReport::new(&spec, &plan);
        let initial = serde_json::to_value(&report).unwrap();
        assert_eq!(initial["version"], 3);
        assert_eq!(initial["status"], "downloading");

        let error = anyhow::anyhow!("assembly conflict");
        report.failed(
            FailurePhase::Assembly,
            &error,
            vec![PathBuf::from("part.nc")],
        );
        let failed = serde_json::to_value(report).unwrap();
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["phase"], "assembly");
        assert_eq!(failed["retained_parts"], serde_json::json!(["part.nc"]));
    }

    #[test]
    fn stage_specific_failure_preserves_error_wire_status() {
        let progress = RequestProgress::Planned
            .submitted(JobId::parse("test-job").unwrap(), 12)
            .processing_failed(34, "failed".into());

        let value = serde_json::to_value(progress).unwrap();

        assert_eq!(value["status"], "error");
        assert_eq!(value["job_id"], "test-job");
        assert_eq!(value["submitted_ms"], 12);
        assert_eq!(value["processing_ms"], 34);
        assert_eq!(value["error"], "failed");
        assert!(value.get("download_ms").is_none());
    }

    #[test]
    fn transfer_rate_uses_integer_bytes_per_second() {
        assert_eq!(
            transfer_rate(Some(1_000), Duration::from_millis(500)),
            Some(2_000)
        );
        assert_eq!(transfer_rate(None, Duration::from_secs(1)), None);
    }

    #[tokio::test]
    async fn starts_ready_download_while_next_submission_is_in_flight() {
        let (endpoint, requests, server) = pipeline_server().await;
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("result.nc");
        let spec = RetrievalSpec::parse(&format!(
            r#"
dataset = "derived-era5-single-levels-daily-statistics"
output = "{}"
[time]
start = "2025-01-01"
end = "2025-01-01"
[split]
max_items = 1
[request]
variable = ["2m_temperature", "surface_pressure", "total_precipitation"]
"#,
            output.display()
        ))
        .unwrap();
        let plan = build_plan(&spec).unwrap();
        assert_eq!(plan.len(), 3);
        let client = Client::builder(endpoint.parse().unwrap()).build().unwrap();
        let progress = ProgressDisplay::new(plan.len(), ProgressMode::Never);
        let execution = Execution {
            client: &client,
            dataset: spec.dataset(),
            overwrite: false,
            report_path: None,
            journal_path: None,
            limits: spec.execution(),
            progress: &progress,
        };

        execution.run(&plan, &mut None).await.unwrap();
        server.await.unwrap();

        let requests = requests.lock().unwrap().clone();
        let third_submission = requests
            .iter()
            .enumerate()
            .filter(|(_, request)| request.starts_with("POST "))
            .nth(2)
            .unwrap()
            .0;
        let first_download = requests
            .iter()
            .position(|request| request.starts_with("GET /asset/"))
            .unwrap();
        assert!(first_download < third_submission, "{requests:#?}");
    }

    async fn pipeline_server() -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/api/", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let submissions = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(async move {
            let mut handlers = JoinSet::new();
            for _ in 0..12 {
                let (stream, _) = listener.accept().await.unwrap();
                let captured = Arc::clone(&captured);
                let submissions = Arc::clone(&submissions);
                handlers.spawn(handle_pipeline_request(stream, captured, submissions));
            }
            while let Some(result) = handlers.join_next().await {
                result.unwrap();
            }
        });
        (endpoint, requests, server)
    }

    async fn handle_pipeline_request(
        mut stream: tokio::net::TcpStream,
        requests: Arc<Mutex<Vec<String>>>,
        submissions: Arc<AtomicUsize>,
    ) {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
        }
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .and_then(|value| value.parse::<usize>().ok())
            })
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
        }
        let request_line = headers.lines().next().unwrap().to_owned();
        requests.lock().unwrap().push(request_line.clone());

        let body = if request_line.starts_with("POST ") {
            let number = submissions.fetch_add(1, Ordering::SeqCst) + 1;
            if number == 2 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            format!(r#"{{"jobID":"job-{number}"}}"#)
        } else if request_line.contains("/results ") {
            let job_id = request_line
                .split_whitespace()
                .nth(1)
                .unwrap()
                .split('/')
                .nth_back(1)
                .unwrap();
            format!(r#"{{"asset":{{"value":{{"href":"/asset/{job_id}","file:size":4}}}}}}"#)
        } else if request_line.starts_with("GET /asset/") {
            "DATA".to_owned()
        } else {
            let path = request_line.split_whitespace().nth(1).unwrap();
            let job_id = path
                .split('/')
                .next_back()
                .unwrap()
                .split('?')
                .next()
                .unwrap();
            format!(r#"{{"processID":"dataset","jobID":"{job_id}","status":"successful"}}"#)
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }
}
