use std::{future::Future, path::Path, time::Duration};

use clap::ValueEnum;
use colored::Colorize as _;
use console::Term;
use ecmwf_datastores_client::{DownloadProgress, JobId};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

#[derive(Debug, Default, Clone, Copy, ValueEnum)]
pub enum ProgressMode {
    /// Show interactive progress when standard error is a terminal.
    #[default]
    Auto,

    /// Disable interactive progress.
    Never,

    /// Show interactive progress even when terminal detection fails.
    Always,
}

pub struct DownloadBar {
    bar: TransientBar,

    determinate: bool,
}

impl DownloadBar {
    fn hidden() -> Self {
        Self {
            bar: TransientBar::hidden(),
            determinate: false,
        }
    }

    pub fn update(&mut self, progress: DownloadProgress) {
        let Some(bar) = self.bar.get() else {
            return;
        };
        if !self.determinate
            && let Some(total) = progress.total()
        {
            bar.set_length(total);
            bar.set_style(download_style());
            self.determinate = true;
        }
        bar.set_position(progress.downloaded());
    }

    pub fn finish(self) {
        self.bar.finish();
    }
}

struct TransientBar(Option<ProgressBar>);

impl TransientBar {
    fn new(bar: ProgressBar) -> Self {
        Self(Some(bar))
    }

    fn get(&self) -> Option<&ProgressBar> {
        self.0.as_ref()
    }

    fn clear(&mut self) {
        if let Some(bar) = self.0.take() {
            bar.finish_and_clear();
        }
    }

    const fn hidden() -> Self {
        Self(None)
    }

    fn finish(mut self) {
        self.clear();
    }
}

impl Drop for TransientBar {
    fn drop(&mut self) {
        self.clear();
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PipelineStatus {
    ready: usize,

    completed: usize,

    submitting: Option<usize>,

    processing: usize,

    downloading: usize,
}

impl PipelineStatus {
    pub const fn new(
        completed: usize,
        submitting: Option<usize>,
        processing: usize,
        ready: usize,
        downloading: usize,
    ) -> Self {
        Self {
            ready,
            completed,
            submitting,
            processing,
            downloading,
        }
    }

    fn message(self, total: usize) -> String {
        let mut stages = vec![format!("completed: {}", self.completed)];
        if let Some(number) = self.submitting {
            stages.push(format!("submitting request {number}/{total}"));
        }
        if self.processing > 0 {
            stages.push(format!("processing: {}", self.processing));
        }
        if self.ready > 0 {
            stages.push(format!("ready: {}", self.ready));
        }
        if self.downloading > 0 {
            stages.push(format!("downloading: {}", self.downloading));
        }
        stages.join(", ")
    }
}

pub struct ProgressDisplay {
    multi: MultiProgress,

    total: usize,

    overall: ProgressBar,

    finalized: bool,

    interactive: bool,
}

impl ProgressDisplay {
    pub fn new(total: usize, mode: ProgressMode) -> Self {
        let multi = MultiProgress::with_draw_target(progress_target(mode));
        let overall = multi.add(ProgressBar::new(as_u64(total)));
        overall.set_style(overall_style());
        overall.set_prefix("Overall");
        overall.set_message("preparing");
        let interactive = !multi.is_hidden();
        Self {
            multi,
            overall,
            interactive,
            total,
            finalized: false,
        }
    }

    pub fn fail(mut self) {
        self.finalized = true;
        self.overall.abandon_with_message("failed");
    }

    pub fn saved(&self, job_id: &JobId, path: &Path) {
        if !self.interactive {
            eprintln!(
                "{} job {} to {}",
                "Saved".green().bold(),
                job_id.as_str().cyan(),
                path.display().to_string().green()
            );
        }
    }

    pub fn finish(mut self) {
        self.finalized = true;
        self.overall.set_position(as_u64(self.total));
        self.overall.finish_with_message("complete");
    }

    pub fn download(&self, number: usize, total: Option<u64>) -> DownloadBar {
        if !self.interactive {
            return DownloadBar::hidden();
        }
        let bar = total.map_or_else(ProgressBar::new_spinner, ProgressBar::new);
        bar.set_prefix(format!("Request {number}/{}", self.total));
        if total.is_some() {
            bar.set_style(download_style());
        } else {
            bar.set_style(download_spinner_style());
            bar.enable_steady_tick(Duration::from_millis(100));
        }
        DownloadBar {
            bar: TransientBar::new(self.multi.add(bar)),
            determinate: total.is_some(),
        }
    }

    pub fn submitted(&self, number: usize, job_id: &JobId) {
        if !self.interactive {
            eprintln!(
                "{} {} submitted as job {}",
                "Request".bold(),
                format!("{number}/{}", self.total).cyan(),
                job_id.as_str().cyan()
            );
        }
    }

    pub fn submitting(&self, number: usize) {
        if self.interactive {
            self.overall
                .set_message(format!("submitting request {number}/{}", self.total));
        }
    }

    pub fn update_pipeline(&self, status: PipelineStatus) {
        self.overall.set_position(as_u64(status.completed));
        self.overall.set_message(status.message(self.total));
    }
}

impl Drop for ProgressDisplay {
    fn drop(&mut self) {
        if !self.finalized {
            self.overall.abandon_with_message("failed");
        }
    }
}

fn as_u64(value: usize) -> u64 {
    u64::try_from(value).expect("request count fits in u64")
}
fn overall_style() -> ProgressStyle {
    const TEMPLATE: &str =
        "{prefix:.bold} [{wide_bar:.green/blue}] {pos}/{len} {elapsed_precise} {wide_msg}";
    ProgressStyle::with_template(TEMPLATE)
        .expect("the overall progress template is valid")
        .progress_chars("=>-")
}

fn progress_target(mode: ProgressMode) -> ProgressDrawTarget {
    match mode {
        ProgressMode::Auto => ProgressDrawTarget::stderr(),
        ProgressMode::Always => {
            ProgressDrawTarget::term_like_with_hz(Box::new(Term::buffered_stderr()), 20)
        }
        ProgressMode::Never => ProgressDrawTarget::hidden(),
    }
}

fn download_style() -> ProgressStyle {
    const TEMPLATE: &str =
        "{prefix:.bold} [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} {bytes_per_sec} ETA {eta}";
    ProgressStyle::with_template(TEMPLATE)
        .expect("the download progress template is valid")
        .progress_chars("=>-")
}

fn download_spinner_style() -> ProgressStyle {
    const TEMPLATE: &str = "{prefix:.bold} {spinner:.cyan} {bytes} {bytes_per_sec}";
    ProgressStyle::with_template(TEMPLATE).expect("the download spinner template is valid")
}

fn planning_spinner_style() -> ProgressStyle {
    const TEMPLATE: &str = "{spinner:.cyan} {msg}";
    ProgressStyle::with_template(TEMPLATE)
        .expect("the planning spinner template is valid")
        .tick_strings(&[
            "●····",
            "·●···",
            "··●··",
            "···●·",
            "····●",
            "···●·",
            "··●··",
            "·●···",
            "·····",
        ])
}

pub async fn with_planning_spinner<T, F>(mode: ProgressMode, operation: impl FnOnce() -> F) -> T
where
    F: Future<Output = T>,
{
    let bar = ProgressBar::with_draw_target(None, progress_target(mode));
    bar.set_style(planning_spinner_style());
    bar.set_message("Building retrieval plan");
    if !bar.is_hidden() {
        bar.enable_steady_tick(Duration::from_millis(120));
    }
    let bar = TransientBar::new(bar);
    let result = operation().await;
    bar.finish();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planning_spinner_sweeps_in_both_directions_and_ends_empty() {
        let style = planning_spinner_style();
        let frames = [
            "●····",
            "·●···",
            "··●··",
            "···●·",
            "····●",
            "···●·",
            "··●··",
            "·●···",
        ];
        for (index, expected) in frames.into_iter().enumerate() {
            assert_eq!(style.get_tick_str(index as u64), expected);
        }
        assert_eq!(style.get_final_tick_str(), "·····");
    }

    #[test]
    fn pipeline_message_names_every_stage_unambiguously() {
        let status = PipelineStatus::new(1, Some(3), 2, 1, 1);

        assert_eq!(
            status.message(6),
            "completed: 1, submitting request 3/6, processing: 2, ready: 1, downloading: 1"
        );

        assert_eq!(
            PipelineStatus::new(1, None, 2, 0, 1).message(6),
            "completed: 1, processing: 2, downloading: 1"
        );
    }
}
