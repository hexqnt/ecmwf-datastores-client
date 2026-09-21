use ecmwf_datastores_client::error::Result;
use ecmwf_datastores_client::{Client, CollectionsRequest, Credentials};

#[tokio::main]
async fn main() -> Result<()> {
    let client = Client::from_credentials(Credentials::from_cdsapirc()?)?;

    if let Ok(v) = client.check_authentication().await {
        println!("Authenticated: {v:#?}");
    } else {
        println!("Not authenticated");
    }
    // Search for collections that mention temperature.
    let request = CollectionsRequest::default().with_query("temperature");
    let page = client.collections(&request).await?;
    println!(
        "found {} collections on the first page",
        page.collections().len()
    );
    for collection in page.collections() {
        let title = collection.title.as_deref().unwrap_or("<no title>");
        println!("- {} :: {}", collection.id, title);
    }

    // Fetch one entry in detail to access its metadata.
    if let Some(first) = page.collections().first() {
        let detailed = client.collection(&first.id).await?;
        println!("\n{} details:", detailed.id);
        println!("title: {}", detailed.title.as_deref().unwrap_or("<none>"));
        let keywords = if detailed.keywords.is_empty() {
            "n/a".to_string()
        } else {
            detailed.keywords.join(", ")
        };
        println!("keywords: {keywords}");
        println!(
            "updated at: {}",
            detailed
                .updated_at()
                .map_or_else(|| "unknown".to_string(), |ts| ts.to_rfc3339())
        );
    }

    Ok(())
}
