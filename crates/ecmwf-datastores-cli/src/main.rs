use std::{
    collections::HashSet,
    fs,
    io::Write as _,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use colored::Colorize as _;
use ecmwf_datastores_cli::{
    assembly,
    config::RetrievalSpec,
    plan::{PlanSummary, PlannedRequest, build_adaptive_plan, build_plan},
    python::import_api_code,
};
use ecmwf_datastores_client::planning::CostingOutcome;
use ecmwf_datastores_client::{Client, ClientBuilder, Credentials, ExistingTarget, JobId};
use tempfile::NamedTempFile;

use progress::{ProgressMode, with_planning_spinner};

mod execution;
mod progress;

#[derive(Debug, Subcommand)]
enum Command {
    /// Expand a configuration, using provider costing when available.
    Plan {
        /// Emit the complete plan as JSON.
        #[arg(long)]
        json: bool,

        /// TOML retrieval configuration.
        config: PathBuf,

        #[command(flatten)]
        connection: Connection,
    },

    /// Continue waiting for an existing job and download its result.
    Resume {
        /// Identifier of a previously submitted job.
        job_id: JobId,

        /// File or directory to which the result is downloaded.
        output: PathBuf,

        /// Replace an existing output file.
        #[arg(long)]
        overwrite: bool,

        #[command(flatten)]
        connection: Connection,
    },

    /// Retrieve all planned requests through a bounded pipeline.
    Retrieve {
        /// TOML retrieval configuration.
        config: PathBuf,

        /// Write machine-readable timing and job information to this file.
        #[arg(long)]
        report: Option<PathBuf>,

        /// Allow completed files to replace existing targets.
        #[arg(long)]
        overwrite: bool,

        #[command(flatten)]
        connection: Connection,
    },

    /// Assemble already downloaded multipart results.
    Assemble {
        /// Downloaded parts in plan order.
        #[arg(required = true, num_args = 2..)]
        parts: Vec<PathBuf>,

        /// Final assembled file.
        output: PathBuf,

        /// Replace an existing output file.
        #[arg(long)]
        overwrite: bool,
    },

    /// Query server-side allowed values for every planned request.
    Constraints {
        /// TOML retrieval configuration.
        config: PathBuf,

        #[command(flatten)]
        connection: Connection,
    },

    /// Convert code copied from “Show API request code” into TOML.
    ImportPython {
        /// Python source copied from the CDS request form.
        source: PathBuf,

        /// Write to a file instead of standard output.
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Replace an existing output file.
        #[arg(long)]
        overwrite: bool,
    },
}

#[derive(Debug, Parser)]
#[command(version, about = "Retrieve and partition ECMWF Data Stores requests")]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Control interactive progress rendering on standard error.
    #[arg(long, value_enum, default_value_t, global = true)]
    progress: ProgressMode,
}

#[derive(Debug, Args)]
struct Connection {
    /// Credentials file; discovery is used when omitted.
    #[arg(long)]
    credentials: Option<PathBuf>,
}

impl Connection {
    fn client(&self) -> Result<Client> {
        let credentials = self.credentials.as_ref().map_or_else(
            || Credentials::discover().context("failed to discover credentials"),
            |path| {
                Credentials::from_file(path)
                    .with_context(|| format!("failed to load credentials from {}", path.display()))
            },
        )?;
        Client::from_credentials(credentials).context("failed to build API client")
    }

    fn costing_client(&self) -> Result<Client> {
        let credentials = match self.credentials.as_ref() {
            Some(path) => Credentials::from_file(path)
                .with_context(|| format!("failed to load credentials from {}", path.display()))?,
            None => Credentials::discover().unwrap_or(Credentials::new(
                "https://cds.climate.copernicus.eu/api/".parse()?,
                None,
            )),
        };
        ClientBuilder::from_credentials(credentials)
            .request_timeout(Duration::from_secs(10))
            .build()
            .context("failed to build API client for provider costing")
    }
}

fn write_atomic(path: &Path, contents: &str, overwrite: bool) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create directory {}", parent.display()))?;
    let mut temporary = NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create a temporary file in {}", parent.display()))?;
    temporary
        .write_all(contents.as_bytes())
        .and_then(|()| temporary.as_file_mut().sync_all())
        .with_context(|| format!("failed to write temporary file for {}", path.display()))?;
    let result = if overwrite {
        temporary.persist(path)
    } else {
        temporary.persist_noclobber(path)
    };
    result
        .map(|_| ())
        .map_err(|error| error.error)
        .with_context(|| format!("failed to persist {}", path.display()))
}

fn resolve_target(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut existing = absolute.as_path();
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(existing) {
            Ok(mut canonical) => {
                for component in suffix.iter().rev() {
                    match component {
                        Component::ParentDir => {
                            canonical.pop();
                        }
                        Component::CurDir => {}
                        other => canonical.push(other.as_os_str()),
                    }
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .components()
                    .next_back()
                    .context("target path has no existing ancestor")?;
                suffix.push(name);
                existing = existing.parent().context("target path has no parent")?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to resolve {}", path.display()));
            }
        }
    }
}

fn existing_target(overwrite: bool) -> ExistingTarget {
    if overwrite {
        ExistingTarget::Replace
    } else {
        ExistingTarget::Error
    }
}

fn display_estimate(estimate: Option<usize>) -> String {
    estimate.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

fn print_welcome() {
    eprintln!(
        "{} {}",
        "ECMWF Data Stores CLI".bold(),
        env!("CARGO_PKG_VERSION").cyan()
    );
}

pub(crate) fn print_retrieval_plan(spec: &RetrievalSpec, plan: &[PlannedRequest]) {
    let summary = PlanSummary::new(spec, plan);
    eprintln!(
        "{} {}\n{} {} {} ({})\n{}",
        "Dataset:".bold(),
        summary.dataset.as_str().cyan(),
        "Work split:".bold(),
        summary.requests.len().to_string().cyan(),
        if summary.requests.len() == 1 {
            "request"
        } else {
            "requests"
        },
        format!(
            "{} estimated items total",
            display_estimate(summary.estimated_items)
        )
        .dimmed(),
        "Parts:".bold()
    );
    for request in summary.requests {
        let provider_cost = request.provider_cost.map_or_else(String::new, |cost| {
            format!(", provider cost {:.0}/{:.0}", cost.cost(), cost.limit())
        });
        eprintln!(
            "  {}/{}: {} estimated items{} {} {}",
            request.number.to_string().cyan(),
            plan.len(),
            display_estimate(request.estimated_items),
            provider_cost.dimmed(),
            "->".dimmed(),
            request.output.display().to_string().green()
        );
    }
    eprintln!(
        "{} {}",
        "Final output:".bold(),
        summary.output.display().to_string().green()
    );
}

fn ensure_distinct_targets<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Result<()> {
    let mut unique = HashSet::new();
    for path in paths {
        if !unique.insert(resolve_target(path)?) {
            bail!("target {} is used more than once", path.display());
        }
    }
    Ok(())
}

fn ensure_targets_available<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
    overwrite: bool,
) -> Result<()> {
    for path in paths {
        if path.is_dir() {
            bail!("target {} is a directory; expected a file", path.display());
        }
        if !overwrite && path.exists() {
            bail!(
                "target {} already exists; use --overwrite to replace it",
                path.display()
            );
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();
    print_welcome();
    match cli.command {
        Command::Plan {
            config,
            connection,
            json,
        } => plan_command(&config, &connection, json, cli.progress).await,
        Command::Constraints { config, connection } => {
            constraints_command(&config, &connection, cli.progress).await
        }
        Command::Retrieve {
            config,
            connection,
            overwrite,
            report,
        } => {
            execution::retrieve_command(
                &config,
                &connection,
                overwrite,
                report.as_deref(),
                cli.progress,
            )
            .await
        }
        Command::Resume {
            job_id,
            output,
            connection,
            overwrite,
        } => resume_command(&job_id, output, &connection, overwrite, cli.progress).await,
        Command::Assemble {
            output,
            parts,
            overwrite,
        } => assemble_command(parts, output, overwrite).await,
        Command::ImportPython {
            source,
            output,
            overwrite,
        } => import_command(&source, output.as_deref(), overwrite).await,
    }
}

async fn plan_command(
    path: &Path,
    connection: &Connection,
    json: bool,
    progress_mode: ProgressMode,
) -> Result<()> {
    let spec = RetrievalSpec::from_path(path)?;
    let plan = match connection.costing_client() {
        Ok(client) => adaptive_plan(&spec, &client, progress_mode).await?,
        Err(error) => {
            eprintln!(
                "warning: provider costing is unavailable ({error:#}); using offline planning"
            );
            with_planning_spinner(progress_mode, || async { build_plan(&spec) }).await?
        }
    };
    let summary = PlanSummary::new(&spec, &plan);
    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!(
            "{} {}\n{} {}\n{} {}",
            "Dataset:".bold(),
            summary.dataset.as_str().cyan(),
            "Requests:".bold(),
            summary.requests.len().to_string().cyan(),
            "Estimated items:".bold(),
            display_estimate(summary.estimated_items).cyan()
        );
        println!(
            "{} {}",
            "Output:".bold(),
            summary.output.display().to_string().green()
        );
        for request in summary.requests {
            let cost = request.provider_cost.map_or_else(String::new, |cost| {
                format!(" cost {:.0}/{:.0}", cost.cost(), cost.limit())
            });
            println!(
                "  {}: {:>8} {}{} {} {}",
                format!("{:04}", request.number).cyan(),
                display_estimate(request.estimated_items),
                "items".dimmed(),
                cost.dimmed(),
                "->".dimmed(),
                request.output.display().to_string().green()
            );
        }
    }
    Ok(())
}

pub(crate) async fn adaptive_plan(
    spec: &RetrievalSpec,
    client: &Client,
    progress_mode: ProgressMode,
) -> Result<Vec<PlannedRequest>> {
    let result = with_planning_spinner(progress_mode, || build_adaptive_plan(spec, client)).await;
    let (plan, costing) = result?;
    if let CostingOutcome::LocalFallback { reason } = costing {
        eprintln!(
            "warning: provider costing is unavailable ({reason}); using offline planning for {}",
            spec.dataset()
        );
    }
    Ok(plan)
}

async fn resume_command(
    job_id: &JobId,
    output: PathBuf,
    connection: &Connection,
    overwrite: bool,
    progress_mode: ProgressMode,
) -> Result<()> {
    ensure_targets_available([output.as_path()], overwrite)?;
    let client = connection.client()?;
    let mut job = client.job(job_id)?;
    let progress = progress::ProgressDisplay::new(1, progress_mode);
    progress.update_pipeline(progress::PipelineStatus::new(0, None, 1, 0, 0));
    let result = async {
        let results = job.wait_for_results().await?;
        progress.update_pipeline(progress::PipelineStatus::new(0, None, 0, 0, 1));
        let mut download = progress.download(1, results.asset().file_size());
        let result = results
            .download_to_with_progress(output, existing_target(overwrite), |update| {
                download.update(update);
            })
            .await;
        download.finish();
        result
    }
    .await;
    let saved = match result {
        Ok(saved) => saved,
        Err(error) => {
            progress.fail();
            return Err(error).with_context(|| format!("failed to resume job {job_id}"));
        }
    };
    progress.finish();
    println!(
        "{} job {} to {}",
        "Saved".green().bold(),
        job_id.as_str().cyan(),
        saved.display().to_string().green()
    );
    Ok(())
}

async fn import_command(source: &Path, output: Option<&Path>, overwrite: bool) -> Result<()> {
    let source_text = tokio::fs::read_to_string(source)
        .await
        .with_context(|| format!("failed to read {}", source.display()))?;
    let imported = import_api_code(&source_text)?;
    if let Some(output) = output {
        write_atomic_async(output, imported, overwrite).await?;
        println!(
            "{} configuration to {}",
            "Wrote".green().bold(),
            output.display().to_string().green()
        );
    } else {
        print!("{imported}");
    }
    Ok(())
}

async fn assemble_command(parts: Vec<PathBuf>, output: PathBuf, overwrite: bool) -> Result<()> {
    ensure_distinct_targets(
        parts
            .iter()
            .map(PathBuf::as_path)
            .chain(std::iter::once(output.as_path())),
    )?;
    ensure_targets_available([output.as_path()], overwrite)?;
    let saved = assembly::assemble(parts, output, overwrite).await?;
    println!(
        "{} results to {}",
        "Saved".green().bold(),
        saved.display().to_string().green()
    );
    Ok(())
}

async fn write_atomic_async(path: &Path, contents: String, overwrite: bool) -> Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || write_atomic(&path, &contents, overwrite)).await??;
    Ok(())
}

async fn constraints_command(
    path: &Path,
    connection: &Connection,
    progress_mode: ProgressMode,
) -> Result<()> {
    let spec = RetrievalSpec::from_path(path)?;
    let client = connection.client()?;
    let plan = adaptive_plan(&spec, &client, progress_mode).await?;
    for (index, request) in plan.iter().enumerate() {
        let allowed = client
            .apply_constraints(spec.dataset(), request.selection())
            .await
            .with_context(|| format!("failed to query constraints for request {}", index + 1))?;
        println!(
            "{} {}: {}",
            "Request".bold(),
            format!("{}/{}", index + 1, plan.len()).cyan(),
            serde_json::to_string_pretty(&allowed)?
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_does_not_replace_without_permission() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.json");
        fs::write(&path, "original").unwrap();

        assert!(write_atomic(&path, "replacement", false).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "original");
    }

    #[test]
    fn duplicate_targets_are_rejected() {
        let path = Path::new("same.nc");
        let error = ensure_distinct_targets([path, path]).unwrap_err();
        assert!(error.to_string().contains("used more than once"));
    }

    #[test]
    fn absolute_and_relative_targets_are_rejected() {
        let relative = Path::new("result.nc");
        let absolute = std::env::current_dir().unwrap().join(relative);
        assert!(ensure_distinct_targets([relative, absolute.as_path()]).is_err());
    }

    #[test]
    fn normalizes_nonexistent_targets_through_existing_parent() {
        let directory = tempfile::tempdir().unwrap();
        let direct = directory.path().join("result.nc");
        let aliased = directory.path().join("missing/../result.nc");
        assert!(ensure_distinct_targets([direct.as_path(), aliased.as_path()]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn resolves_symlinked_parent_targets() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let link = directory.path().join("link");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let direct = real.join("result.nc");
        let aliased = link.join("result.nc");
        assert!(ensure_distinct_targets([direct.as_path(), aliased.as_path()]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn resolves_existing_file_symlink_targets() {
        let directory = tempfile::tempdir().unwrap();
        let direct = directory.path().join("result.nc");
        let aliased = directory.path().join("report.json");
        fs::write(&direct, "data").unwrap();
        std::os::unix::fs::symlink(&direct, &aliased).unwrap();
        assert!(ensure_distinct_targets([direct.as_path(), aliased.as_path()]).is_err());
    }
}
