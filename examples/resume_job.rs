use std::env;

use ecmwf_datastores_client::error::Result;
use ecmwf_datastores_client::{Client, Credentials, ExistingTarget, JobId};

#[tokio::main]
async fn main() -> Result<()> {
    let job_id = env::args()
        .nth(1)
        .expect("pass a job id: cargo run --example resume_job <job-id> [target]");
    let target = env::args().nth(2);

    let client = Client::from_credentials(Credentials::from_cdsapirc()?)?;
    let job_id = JobId::parse(&job_id)?;
    let mut remote = client.job(&job_id)?;
    let results = remote.wait_for_results().await?;

    let saved = if let Some(path) = target {
        results.download_to(path, ExistingTarget::Error).await?
    } else {
        let suggested = results.suggested_filename();
        results
            .download_to(&suggested, ExistingTarget::Error)
            .await?
    };

    println!("downloaded {}", saved.display());
    Ok(())
}
