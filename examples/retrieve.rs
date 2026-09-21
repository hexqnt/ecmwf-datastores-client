use std::path::PathBuf;

use ecmwf_datastores_client::error::Result;
use ecmwf_datastores_client::{Client, CollectionId, Credentials, ExistingTarget, Selection};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let client = Client::from_credentials(Credentials::from_cdsapirc()?)?;

    let request = Selection::try_from(json!({
        "product_type": ["reanalysis"],
        "variable": ["2m_temperature"],
        "year": ["2023"],
        "month": ["01"],
        "day": ["01"],
        "time": ["00:00"],
        "data_format": "netcdf",
        "download_format": "unarchived"
    }))?;

    let target = PathBuf::from("era5-2m-temperature.nc");
    let saved = client
        .retrieve(
            &CollectionId::parse("reanalysis-era5-single-levels")?,
            &request,
            Some(target),
            ExistingTarget::Error,
        )
        .await?;
    println!("saved to {}", saved.display());
    Ok(())
}
