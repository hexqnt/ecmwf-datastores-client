use ecmwf_datastores_client::error::Result;
use ecmwf_datastores_client::{Client, CollectionId, Credentials, Selection};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let client = Client::from_credentials(Credentials::from_cdsapirc()?)?;
    let collection = CollectionId::parse("reanalysis-era5-land")?;
    let selection = Selection::try_from(json!({
        "variable": ["2m_temperature"],
        "year": ["2022"],
        "month": ["02"],
        "day": ["01"],
        "time": ["00:00"],
        "data_format": "netcdf",
        "download_format": "unarchived"
    }))?;
    let job = client.submit(&collection, &selection).await?;
    println!("job queued: {}", job.id());
    Ok(())
}
