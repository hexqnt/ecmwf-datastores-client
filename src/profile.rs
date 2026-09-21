use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::{
    catalogue::Licence,
    error::Result,
    http::{HttpClient, RequestTemplate},
    id::{CollectionId, LicenceId},
    utils,
};

/// Client for profile- and account-related operations.
#[derive(Clone)]
pub struct ProfileApi {
    http: HttpClient,

    base_url: Url,
}

impl ProfileApi {
    /// Creates a profile API client rooted at the given base URL.
    pub fn new(mut base_url: Url, http: HttpClient) -> Self {
        utils::ensure_trailing_slash(&mut base_url);
        Self { http, base_url }
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.base_url.join(path).map_err(crate::error::Error::from)
    }

    /// Accepts a licence revision for a collection.
    pub async fn accept_licence(&self, licence_id: &LicenceId, revision: u32) -> Result<Value> {
        let url = self.endpoint(&format!("account/licences/{licence_id}"))?;
        let template =
            RequestTemplate::new(Method::PUT, url).with_json(&RevisionPayload { revision })?;
        self.http.execute_typed(&template).await
    }

    /// Adds a collection to the starred list.
    pub async fn star_collection(&self, collection_id: &CollectionId) -> Result<Vec<CollectionId>> {
        let url = self.endpoint("account/starred")?;
        let template = RequestTemplate::new(Method::POST, url)
            .with_json(&StarredPayload { uid: collection_id })?
            .with_log_messages(false);
        self.http.execute_typed(&template).await
    }

    /// Lists licences accepted by the user.
    pub async fn accepted_licences(&self, scope: Option<&str>) -> Result<Vec<Licence>> {
        let mut template = RequestTemplate::new(Method::GET, self.endpoint("account/licences")?);
        if let Some(scope) = scope {
            template = template.with_query_pair("scope", scope.to_string());
        }
        let payload: LicencePayload = self.http.execute_typed(&template).await?;
        Ok(payload.licences)
    }

    /// Removes a collection from the starred list.
    pub async fn unstar_collection(&self, collection_id: &CollectionId) -> Result<()> {
        let url = self.endpoint(&format!("account/starred/{collection_id}"))?;
        let template = RequestTemplate::new(Method::DELETE, url).with_log_messages(false);
        self.http.execute(&template).await?;
        Ok(())
    }

    /// Verifies the current credentials.
    pub async fn check_authentication(&self) -> Result<Value> {
        let url = self.endpoint("account/verification/pat")?;
        let template = RequestTemplate::new(Method::POST, url);
        self.http.execute_typed(&template).await
    }
}

#[derive(Debug, Deserialize)]
struct LicencePayload {
    #[serde(default)]
    licences: Vec<Licence>,
}

#[derive(Serialize)]
struct StarredPayload<'a> {
    uid: &'a CollectionId,
}
#[derive(Serialize)]
struct RevisionPayload {
    revision: u32,
}
