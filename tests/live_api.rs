use std::{env, error::Error as StdError, io, num::NonZeroU32, time::Duration};

use ecmwf_datastores_client::{
    Client, CollectionId, CollectionsRequest, Credentials, ExistingTarget, JobId, JobStatus,
    Selection,
};
use serde_json::{Value, json};

const ERA5_SINGLE_LEVELS: &str = "reanalysis-era5-single-levels";
// `Duration::from_mins` is newer than the crate's MSRV (Rust 1.88).
#[allow(clippy::duration_suboptimal_units)]
const JOB_TIMEOUT: Duration = Duration::from_secs(15 * 60);

type TestResult<T = ()> = Result<T, Box<dyn StdError>>;

fn require(condition: bool, message: &'static str) -> TestResult {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message).into())
    }
}

fn era5_request() -> TestResult<Selection> {
    Ok(Selection::try_from(json!({
        "product_type": ["reanalysis"],
        "variable": ["2m_temperature"],
        "year": ["2023"],
        "month": ["01"],
        "day": ["01"],
        "time": ["00:00"],
        "area": [51.0, -1.0, 50.0, 0.0],
        "data_format": "grib",
        "download_format": "unarchived"
    }))?)
}

fn live_client(credentials: &Credentials) -> TestResult<Client> {
    Ok(Client::builder(credentials.endpoint().clone())
        .api_key(
            credentials
                .api_key()
                .cloned()
                .ok_or_else(|| io::Error::other("live tests require an API key"))?,
        )
        .poll_max_delay(Duration::from_secs(10))
        .build()?)
}

fn live_credentials() -> TestResult<Credentials> {
    let path = env::var_os("ECMWF_DATASTORES_TEST_CREDENTIALS").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "ECMWF_DATASTORES_TEST_CREDENTIALS must point to a credentials file",
        )
    })?;
    Ok(Credentials::from_file(path)?)
}

async fn exercise_retrieval(
    credentials: &Credentials,
    collection_id: &CollectionId,
    job_id: &JobId,
) -> TestResult {
    // Construct a fresh client to prove that a persisted JobId is sufficient
    // to resume a submission after a process restart.
    let resumed_client = live_client(credentials)?;
    let mut resumed = resumed_client.job(job_id)?;
    let results = tokio::time::timeout(JOB_TIMEOUT, resumed.wait_for_results())
        .await
        .map_err(|_| io::Error::other("timed out waiting for the live job"))??;

    let details = resumed.details().await?;
    require(details.job_id == *job_id, "job details returned another id")?;
    require(
        details.process_id == *collection_id,
        "job details returned another process id",
    )?;
    require(
        details.status == JobStatus::Successful,
        "completed job is not successful",
    )?;
    require(
        details
            .request()
            .and_then(|request| request.as_map().get("year"))
            == Some(&json!(["2023"])),
        "job details do not contain the submitted year",
    )?;

    let receipt = resumed_client.receipt(job_id).await?;
    require(
        matches!(receipt, Value::Object(ref object) if !object.is_empty()),
        "receipt must be a non-empty JSON object",
    )?;

    let directory = tempfile::tempdir()?;
    let target = directory.path().join("era5.grib");
    let saved = results.download_to(&target, ExistingTarget::Error).await?;
    require(saved == target, "download returned an unexpected path")?;
    require(
        tokio::fs::metadata(saved).await?.len() > 0,
        "downloaded asset is empty",
    )
}
#[tokio::test]
#[ignore = "submits, downloads, and deletes a live ECMWF Data Stores job"]
async fn cds_retrieve_contract() -> TestResult {
    let credentials = live_credentials()?;
    let client = live_client(&credentials)?;
    let collection_id = CollectionId::parse(ERA5_SINGLE_LEVELS)?;
    let request = era5_request()?;
    let submitted = client.submit(&collection_id, &request).await?;
    let job_id = submitted.id().clone();

    let exercise_result = exercise_retrieval(&credentials, &collection_id, &job_id).await;
    let cleanup_result = submitted.delete().await;

    match (exercise_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(exercise), Err(cleanup)) => Err(io::Error::other(format!(
            "live retrieval failed: {exercise}; job cleanup also failed: {cleanup}"
        ))
        .into()),
    }
}

#[tokio::test]
#[ignore = "requires live ECMWF Data Stores credentials and network access"]
async fn cds_read_only_contract() -> TestResult {
    let credentials = live_credentials()?;
    let client = live_client(&credentials)?;
    let collection_id = CollectionId::parse(ERA5_SINGLE_LEVELS)?;
    let request = era5_request()?;

    let authentication = client.check_authentication().await?;
    require(
        !authentication.is_null(),
        "authentication response must not be null",
    )?;

    let collection = client.collection(&collection_id).await?;
    require(
        collection.id == collection_id,
        "catalogue returned a different collection id",
    )?;
    require(
        !client.collection_form(&collection_id).await?.is_empty(),
        "catalogue returned an empty collection form",
    )?;
    require(
        !client
            .collection_constraints(&collection_id)
            .await?
            .is_empty(),
        "catalogue returned no collection constraints",
    )?;
    let page = client
        .collections(&CollectionsRequest {
            limit: Some(NonZeroU32::new(1).unwrap()),
            search_stats: true,
            ..CollectionsRequest::default()
        })
        .await?;
    require(
        page.number_matched().is_some() && page.search_stats().is_some(),
        "catalogue returned no search statistics",
    )?;

    let process = client.process(&collection_id).await?;
    require(
        process.id == collection_id,
        "processing API returned a different process id",
    )?;

    let constraints = client.apply_constraints(&collection_id, &request).await?;
    require(
        constraints.is_object(),
        "constraints response must be a JSON object",
    )
}
