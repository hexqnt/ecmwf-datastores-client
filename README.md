# ecmwf-datastores-client

[![CI](https://github.com/hexqnt/ecmwf-datastores-client/actions/workflows/ci.yml/badge.svg)](https://github.com/hexqnt/ecmwf-datastores-client/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/ecmwf-datastores-client.svg)](https://crates.io/crates/ecmwf-datastores-client) [![docs.rs](https://docs.rs/ecmwf-datastores-client/badge.svg)](https://docs.rs/ecmwf-datastores-client)

[🇺🇸 English](./README.md) · [🇷🇺 Русский](./README.ru.md)

An unofficial asynchronous Rust client for the ECMWF Data Stores API (CDS, ADS, and EWDS). It supports catalogue queries, retrieval jobs, user profiles, and optional planning of large ERA5 requests.

## Installation

```sh
cargo add ecmwf-datastores-client
```

## Features

- Typed identifiers, job statuses, extents, and planning inputs.
- Job submission, polling, recovery by ID, and deletion.
- Catalogue search, collection forms and constraints, and pagination.
- In-memory, streaming, and file downloads.
- Local request planning refined with provider costs when available.

## Configuration

The client accepts an explicit endpoint and API key. The default `discovery` feature also loads credentials from `ECMWF_DATASTORES_URL`/`ECMWF_DATASTORES_KEY`, `~/.ecmwfdatastoresrc`, or `~/.cdsapirc`:

```rust
use ecmwf_datastores_client::{Client, Credentials, error::Result};

#[cfg(feature = "discovery")]
fn client() -> Result<Client> {
    Client::from_credentials(Credentials::discover()?)
}
```

Use `default-features = false` if discovery is not needed. `Credentials::from_file()` reads YAML containing `url` and an optional `key`.

## Example

```rust
use ecmwf_datastores_client::{
    Client, CollectionId, Credentials, ExistingTarget, Selection, error::Result,
};
use serde_json::json;

#[cfg(feature = "discovery")]
async fn download_example() -> Result<()> {
    let client = Client::from_credentials(Credentials::from_cdsapirc()?)?;
    let collection = CollectionId::parse("reanalysis-era5-single-levels")?;
    let selection = Selection::try_from(json!({
        "product_type": ["reanalysis"],
        "variable": ["2m_temperature"],
        "year": ["2023"],
        "month": ["01"],
        "day": ["01"],
        "time": ["00:00"],
        "data_format": "netcdf"
    }))?;

    let job = client.submit(&collection, &selection).await?;
    let job_id = job.id().clone();
    let results = client.job(&job_id)?.wait_for_results().await?;
    results.asset().download_to("era5.nc", ExistingTarget::Error).await?;
    Ok(())
}
```

Store `job_id.to_string()` to resume a job after a restart. `wait_for_results()` waits for completion; `status()` performs one request. Jobs can be removed with `Job::delete()` or `Client::delete_jobs()`.

Useful APIs include:

- `Asset::bytes()`, `byte_stream()`, and `download_to()` for results.
- `Job::details()` for typed job metadata and `Client::receipt_details()` for typed receipts.
- `Client::collection_form()` and `collection_constraints()` for request fields.
- `Client::builder().wait_timeout()` to change the default 24-hour wait limit.

## Request planning

The `planning` module splits large ERA5 requests locally. `Planner::plan()` refines the split through the provider costing endpoint when available, but never submits jobs. Use `plan_locally()` for an I/O-free plan or `refine_with_costs()` to refine an existing one.

```rust
use ecmwf_datastores_client::{Client, planning::{Planner, PlanningRequest}};

async fn planning_example(
    client: &Client,
    request: &PlanningRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let outcome = Planner::new().plan(client, request).await?;
    for part in outcome.plan().requests() {
        client.submit(outcome.plan().dataset(), part.selection()).await?;
    }
    Ok(())
}
```

## Notes

- `Selection` is a JSON object; dataset-specific fields are checked by the service.
- A file download resumes within the same `download_to()` call when the server supports it. Cancellation removes the temporary file.
- Open Data API, IFS, and AIFS are outside this crate's scope.

For TOML jobs, progress reporting, multipart assembly, and importing CDS-generated Python snippets, use the separate [`ecmwf-datastores-cli`](https://github.com/hexqnt/ecmwf-datastores-client/tree/main/crates/ecmwf-datastores-cli) package.

## Development

See [performance measurements](perf/README.md) for the benchmark catalogue,
baseline comparisons, flamegraphs, and Linux `perf` commands.
